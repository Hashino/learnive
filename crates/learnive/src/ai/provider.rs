//! Provedor de IA como seam trocável (§12, §14 — "knob roteado por sub-tarefa e
//! trocável, não dependência").
//!
//! OpenRouter (default, §12), OpenAI/OpenCode Zen diretos e a maioria dos
//! provedores compatíveis falam o mesmo formato `chat/completions` da OpenAI —
//! então um único `OpenAiCompat` cobre todos variando `base_url` + auth. O
//! `Mock` permite rodar o loop sem chave e testar sem rede.
//!
//! Consumido pelo loop (Fase 1, Task #5); daí o `allow(dead_code)` temporário.
#![allow(dead_code)]

use std::pin::Pin;

use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

/// Stream de deltas de texto (§14 — streaming token-a-token para TTFT baixo).
pub type TokenStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>;

#[derive(Debug)]
pub enum ProviderError {
    /// Falha de transporte HTTP.
    Http(String),
    /// A API respondeu com status de erro.
    Api { status: u16, body: String },
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::Http(e) => write!(f, "erro HTTP: {e}"),
            ProviderError::Api { status, body } => write!(f, "erro da API ({status}): {body}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// Papel de uma mensagem no formato de chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// Uma mensagem de chat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// Uma requisição de completion.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub temperature: Option<f32>,
}

/// Enum de provedores (dispatch por enum evita a não-object-safety de async fn
/// em trait; adicionar um provedor = nova variante).
pub enum Provider {
    OpenAiCompat(OpenAiCompat),
    Mock(MockProvider),
}

impl Provider {
    pub async fn stream(&self, req: ChatRequest) -> Result<TokenStream, ProviderError> {
        match self {
            Provider::OpenAiCompat(p) => p.stream(req).await,
            Provider::Mock(m) => Ok(m.stream(req)),
        }
    }
}

/// Cliente para qualquer endpoint compatível com OpenAI `chat/completions`.
pub struct OpenAiCompat {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiCompat {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key,
        }
    }

    /// OpenRouter — o caminho default (§12).
    pub fn openrouter(api_key: Option<String>) -> Self {
        Self::new("https://openrouter.ai/api/v1", api_key)
    }

    async fn stream(&self, req: ChatRequest) -> Result<TokenStream, ProviderError> {
        #[derive(Serialize)]
        struct Body<'a> {
            model: &'a str,
            messages: &'a [ChatMessage],
            stream: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            temperature: Option<f32>,
        }

        let mut builder = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .json(&Body {
                model: &req.model,
                messages: &req.messages,
                stream: true,
                temperature: req.temperature,
            });
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Api { status, body });
        }

        let byte_stream = resp.bytes_stream();
        let stream = async_stream::stream! {
            futures_util::pin_mut!(byte_stream);
            let mut buf = String::new();
            while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(ProviderError::Http(e.to_string()));
                        return;
                    }
                };
                buf.push_str(&String::from_utf8_lossy(&chunk));
                // Processa cada linha completa (a API delimita eventos SSE por linha).
                while let Some(pos) = buf.find('\n') {
                    let line: String = buf.drain(..=pos).collect();
                    match parse_sse_line(line.trim_end()) {
                        SseEvent::Done => return,
                        SseEvent::Delta(text) if !text.is_empty() => yield Ok(text),
                        _ => {}
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

/// Provedor falso: streama uma resposta token-a-token (palavra a palavra) —
/// para rodar o loop sem chave e para testes sem rede. A resposta pode ser
/// constante (`new`) ou decidida a partir da requisição (`scripted`), o que
/// permite um modo demo offline que responde diferente por sub-tarefa.
pub struct MockProvider {
    responder: Box<dyn Fn(&ChatRequest) -> String + Send + Sync>,
}

impl MockProvider {
    /// Sempre responde a mesma string.
    pub fn new(reply: impl Into<String>) -> Self {
        let reply = reply.into();
        Self {
            responder: Box::new(move |_| reply.clone()),
        }
    }

    /// Decide a resposta a partir da requisição (ex.: por palavra-chave do
    /// prompt), para simular o loop inteiro offline.
    pub fn scripted<F>(f: F) -> Self
    where
        F: Fn(&ChatRequest) -> String + Send + Sync + 'static,
    {
        Self {
            responder: Box::new(f),
        }
    }

    fn stream(&self, req: ChatRequest) -> TokenStream {
        let reply = (self.responder)(&req);
        let tokens: Vec<String> = reply.split_inclusive(' ').map(|s| s.to_string()).collect();
        let stream = async_stream::stream! {
            for token in tokens {
                yield Ok(token);
            }
        };
        Box::pin(stream)
    }
}

/// Um evento de linha SSE do provedor.
enum SseEvent {
    Delta(String),
    Done,
    Ignore,
}

/// Faz o parse de uma linha `data: {...}` do stream. Pura e testável.
fn parse_sse_line(line: &str) -> SseEvent {
    let Some(data) = line.strip_prefix("data:") else {
        return SseEvent::Ignore;
    };
    let data = data.trim();
    if data == "[DONE]" {
        return SseEvent::Done;
    }
    if data.is_empty() {
        return SseEvent::Ignore;
    }

    #[derive(Deserialize)]
    struct Chunk {
        choices: Vec<Choice>,
    }
    #[derive(Deserialize)]
    struct Choice {
        delta: Delta,
    }
    #[derive(Deserialize)]
    struct Delta {
        content: Option<String>,
    }

    match serde_json::from_str::<Chunk>(data) {
        Ok(chunk) => SseEvent::Delta(
            chunk
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.delta.content)
                .unwrap_or_default(),
        ),
        Err(_) => SseEvent::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_delta_line() {
        let line = r#"data: {"choices":[{"delta":{"content":"olá"}}]}"#;
        match parse_sse_line(line) {
            SseEvent::Delta(t) => assert_eq!(t, "olá"),
            _ => panic!("esperava delta"),
        }
    }

    #[test]
    fn parses_done_and_ignores_noise() {
        assert!(matches!(parse_sse_line("data: [DONE]"), SseEvent::Done));
        assert!(matches!(parse_sse_line(": keep-alive"), SseEvent::Ignore));
        assert!(matches!(parse_sse_line("data:"), SseEvent::Ignore));
        assert!(matches!(parse_sse_line("data: not-json"), SseEvent::Ignore));
    }

    #[tokio::test]
    async fn mock_streams_tokens_that_reassemble() {
        let provider = Provider::Mock(MockProvider::new("um dois três"));
        let stream = provider
            .stream(ChatRequest {
                model: "mock".to_string(),
                messages: vec![ChatMessage::user("oi")],
                temperature: None,
            })
            .await
            .unwrap();

        let tokens: Vec<String> = stream.map(|r| r.unwrap()).collect().await;
        assert!(tokens.len() > 1, "deve streamar em múltiplos tokens");
        assert_eq!(tokens.concat(), "um dois três");
    }
}
