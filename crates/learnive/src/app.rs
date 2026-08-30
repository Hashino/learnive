//! Application-state assembly and HTTP router.
//!
//! `build_router` is pure (takes the state, returns the `Router`), separated from
//! `main` so tests can exercise the router with `oneshot` without opening a port.

use std::{collections::HashSet, convert::Infallible, sync::Arc};

use axum::{
    Router,
    extract::{Path, State},
    http::{StatusCode, header},
    middleware,
    response::{
        Html, IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{delete, get, post, put},
};
use tokio_stream::Stream;

use arc_swap::ArcSwap;
use tokio::sync::RwLock;

use crate::ai::Ai;
use crate::config::AppConfig;
use crate::movement::AgentPolicy;
use crate::retrieval::Retriever;
use crate::secret::SecretStore;
use crate::source::{Corpus, Source};
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
    /// AI provider + tiering (§12). Hot-swappable so the settings window
    /// applies without a restart — read per request with `state.ai.load_full()`.
    pub ai: Arc<ArcSwap<Ai>>,
    /// Policy-ladder rung (§14) that goes with `ai` — always set together with
    /// it (`api::build_ai`), never derived from `config` alone (see that
    /// function's doc comment).
    pub policy: Arc<ArcSwap<AgentPolicy>>,
    /// Persisted user config (provider + intent, §12/§12.1). No secrets.
    pub config: Arc<RwLock<AppConfig>>,
    /// Secret store for API keys (§12) — file-first, never a central DB.
    pub secret: Arc<SecretStore>,
    /// Data directory, needed to persist config on setup.
    pub data_dir: Arc<str>,
    /// Source acquisition backend (§11.1) — swappable; `Source::Unconfigured`
    /// when no mirror URL is configured (see `build_source`).
    pub source: Arc<Source>,
    /// §11.1's fallback tier — tried when `source` finds nothing for a query
    /// (`api::cold_start::acquire`); `Source::Unconfigured` when no secondary
    /// mirror URL is configured.
    pub fallback_source: Arc<Source>,
    /// Immutable source corpus (§4/§11).
    pub corpus: Corpus,
    /// Retrieval index for grounding (§10). `None` when the embedding model could
    /// not be loaded — the loop then runs ungrounded rather than failing.
    pub retriever: Option<Arc<RwLock<Retriever>>>,
    /// S27d/S27e: the real HTTP client `api::cold_start::verify_reading_list`
    /// checks every proposed book/article against. Swappable the same way
    /// `ai`/`source` are: `app::tests::test_state_with_ai` wires
    /// `BibliographyClient::unreachable_for_test()` instead, so an
    /// integration test that creates a document never makes a real network
    /// call just because it skipped the outline-confirmation screen.
    pub bibliography_client: Arc<crate::source::BibliographyClient>,
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
        let corpus = Corpus::open(&data_dir).expect("open source corpus");
        let config = AppConfig::load(&data_dir);
        let secret = SecretStore::open(&data_dir);

        // Load the embedding model and open the retrieval index (§10). Non-fatal:
        // offline / first-run-download failures just disable grounding.
        let retriever = match crate::retrieval::Embedder::default_model() {
            Ok(embedder) => match Retriever::open(&data_dir, &corpus, embedder) {
                Ok(r) => Some(Arc::new(RwLock::new(r))),
                Err(e) => {
                    eprintln!("grounding disabled (index): {e}");
                    None
                }
            },
            Err(e) => {
                eprintln!("grounding disabled (embedding model): {e}");
                None
            }
        };

        let (ai, policy) = api::build_ai(&config, &secret);
        let ai = Arc::new(ArcSwap::from_pointee(ai));
        let policy = Arc::new(ArcSwap::from_pointee(policy));

        Self {
            token: Arc::from(security::generate_token()),
            allowed_origins: Arc::new(allowed_origins),
            allowed_hosts: Arc::new(allowed_hosts),
            store,
            ai,
            policy,
            config: Arc::new(RwLock::new(config)),
            secret: Arc::new(secret),
            data_dir: Arc::from(data_dir.as_str()),
            source: {
                let source = api::build_source();
                // A missing backend is expected post-pivot (§11.1 origin is a
                // deliberately open question): say so once instead of spamming
                // an error on every acquisition. The document still generates,
                // just ungrounded, until a backend is wired up.
                if matches!(source, crate::source::Source::Unconfigured) {
                    println!(
                        "note: no source acquisition backend configured — \
                         documents will be generated ungrounded. Set \
                         LEARNIVE_LIBGEN_URL or LEARNIVE_SCIHUB_URL to enable \
                         grounding (a mirror you supply; no default is baked in)."
                    );
                }
                Arc::new(source)
            },
            fallback_source: Arc::new(api::build_fallback_source()),
            corpus,
            retriever,
            bibliography_client: Arc::new(crate::source::BibliographyClient::new()),
        }
    }
}

/// Assembles the router with the security layer (§3.1) on top of everything.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        // Page assets (§15: still baked into the binary, just not into one
        // file). Fixed table, no filesystem lookup — nothing to traverse.
        .route("/assets/{file}", get(asset))
        .route("/health", get(health))
        .route("/events", get(events))
        // Setup (§12): GET reads status (no secret); POST saves + hot-swaps.
        .route("/api/setup", get(api::setup_status).post(api::save_setup))
        // Curriculum loop (§6, §8, §S4). All POST — mutations never on GET (§3.1).
        .route("/api/objective/propose", post(api::propose_objective))
        // §S15/§S16: proposes the WHOLE outline tree (prerequisites and the
        // objective's own decomposition, unified into one ordered array) for
        // a topic/objective, resolved against existing documents —
        // stateless, like `objective/propose`, brackets `create_document`
        // which receives the learner's confirmed choices back.
        .route("/api/outline/propose", post(api::propose_outline))
        // GET lists the existing living documents so the app can reopen where
        // the last session left off (§S12); POST creates a new one.
        .route(
            "/api/documents",
            get(api::list_documents).post(api::create_document),
        )
        // Read-only outline + gate state (§S5) — GET is fine, it mutates nothing.
        .route("/api/documents/{doc}/outline", get(api::get_outline))
        // Non-destructive read of an already-generated node (§S5/§4.3) —
        // GET, so revisiting never risks the write path in `generate`.
        .route("/api/documents/{doc}/nodes/{id}", get(api::get_node))
        .route(
            "/api/documents/{doc}/nodes/{id}/generate",
            post(api::generate_node),
        )
        .route(
            "/api/documents/{doc}/nodes/{node}/answer",
            post(api::answer),
        )
        // Sandboxed exercise frame (§4.4): its own CSP, served instead of
        // `iframe.srcdoc` so it survives the app origin's CSP hardening.
        .route(
            "/api/documents/{doc}/nodes/{node}/exercise-frame",
            get(api::exercise_frame),
        )
        // Sandboxed interactive-island frame (§4.4, §S11) — the generalized
        // sibling of the exercise frame above, for any `data-interactive`
        // block a streamed move opened mid-prose.
        .route(
            "/api/documents/{doc}/nodes/{node}/blocks/{block}/frame",
            get(api::block_frame),
        )
        .route("/api/documents/{doc}/nodes/{id}/skip", post(api::skip_node))
        // Terminal remediation for a chapter match_chapter (S27g) could not
        // place — the "skip the whole book" and "pick the page yourself"
        // arms (the third, "restart", is a plain client-side re-run of cold
        // start and needs no route of its own). See `skip_book`'s doc
        // comment for why this is not just `skip_node` looped client-side.
        .route(
            "/api/documents/{doc}/outline/{item}/skip_book",
            post(api::skip_book),
        )
        .route(
            "/api/documents/{doc}/outline/{item}/resolved_page",
            put(api::set_resolved_page),
        )
        // On-demand retrieval practice on an already-demonstrated node
        // (§S15 item 5) — overwrites the same rubric sidecar remediation
        // does, so `exercise-frame`/`answer` above serve/grade it unchanged.
        .route(
            "/api/documents/{doc}/nodes/{id}/practice",
            post(api::practice_node),
        )
        // Reading interactions (§S6, §9 "the document is the answer") — only
        // valid on an already-finalized node (see `api.rs`'s §S6 doc comment).
        .route(
            "/api/documents/{doc}/nodes/{id}/ask",
            post(api::ask_question),
        )
        .route(
            "/api/documents/{doc}/nodes/{id}/annotate",
            post(api::annotate),
        )
        .route(
            "/api/documents/{doc}/nodes/{id}/annotations/{annotation}",
            put(api::update_annotation),
        )
        .route(
            "/api/documents/{doc}/nodes/{id}/read-to-end",
            post(api::read_to_end),
        )
        .route(
            "/api/documents/{doc}/plan/decide",
            post(api::decide_plan_proposal),
        )
        .route(
            "/api/documents/{doc}/objective",
            post(api::revise_objective),
        )
        // "What are we learning next?" (§S15c) — appends a new epoch to an
        // already-completed document instead of starting a new one.
        .route("/api/documents/{doc}/next", post(api::next_topic))
        // Document display name (§S12) — the sidebar's title, renameable.
        .route("/api/documents/{doc}/name", post(api::rename_document))
        // Deleting a whole living document (§S12) — DELETE, never GET (§3.1).
        .route("/api/documents/{doc}", delete(api::delete_document))
        .route(
            "/api/documents/{doc}/profile",
            get(api::get_profile).post(api::revise_profile),
        )
        // S27f: the acervo gate report ("what's missing"), PDF<->item
        // manual matching, and TOC-confirmation screens. Read/act-on-demand:
        // enforcement lives in `api::reading::ensure_document_grounded`
        // (S27m), which refuses generation while the acervo isn't clear.
        .route("/api/documents/{doc}/acervo", get(api::get_acervo_report))
        .route(
            "/api/documents/{doc}/acervo/matches",
            get(api::get_acervo_matches).post(api::set_acervo_match),
        )
        .route(
            "/api/documents/{doc}/acervo/toc/{item}",
            get(api::get_acervo_toc).put(api::put_acervo_toc),
        )
        // Read-only source viewer (§11) — the corpus is global, not per-document
        // (§4), so this lives outside the `/api/documents/{doc}` tree; a citation
        // click resolves here for its `data-source-id`. Meta+toc only: the
        // display surface is the browser's native PDF viewer, not this app's
        // own reader (§4/§11, post-pivot).
        .route("/api/sources/{id}", get(api::get_source))
        // The canonical PDF artifact (§4/§11) — served as-is for the native
        // viewer to render; content-addressed by source id, safe to cache.
        .route(
            "/api/sources/{id}/assets/{filename}",
            get(api::get_source_asset),
        )
        // S27n: citations on real generated documents cite a local-library
        // content hash (`ground_node`'s `<cite data-source-id>`), which never
        // matches a `state.corpus` id — this route resolves that hash
        // straight against `<data>/library/` instead. Kept a separate path
        // rather than overloading `/api/sources/{id}` so the corpus route
        // stays simple and this one owns its own not-found semantics (hash
        // not in the library vs. corpus source id not found mean different
        // things to the client).
        .route("/api/library/{hash}", get(api::get_library_meta))
        .route("/api/library/{hash}/pdf", get(api::get_library_pdf))
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

/// Page assets: the stylesheet and the scripts `index.html` pulls in.
///
/// They are still compiled into the binary (§15 portability) — the split is
/// purely so the page is editable; there is no asset directory to ship and no
/// path is ever taken from the request, only matched against this table.
async fn asset(Path(file): Path<String>) -> Response {
    const CSS: &str = "text/css; charset=utf-8";
    const JS: &str = "text/javascript; charset=utf-8";
    let (mime, body) = match file.as_str() {
        "app.css" => (CSS, include_str!("assets/app.css")),
        "theme-init.js" => (JS, include_str!("assets/theme-init.js")),
        "i18n.js" => (JS, include_str!("assets/i18n.js")),
        "core.js" => (JS, include_str!("assets/core.js")),
        "documents.js" => (JS, include_str!("assets/documents.js")),
        "acervo.js" => (JS, include_str!("assets/acervo.js")),
        "outline.js" => (JS, include_str!("assets/outline.js")),
        "remediate.js" => (JS, include_str!("assets/remediate.js")),
        "reading.js" => (JS, include_str!("assets/reading.js")),
        "node.js" => (JS, include_str!("assets/node.js")),
        "settings.js" => (JS, include_str!("assets/settings.js")),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    // `no-store`: the asset URLs are stable (no content hash) but their bodies
    // change with every rebuild, and the page they belong to is never cached
    // either — a stale stylesheet here shows up as a layout that "didn't take"
    // long after the code changed, which is a miserable thing to debug. This
    // is a localhost server reading from memory; there is nothing to save.
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
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
mod tests;
