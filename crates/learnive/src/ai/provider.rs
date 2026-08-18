//! AI provider as a swappable seam (§12, §14 — "a knob routed per sub-task and
//! swappable, not a dependency").
//!
//! OpenRouter (default, §12), direct OpenAI/OpenCode Zen, and most compatible
//! providers speak the same OpenAI `chat/completions` format — so a single
//! `OpenAiCompat` covers all of them by varying `base_url` + auth. The `Mock`
//! lets the loop run without a key and be tested without a network.
//!
//! Consumed by the loop (Phase 1, Task #5); hence the temporary `allow(dead_code)`.
#![allow(dead_code)]

use std::pin::Pin;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

/// Bounds a single body read once response headers have already arrived.
/// `reqwest::ClientBuilder::timeout` only wraps the future returned by
/// `.send()` (connect through response *headers*) — confirmed live
/// (2026-08-18) against a stuck node: the client already carried a 120s
/// timeout, yet a provider that accepted the connection, returned 200, then
/// stalled mid-body hung well past that. `.text()`/`.json()`/the streamed
/// body's chunks are all read in a separate await afterward, unguarded by
/// that client-level setting. 45s comfortably covers the idle gap between
/// two chunks of a healthy stream (documented TTFT ~1-2s) or one full
/// non-streamed completion; used per-chunk below rather than once for the
/// whole body, since a long legitimate streamed prose move can run well
/// past 45s in total even though no single gap should.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(45);

/// Stream of text deltas (§14 — token-by-token streaming for low TTFT).
pub type TokenStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>;

#[derive(Debug)]
pub enum ProviderError {
    /// HTTP transport failure.
    Http(String),
    /// The API responded with an error status.
    Api { status: u16, body: String },
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::Http(e) => write!(f, "HTTP error: {e}"),
            ProviderError::Api { status, body } => write!(f, "API error ({status}): {body}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// Role of a message in the chat format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// A chat message.
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

/// A completion request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub temperature: Option<f32>,
}

/// Provider enum (enum dispatch avoids the non-object-safety of `async fn` in a
/// trait; adding a provider = a new variant).
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

    /// Non-streamed completion — for callers that buffer the whole response
    /// before use anyway (`engine::collect`, §14: streaming exists for TTFT
    /// on moves rendered token-by-token to the reader; a structured/JSON-only
    /// call like an outline or a rubric proposal is never shown live, so
    /// asking the provider to stream it bought nothing and cost reliability
    /// — confirmed live against a reasoning-heavy model that routed its
    /// entire output through the streaming `reasoning` delta and only
    /// sometimes reached a final `content` chunk at all).
    pub async fn complete(&self, req: ChatRequest) -> Result<String, ProviderError> {
        match self {
            Provider::OpenAiCompat(p) => p.complete(req).await,
            Provider::Mock(m) => Ok(m.complete(req)),
        }
    }
}

/// Client for any endpoint compatible with OpenAI `chat/completions`.
pub struct OpenAiCompat {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiCompat {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            // Live report (2026-08-17, `hidc0ayawb`): the free-tier provider
            // this app defaults new users toward is documented (`.env.example`)
            // to hang for minutes under load with no error — an unbounded
            // client left `generate_node`'s SSE loop stuck forever, which is
            // what put a node on disk with content but no `NodeGenerated`
            // event in the first place. 120s comfortably covers a full
            // streamed prose move (TTFT is ~1-2s on a healthy model); a short
            // connect timeout fails fast on a dead endpoint instead of
            // queueing behind the same 120s.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client builder with static config"),
            base_url: base_url.into(),
            api_key,
        }
    }

    /// OpenRouter — the default path (§12).
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
            let body = tokio::time::timeout(BODY_READ_TIMEOUT, resp.text())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            return Err(ProviderError::Api { status, body });
        }

        let byte_stream = resp.bytes_stream();
        let stream = async_stream::stream! {
            futures_util::pin_mut!(byte_stream);
            let mut buf = String::new();
            loop {
                let next = match tokio::time::timeout(BODY_READ_TIMEOUT, byte_stream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        yield Err(ProviderError::Http(format!(
                            "stream stalled: no data for {BODY_READ_TIMEOUT:?}"
                        )));
                        return;
                    }
                };
                let Some(chunk) = next else { break };
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(ProviderError::Http(e.to_string()));
                        return;
                    }
                };
                buf.push_str(&String::from_utf8_lossy(&chunk));
                // Process each complete line (the API delimits SSE events by line).
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

    /// Non-streamed `chat/completions` (`stream: false`) — the provider
    /// returns the whole message in one JSON object instead of SSE deltas.
    /// For a reasoning model this also gets a clean split (the provider's
    /// own `message.reasoning` vs. `message.content`) that the streaming
    /// path doesn't reliably deliver for every request.
    async fn complete(&self, req: ChatRequest) -> Result<String, ProviderError> {
        #[derive(Serialize)]
        struct Body<'a> {
            model: &'a str,
            messages: &'a [ChatMessage],
            stream: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            temperature: Option<f32>,
        }
        #[derive(Deserialize)]
        struct Resp {
            choices: Vec<Choice>,
        }
        #[derive(Deserialize)]
        struct Choice {
            message: Msg,
        }
        #[derive(Deserialize)]
        struct Msg {
            content: Option<String>,
        }

        let mut builder = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .json(&Body {
                model: &req.model,
                messages: &req.messages,
                stream: false,
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
            let body = tokio::time::timeout(BODY_READ_TIMEOUT, resp.text())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            return Err(ProviderError::Api { status, body });
        }

        let body: Resp = tokio::time::timeout(BODY_READ_TIMEOUT, resp.json())
            .await
            .map_err(|_| {
                ProviderError::Http(format!(
                    "reading response body stalled for {BODY_READ_TIMEOUT:?}"
                ))
            })?
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        Ok(body
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default())
    }
}

/// Fake provider: streams a response token by token (word by word) — to run the
/// loop without a key and for network-free tests. The response can be constant
/// (`new`) or decided from the request (`scripted`), which enables an offline
/// demo mode that answers differently per sub-task.
pub struct MockProvider {
    responder: Box<dyn Fn(&ChatRequest) -> String + Send + Sync>,
}

impl MockProvider {
    /// Always responds with the same string.
    pub fn new(reply: impl Into<String>) -> Self {
        let reply = reply.into();
        Self {
            responder: Box::new(move |_| reply.clone()),
        }
    }

    /// Decides the response from the request (e.g. by a prompt keyword), to
    /// simulate the whole loop offline.
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

    fn complete(&self, req: ChatRequest) -> String {
        (self.responder)(&req)
    }
}

/// A single SSE line event from the provider.
enum SseEvent {
    Delta(String),
    Done,
    Ignore,
}

/// Parses a `data: {...}` line from the stream. Pure and testable.
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
        let line = r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#;
        match parse_sse_line(line) {
            SseEvent::Delta(t) => assert_eq!(t, "hello"),
            _ => panic!("expected delta"),
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
        let provider = Provider::Mock(MockProvider::new("one two three"));
        let stream = provider
            .stream(ChatRequest {
                model: "mock".to_string(),
                messages: vec![ChatMessage::user("hi")],
                temperature: None,
            })
            .await
            .unwrap();

        let tokens: Vec<String> = stream.map(|r| r.unwrap()).collect().await;
        assert!(tokens.len() > 1, "should stream in multiple tokens");
        assert_eq!(tokens.concat(), "one two three");
    }
}
