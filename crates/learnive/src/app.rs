//! Application-state assembly and HTTP router.
//!
//! `build_router` is pure (takes the state, returns the `Router`), separated from
//! `main` so tests can exercise the router with `oneshot` without opening a port.

use std::{collections::HashSet, convert::Infallible, sync::Arc};

use axum::{
    Router,
    extract::State,
    middleware,
    response::{
        Html, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use tokio_stream::Stream;

use crate::ai::Ai;
use crate::store::Store;
use crate::{api, security};

/// Shared state. Cheap to clone (everything behind `Arc`/lightweight handles).
#[derive(Clone)]
pub struct AppState {
    /// Session token required on every request (§3.1).
    pub token: Arc<str>,
    /// Accepted origins (e.g. `http://127.0.0.1:7420`). Never `*`.
    pub allowed_origins: Arc<HashSet<String>>,
    /// Accepted hosts (e.g. `127.0.0.1:7420`) — DNS-rebinding defense.
    pub allowed_hosts: Arc<HashSet<String>>,
    /// File storage (§4).
    pub store: Store,
    /// AI provider + tiering (§12).
    pub ai: Arc<Ai>,
}

impl AppState {
    /// Builds the state for a given port, generating a fresh token.
    pub fn new(port: u16) -> Self {
        let allowed_origins = HashSet::from([
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
        ]);
        let allowed_hosts =
            HashSet::from([format!("127.0.0.1:{port}"), format!("localhost:{port}")]);

        let data_dir =
            std::env::var("LEARNIVE_DATA_DIR").unwrap_or_else(|_| "learnive-data".to_string());
        let store = Store::open(&data_dir).expect("open data store");

        Self {
            token: Arc::from(security::generate_token()),
            allowed_origins: Arc::new(allowed_origins),
            allowed_hosts: Arc::new(allowed_hosts),
            store,
            ai: Arc::new(api::build_ai()),
        }
    }
}

/// Assembles the router with the security layer (§3.1) on top of everything.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/events", get(events))
        // Curriculum loop (§6, §8). All POST — mutations never on GET (§3.1).
        .route("/api/documents", post(api::create_document))
        .route(
            "/api/documents/{doc}/nodes/{index}/generate",
            post(api::generate_node),
        )
        .route(
            "/api/documents/{doc}/nodes/{node}/answer",
            post(api::answer),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security::guard,
        ))
        .with_state(state)
}

/// Home page: injects the token into the meta tag so the client resends it as a
/// header (never a cookie) on subsequent requests.
async fn index(State(state): State<AppState>) -> Html<String> {
    Html(include_str!("assets/index.html").replace("{{TOKEN}}", &state.token))
}

/// Liveness. Exposes nothing sensitive; still requires the token via the layer.
async fn health() -> &'static str {
    "ok"
}

/// SSE skeleton (§3): the server→client channel over which generated content
/// will be streamed token by token in later phases.
async fn events() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = tokio_stream::iter([Ok(Event::default()
        .event("hello")
        .data("learnive SSE online"))]);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for `oneshot`

    const TOKEN: &str = "testtoken";
    const HOST: &str = "127.0.0.1:7420";
    const ORIGIN: &str = "http://127.0.0.1:7420";

    fn test_state() -> AppState {
        // Store in a unique temp directory; AI in demo mode (scripted mock).
        let dir = std::env::temp_dir().join(format!("learnive-test-{}", crate::engine::new_id()));
        AppState {
            token: Arc::from(TOKEN),
            allowed_origins: Arc::new(HashSet::from([ORIGIN.to_string()])),
            allowed_hosts: Arc::new(HashSet::from([HOST.to_string()])),
            store: crate::store::Store::open(dir).unwrap(),
            ai: Arc::new(crate::api::demo_ai()),
        }
    }

    fn router() -> Router {
        build_router(test_state())
    }

    async fn send(req: Request<Body>) -> (StatusCode, String) {
        let resp = router().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn authed(method: &str, uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("host", HOST)
            .header("x-learnive-token", TOKEN)
            .header("origin", ORIGIN)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn index_ok_with_query_token() {
        let req = Request::builder()
            .uri("/?token=testtoken")
            .header("host", HOST)
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(req).await;
        assert_eq!(status, StatusCode::OK);
        // The token was injected into the page.
        assert!(body.contains(TOKEN));
    }

    #[tokio::test]
    async fn rejects_missing_token() {
        let req = Request::builder()
            .uri("/")
            .header("host", HOST)
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rejects_wrong_token() {
        let req = Request::builder()
            .uri("/?token=nope")
            .header("host", HOST)
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn accepts_header_token() {
        let req = Request::builder()
            .uri("/health")
            .header("host", HOST)
            .header("x-learnive-token", TOKEN)
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn rejects_bad_origin() {
        let req = Request::builder()
            .uri("/health")
            .header("host", HOST)
            .header("x-learnive-token", TOKEN)
            .header("origin", "http://evil.example")
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn accepts_allowed_origin() {
        let req = Request::builder()
            .uri("/health")
            .header("host", HOST)
            .header("x-learnive-token", TOKEN)
            .header("origin", ORIGIN)
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(req).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_bad_host() {
        // Attacker host (DNS-rebinding) rejected even with a valid token.
        let req = Request::builder()
            .uri("/health")
            .header("host", "evil.example")
            .header("x-learnive-token", TOKEN)
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn responses_carry_csp_header() {
        // Defense in depth §3.1: every response carries a CSP.
        let req = Request::builder()
            .uri("/health")
            .header("host", HOST)
            .header("x-learnive-token", TOKEN)
            .body(Body::empty())
            .unwrap();
        let resp = router().oneshot(req).await.unwrap();
        let csp = resp
            .headers()
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(csp.contains("default-src 'self'"));
        assert!(csp.contains("connect-src 'self'"));
    }

    #[tokio::test]
    async fn mutating_endpoint_rejects_get() {
        // No mutation responds to GET (§3.1): /api/documents exists only as POST.
        let req = Request::builder()
            .uri("/api/documents")
            .header("host", HOST)
            .header("x-learnive-token", TOKEN)
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(req).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    /// The whole loop closes in demo mode: create document → generate node
    /// (stream) → answer → advance (§6, §8). A single `AppState` shared across the
    /// requests (same store + AI).
    #[tokio::test]
    async fn full_loop_closes_in_demo_mode() {
        let state = test_state();
        let call = |req: Request<Body>| {
            let state = state.clone();
            async move {
                let resp = build_router(state).oneshot(req).await.unwrap();
                let status = resp.status();
                let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                (status, String::from_utf8_lossy(&bytes).into_owned())
            }
        };

        // 1. Cold start: topic → outline.
        let (status, body) =
            call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
        assert_eq!(status, StatusCode::OK);
        let created: serde_json::Value = serde_json::from_str(&body).unwrap();
        let doc_id = created["doc_id"].as_str().unwrap().to_string();
        assert!(!created["titles"].as_array().unwrap().is_empty());

        // 2. Node 0 generation — streams and ends with `done`.
        let (status, body) = call(authed(
            "POST",
            &format!("/api/documents/{doc_id}/nodes/0/generate"),
            "",
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("event: token"), "should stream prose");
        assert!(body.contains("event: done"), "should finish generation");

        // 3. Answer the exercise → advance (demo grades as demonstrated).
        let (status, body) = call(authed(
            "POST",
            &format!("/api/documents/{doc_id}/nodes/n0/answer"),
            r#"{"answer":"I apply the concept to a new case like this..."}"#,
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        let ans: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(ans["advance"], serde_json::json!(true));
    }
}
