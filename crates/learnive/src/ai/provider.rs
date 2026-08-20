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

/// Retries a bare transport failure (connection reset, DNS hiccup, a
/// malformed/truncated body) before giving up — live QA runs (2026-08-18)
/// against the free-tier default provider hit exactly this class of error
/// ("error sending request", "error decoding response body") often enough
/// to strand a whole node, and once every prerequisite under a locked
/// main-line item hits the same wall, the document itself gets stuck with
/// nothing available to generate. Only ever retries before any bytes of a
/// successful response have reached the caller: `complete()` retries the
/// full round trip (nothing is shown to the user until it returns — used
/// for `test`/`profile`/outline/rubric/grading calls, never the live
/// prose stream), `stream()` retries only the initial connect — once a
/// chunk has actually been read from a 200 response, retrying would risk
/// duplicating or racing content already on its way to a live SSE client.
const TRANSIENT_RETRIES: u32 = 2;
const RETRY_BACKOFF: Duration = Duration::from_millis(750);

/// Upper bound on a single `retry-after`-driven sleep, independent of what
/// the header claims. `complete()` already wraps its whole retry loop in
/// `COMPLETE_BUDGET`, so an oversized value there just gets cut off — but
/// `stream()`'s retry loop (below) has no outer budget of its own, so an
/// unclamped header value could stall a live SSE connect indefinitely on a
/// misbehaving or malicious provider. Groq's own observed reset window
/// (2026-08-20 probes) was 13-55s, comfortably inside this.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Wall-clock ceiling on `complete()`'s whole retry loop, independent of how
/// many attempts it takes. This is a *hard* `tokio::time::timeout` around
/// the entire loop, so raising it does not reopen the retry-storm risk
/// described below — total wall time is capped here no matter how many
/// attempts fire inside it.
///
/// Previously assumed structured calls (test/profile/outline/rubric/
/// grading) "normally finish in single-digit seconds even on a slow
/// model" and used 60s. Live QA (2026-08-20) refuted that: a real node's
/// `test` move failed this budget deterministically (2/2) with the exact
/// "gave up after retrying" error below, and direct-provider probes
/// (bypassing learnive) confirmed why — the free-tier default provider is
/// healthy (0.84s time-to-first-token on the same prompt streamed) but a
/// non-streamed structured call for a realistic grounded prompt
/// legitimately takes 126-147s to return anything at all, well past the
/// old 60s. 200s covers that with margin while staying just under the
/// client's 210s send timeout above, so a genuine stall surfaces this
/// crate's own clearer message instead of a raw reqwest error. Retrying
/// 5xx/429 (added 2026-08-19, see `TRANSIENT_RETRIES`'s doc comment) still
/// only gets meaningful extra attempts for *fast* transient failures now —
/// a second full ~140s attempt plus a first mostly exhausts 200s already,
/// which is the correct trade: a synchronous caller's wait is bounded here
/// either way, so there's nothing to budget around beyond this constant.
const COMPLETE_BUDGET: Duration = Duration::from_secs(200);

/// Stream of text deltas (§14 — token-by-token streaming for low TTFT).
pub type TokenStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>;

#[derive(Debug)]
pub enum ProviderError {
    /// HTTP transport failure.
    Http(String),
    /// The API responded with an error status. `retry_after` carries a
    /// parsed `Retry-After` header (seconds) when the response had one —
    /// only ever populated on a 429, since that's the only status a caller
    /// acts on it for.
    Api {
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::Http(e) => write!(f, "HTTP error: {e}"),
            ProviderError::Api { status, body, .. } => {
                write!(f, "API error ({status}): {body}")
            }
        }
    }
}

/// Parses a `Retry-After` header value as whole seconds (the only form the
/// providers this app targets — Groq, OpenCode Zen, OpenRouter — are known
/// to send; the HTTP-date form is not handled since none of them use it).
fn retry_after_from_headers(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
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
            // event in the first place. Live measurement (2026-08-20,
            // direct-provider probes against this same free-tier default,
            // bypassing learnive) found a *non-streamed* `chat/completions`
            // call — used only by `complete()`, never the live prose
            // stream — legitimately taking 126-147s for a realistic
            // structured-move prompt (~8.5KB, grounded); TTFT on the same
            // prompt streamed was 0.84s, ruling out a dead/degraded
            // provider. 210s comfortably covers that with margin and stays
            // above `COMPLETE_BUDGET` below, so *that* timeout's clearer
            // message fires first on a genuine stall instead of a raw
            // reqwest error; a short connect timeout still fails fast on a
            // dead endpoint instead of queueing behind the same 210s.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(210))
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

        let mut last_err: Option<ProviderError> = None;
        let mut sent = None;
        for attempt in 0..=TRANSIENT_RETRIES {
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
            match builder.send().await {
                Ok(resp) if resp.status().is_success() => {
                    sent = Some(resp);
                    break;
                }
                // A non-2xx status arrives before any body chunk is read —
                // still "initial connect" territory, same as a transport
                // failure below: retrying here can never duplicate/race
                // content already on its way to a live SSE client, since
                // nothing has been yielded yet.
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let retry_after = retry_after_from_headers(resp.headers());
                    let body = tokio::time::timeout(BODY_READ_TIMEOUT, resp.text())
                        .await
                        .ok()
                        .and_then(|r| r.ok())
                        .unwrap_or_default();
                    let retryable = status == 429 || (500..600).contains(&status);
                    let err = ProviderError::Api {
                        status,
                        body,
                        retry_after,
                    };
                    if retryable && attempt < TRANSIENT_RETRIES {
                        let backoff = retry_after
                            .map(|d| d.min(MAX_RETRY_AFTER))
                            .unwrap_or(RETRY_BACKOFF);
                        last_err = Some(err);
                        tokio::time::sleep(backoff).await;
                    } else {
                        return Err(err);
                    }
                }
                Err(e) => {
                    last_err = Some(ProviderError::Http(e.to_string()));
                    if attempt < TRANSIENT_RETRIES {
                        tokio::time::sleep(RETRY_BACKOFF).await;
                    }
                }
            }
        }
        let resp = sent.ok_or_else(|| {
            last_err.unwrap_or_else(|| ProviderError::Http("no attempt was made".to_string()))
        })?;

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
        match tokio::time::timeout(COMPLETE_BUDGET, self.complete_retrying(&req)).await {
            Ok(result) => result,
            Err(_) => Err(ProviderError::Http(format!(
                "gave up after retrying for {COMPLETE_BUDGET:?} without a successful response"
            ))),
        }
    }

    async fn complete_retrying(&self, req: &ChatRequest) -> Result<String, ProviderError> {
        let mut last_err = None;
        for attempt in 0..=TRANSIENT_RETRIES {
            match self.complete_once(req).await {
                Ok(text) => return Ok(text),
                // Most Api errors carry a real status the provider chose to
                // return (auth, bad request, ...) — repeating the exact same
                // request won't change that. But 5xx/429 are the provider
                // reporting its OWN transient trouble (live QA 2026-08-19 hit
                // a bare 500 "Internal server error" from the free-tier
                // provider on an otherwise-valid grading request) — worth the
                // same retry as a transport failure.
                Err(e @ ProviderError::Api { status, .. })
                    if status != 429 && !(500..600).contains(&status) =>
                {
                    return Err(e);
                }
                Err(e) => {
                    // A 429's `Retry-After` is the provider telling us
                    // exactly how long its own limiter window is — use it
                    // over the fixed backoff when present, clamped so one
                    // huge/bogus header value can't eat the whole
                    // `COMPLETE_BUDGET` in a single sleep.
                    let backoff = if let ProviderError::Api {
                        retry_after: Some(d),
                        ..
                    } = &e
                    {
                        (*d).min(MAX_RETRY_AFTER)
                    } else {
                        RETRY_BACKOFF
                    };
                    last_err = Some(e);
                    if attempt < TRANSIENT_RETRIES {
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }
        Err(last_err.expect("loop always sets last_err before exiting on retry exhaustion"))
    }

    async fn complete_once(&self, req: &ChatRequest) -> Result<String, ProviderError> {
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
            let retry_after = retry_after_from_headers(resp.headers());
            let body = tokio::time::timeout(BODY_READ_TIMEOUT, resp.text())
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            return Err(ProviderError::Api {
                status,
                body,
                retry_after,
            });
        }

        let text = tokio::time::timeout(BODY_READ_TIMEOUT, resp.text())
            .await
            .map_err(|_| {
                ProviderError::Http(format!(
                    "reading response body stalled for {BODY_READ_TIMEOUT:?}"
                ))
            })?
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        // `.json()` alone only reports "error decoding response body" with no
        // way to see what actually came back — live QA (2026-08-19) hit this
        // repeatedly against the free-tier default provider with no way to
        // tell a genuine malformed/truncated body from a shape this struct
        // doesn't expect. A snippet of the real text is worth the extra
        // allocation on the (hopefully rare) failure path.
        let body: Resp = serde_json::from_str(&text).map_err(|e| {
            let snippet: String = text.chars().take(300).collect();
            ProviderError::Http(format!(
                "decoding response body: {e} — body was: {snippet:?}"
            ))
        })?;
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

    #[test]
    fn retry_after_parses_seconds_and_ignores_garbage_or_missing() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "13".parse().unwrap());
        assert_eq!(
            retry_after_from_headers(&headers),
            Some(Duration::from_secs(13))
        );

        let empty = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_from_headers(&empty), None);

        let mut malformed = reqwest::header::HeaderMap::new();
        malformed.insert(
            "retry-after",
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after_from_headers(&malformed), None);
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
