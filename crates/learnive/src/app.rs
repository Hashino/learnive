//! Montagem do estado da aplicação e do roteador HTTP.
//!
//! `build_router` é puro (recebe o estado, devolve o `Router`), separado de
//! `main` para que os testes exercitem o roteador com `oneshot` sem abrir porta.

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

/// Estado compartilhado. Barato de clonar (tudo atrás de `Arc`/handles leves).
#[derive(Clone)]
pub struct AppState {
    /// Token de sessão exigido em toda requisição (§3.1).
    pub token: Arc<str>,
    /// Origins aceitos (ex.: `http://127.0.0.1:7420`). Nunca `*`.
    pub allowed_origins: Arc<HashSet<String>>,
    /// Hosts aceitos (ex.: `127.0.0.1:7420`) — defesa de DNS-rebinding.
    pub allowed_hosts: Arc<HashSet<String>>,
    /// Armazenamento em arquivos (§4).
    pub store: Store,
    /// Provedor de IA + tiering (§12).
    pub ai: Arc<Ai>,
}

impl AppState {
    /// Constrói o estado para uma dada porta, gerando um token novo.
    pub fn new(port: u16) -> Self {
        let allowed_origins = HashSet::from([
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
        ]);
        let allowed_hosts =
            HashSet::from([format!("127.0.0.1:{port}"), format!("localhost:{port}")]);

        let data_dir =
            std::env::var("LEARNIVE_DATA_DIR").unwrap_or_else(|_| "learnive-data".to_string());
        let store = Store::open(&data_dir).expect("abrir armazenamento de dados");

        Self {
            token: Arc::from(security::generate_token()),
            allowed_origins: Arc::new(allowed_origins),
            allowed_hosts: Arc::new(allowed_hosts),
            store,
            ai: Arc::new(api::build_ai()),
        }
    }
}

/// Monta o roteador com a camada de segurança (§3.1) por cima de tudo.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/events", get(events))
        // Loop de currículo (§6, §8). Tudo POST — mutações nunca em GET (§3.1).
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

/// Página inicial: injeta o token na meta tag para o cliente reenviá-lo como
/// cabeçalho (nunca cookie) nas requisições seguintes.
async fn index(State(state): State<AppState>) -> Html<String> {
    Html(include_str!("assets/index.html").replace("{{TOKEN}}", &state.token))
}

/// Liveness. Não expõe nada sensível; ainda assim exige token pela camada.
async fn health() -> &'static str {
    "ok"
}

/// Esqueleto de SSE (§3): canal servidor→cliente por onde o conteúdo gerado
/// será streamado token-a-token nas fases seguintes.
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
    use tower::ServiceExt; // para `oneshot`

    const TOKEN: &str = "testtoken";
    const HOST: &str = "127.0.0.1:7420";
    const ORIGIN: &str = "http://127.0.0.1:7420";

    fn test_state() -> AppState {
        // Store em diretório temporário único; IA em modo demo (mock scriptado).
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
        // O token foi injetado na página.
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
        // Host do atacante (DNS-rebinding) rejeitado mesmo com token válido.
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
    async fn mutating_endpoint_rejects_get() {
        // Nenhuma mutação responde a GET (§3.1): /api/documents só existe como POST.
        let req = Request::builder()
            .uri("/api/documents")
            .header("host", HOST)
            .header("x-learnive-token", TOKEN)
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(req).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    /// O loop inteiro fecha em modo demo: cria documento → gera nó (stream) →
    /// responde → avança (§6, §8). Uma única `AppState` compartilhada entre as
    /// requisições (mesmo store + IA).
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

        // 1. Cold start: tema → outline.
        let (status, body) = call(authed("POST", "/api/documents", r#"{"topic":"frações"}"#)).await;
        assert_eq!(status, StatusCode::OK);
        let created: serde_json::Value = serde_json::from_str(&body).unwrap();
        let doc_id = created["doc_id"].as_str().unwrap().to_string();
        assert!(!created["titles"].as_array().unwrap().is_empty());

        // 2. Geração do nó 0 — streama e termina com `done`.
        let (status, body) = call(authed(
            "POST",
            &format!("/api/documents/{doc_id}/nodes/0/generate"),
            "",
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("event: token"), "deve streamar prosa");
        assert!(body.contains("event: done"), "deve concluir a geração");

        // 3. Resposta ao exercício → avança (demo grada como demonstrado).
        let (status, body) = call(authed(
            "POST",
            &format!("/api/documents/{doc_id}/nodes/n0/answer"),
            r#"{"answer":"aplico o conceito a um caso novo assim..."}"#,
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        let ans: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(ans["advance"], serde_json::json!(true));
    }
}
