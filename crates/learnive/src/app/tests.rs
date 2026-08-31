use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt; // for `oneshot`

const TOKEN: &str = "testtoken";
const HOST: &str = "127.0.0.1:7420";
const ORIGIN: &str = "http://127.0.0.1:7420";

fn test_state() -> AppState {
    test_state_with_ai(crate::api::demo_ai())
}

// §S8: a custom scripted `Ai` for tests that need a decision the demo
// responder never makes on its own (e.g. spawning a sub-node) — same
// store/secret/source setup as `test_state`, just a different provider.
fn test_state_with_ai(ai: crate::ai::Ai) -> AppState {
    // Store in a unique temp directory; AI in demo mode (scripted mock);
    // acquisition mocked (no network).
    let dir = std::env::temp_dir().join(format!("learnive-test-{}", crate::engine::new_id()));

    // S27m closed the acervo gate hard: `demo_responder`'s reading-list
    // proposal ("propose the initial READING LIST") always names two
    // bibliographic items, "Demo Foundations" and "Demo Document" by "Demo
    // Author" (`api/provider.rs`) — every demo-mode document is now
    // bibliographically sourced, and `ensure_document_grounded`/`ground_node`
    // both hard-refuse to generate without a real, indexed PDF backing each
    // one. Deferred at the time (user, mid-session: "for now continue.
    // later we'll add a pdf specifically for demo mode") — this is that pdf,
    // now built. Both entries are `"kind":"book"`, which enforces an
    // `MIN_PLAUSIBLE_BOOK_PAGES` floor (`source::acervo`), hence 8 pages.
    // S27i: `write_book_pdf` and the "Demo Foundations"/"Demo Document"
    // identity live in `source::mock` now, shared with the eager library
    // seed `app::AppState::new` runs for a live `LEARNIVE_DEMO=1` server —
    // see that function's doc comment for why the seed moved out of
    // `Source::Mock::fetch` instead of reforming it in place.
    let library = crate::source::LocalPdfSource::open(&dir).unwrap();
    let (t1, a1) = crate::source::mock::DEMO_BOOK_1;
    let (t2, a2) = crate::source::mock::DEMO_BOOK_2;
    crate::source::mock::write_book_pdf(&library.root().join("demo-foundations.pdf"), t1, a1);
    crate::source::mock::write_book_pdf(&library.root().join("demo-document.pdf"), t2, a2);

    let corpus = Corpus::open(&dir).unwrap();
    // A retriever is likewise now load-bearing for every bibliographic node
    // (`ground_node` hard-requires `state.retriever` to embed its query
    // against the acervo gate's per-PDF cache) — `Embedder::Mock` gives
    // tests a real, working `Embedder` with no model download, same spirit
    // as `Ai::Mock`/`Source::Mock` above.
    let retriever =
        crate::retrieval::Retriever::open(&dir, &corpus, crate::retrieval::Embedder::Mock).unwrap();

    AppState {
        token: Arc::from(TOKEN),
        allowed_origins: Arc::new(HashSet::from([ORIGIN.to_string()])),
        allowed_hosts: Arc::new(HashSet::from([HOST.to_string()])),
        store: crate::store::Store::open(&dir).unwrap(),
        ai: Arc::new(ArcSwap::from_pointee(ai)),
        policy: Arc::new(ArcSwap::from_pointee(AgentPolicy::L0)),
        config: Arc::new(RwLock::new(AppConfig::default())),
        secret: Arc::new(SecretStore::open(&dir)),
        data_dir: Arc::from(dir.to_string_lossy().as_ref()),
        source: Arc::new(Source::Mock(crate::source::MockSource::new())),
        fallback_source: Arc::new(Source::Mock(crate::source::MockSource::new())),
        corpus,
        retriever: Some(Arc::new(RwLock::new(retriever))),
        // S27e: never hit a real catalog host from an integration test —
        // see the field's own doc comment on `AppState`.
        bibliography_client: Arc::new(crate::source::BibliographyClient::unreachable_for_test()),
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

/// §S18: `/generate` now settles at most one real move per request and ends
/// the stream with `event: move_paused` when more moves remain — a test
/// that wants a node's FULL generation (through its graded exercise, or
/// through a `plan` proposal pausing it) drives that loop the same way
/// `node.js`'s `armReadToEndWatcher` does live: reopen `/generate` on the
/// same node until a terminal event (`event: done` or `event: error`)
/// appears. Concatenates every request's SSE body so existing
/// `body.contains("event: ...")` assertions keep working unchanged. Capped
/// at 8 requests (well above `MAX_MOVES_PER_NODE`'s 4) so a genuine bug that
/// produces neither a terminal nor a `move_paused` event panics instead of
/// hanging the test.
async fn generate_to_completion<F, Fut>(
    call: &F,
    doc_id: &str,
    node_id: &str,
) -> (StatusCode, String)
where
    F: Fn(Request<Body>) -> Fut,
    Fut: std::future::Future<Output = (StatusCode, String)>,
{
    let mut combined = String::new();
    for _ in 0..8 {
        let (status, body) = call(authed(
            "POST",
            &format!("/api/documents/{doc_id}/nodes/{node_id}/generate"),
            "",
        ))
        .await;
        if status != StatusCode::OK {
            return (status, body);
        }
        combined.push_str(&body);
        if body.contains("event: done") || body.contains("event: error") {
            return (status, combined);
        }
        assert!(
            body.contains("event: move_paused"),
            "expected a move_paused pause point, got: {body}"
        );
    }
    panic!("generation did not terminate within 8 requests: {combined}");
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
async fn every_asset_the_page_references_is_served() {
    // The page pulls its style and scripts as subresources, so a typo in a
    // filename is a blank app rather than a compile error. Walk what
    // `index.html` actually asks for and demand a 200 for each. The request
    // is shaped like the browser's: no token header (a <script src> cannot
    // set one), only the `?token=` query the URLs carry, and no Origin —
    // same-origin subresource fetches don't send one.
    let page = include_str!("../assets/index.html");
    let mut referenced = 0;
    for (i, _) in page.match_indices("/assets/") {
        let rest = &page[i..];
        let url = &rest[..rest.find('"').expect("asset href is quoted")];
        let name = url
            .trim_start_matches("/assets/")
            .split('?')
            .next()
            .unwrap();
        referenced += 1;
        let req = Request::builder()
            .uri(url.replace("{{TOKEN}}", TOKEN))
            .header("host", HOST)
            .body(Body::empty())
            .unwrap();
        let resp = router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "asset {name} is not served");
        let mime = resp.headers()[header::CONTENT_TYPE].to_str().unwrap();
        let expected = if name.ends_with(".css") {
            "text/css"
        } else {
            "text/javascript"
        };
        assert!(mime.starts_with(expected), "{name} served as {mime}");
    }
    assert!(
        referenced >= 6,
        "expected the split assets, found {referenced}"
    );

    let (status, _) = send(authed("GET", "/assets/nope.js", "")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mutating_endpoint_rejects_get() {
    // No state-changing endpoint responds to GET (§3.1). `/api/documents`
    // itself is no longer the example: it gained a read-only GET listing
    // (§S12) alongside the POST that creates. Node generation is the
    // clearest remaining mutation — it writes a node file and appends
    // events.
    for uri in [
        "/api/documents/d1/nodes/n1/generate",
        "/api/documents/d1/name",
        "/api/objective/propose",
    ] {
        let req = Request::builder()
            .uri(uri)
            .header("host", HOST)
            .header("x-learnive-token", TOKEN)
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(req).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "GET {uri}");
    }
}

/// §S12: documents survive a restart and are reopenable. Before this,
/// documents were written to disk from the first slice but nothing could
/// read them back, so every page load looked like a fresh install. Covers
/// the whole resume contract: the listing, the resume point, the rename,
/// and — the reason `list_documents` filters on `outline.json` — that the
/// data directory's non-document siblings (`corpus/`, `index/`) never
/// show up as documents.
#[tokio::test]
async fn documents_are_listed_resumable_and_renameable() {
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

    // Nothing yet: an empty list, not an error.
    let (status, body) = call(authed("GET", "/api/documents", "")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.trim(), "[]");

    // Explicit, already-confirmed nodes (as the real client always sends
    // after the outline-confirmation screen) — a single top-level item, so
    // `resume_node_id` (main-line-only, §S15) has something to track without
    // this test also needing to demonstrate a whole decomposition just to
    // clear its gate.
    let (_, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"fractions","name":"Fractions 101","objective_text":"Learn fractions","nodes":[{"id":"n1","title":"Fractions","action":"learn","children":[]}]}"#,
    ))
    .await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(created["name"], "Fractions 101");

    // Listed, but with no resume point until a node actually exists —
    // reopening must never generate one (§12.2).
    let (_, body) = call(authed("GET", "/api/documents", "")).await;
    let docs: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(docs.as_array().unwrap().len(), 1);
    assert_eq!(docs[0]["doc_id"], serde_json::json!(doc_id));
    assert_eq!(docs[0]["name"], "Fractions 101");
    assert!(docs[0]["resume_node_id"].is_null());

    call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;

    let (_, body) = call(authed("GET", "/api/documents", "")).await;
    let docs: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(docs[0]["resume_node_id"], serde_json::json!(node0));

    // Rename: a plain overwrite, reflected in the listing.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/name"),
        r#"{"name":"Frações"}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["name"],
        "Frações"
    );
    let (_, body) = call(authed("GET", "/api/documents", "")).await;
    let docs: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(docs[0]["name"], "Frações");

    // A blank name is refused rather than silently blanking the sidebar.
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/name"),
        r#"{"name":"   "}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // `corpus/` is a sibling of the document directories in the data dir,
    // not a document — it has no outline and must not be listed (nor be
    // renameable into looking like one).
    state.store.create_document("corpus").unwrap();
    let (_, body) = call(authed("GET", "/api/documents", "")).await;
    let docs: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(docs.as_array().unwrap().len(), 1);
    let (status, _) = call(authed(
        "POST",
        "/api/documents/corpus/name",
        r#"{"name":"sneaky"}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Same guard on delete — otherwise this endpoint would happily wipe
    // `corpus/` (the immutable source corpus, §4) or the retrieval index.
    let (status, _) = call(authed("DELETE", "/api/documents/corpus", "")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        state
            .store
            .list_documents()
            .unwrap()
            .contains(&"corpus".to_string())
    );

    // Deleting a real document takes the whole directory with it, and the
    // listing is empty again — nothing left half-deleted behind.
    let (status, _) = call(authed("DELETE", &format!("/api/documents/{doc_id}"), "")).await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = call(authed("GET", "/api/documents", "")).await;
    assert_eq!(body.trim(), "[]");
    assert!(!state.store.list_documents().unwrap().contains(&doc_id));
    assert_eq!(
        call(authed(
            "GET",
            &format!("/api/documents/{doc_id}/outline"),
            ""
        ))
        .await
        .0,
        StatusCode::NOT_FOUND
    );
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

    // 1. Cold start: topic → outline. Only the first item is available
    // (§S5 gate — a fresh linear chain has nothing demonstrated yet).
    let (status, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    assert_eq!(status, StatusCode::OK);
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let items = created["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert_eq!(items[0]["state"], serde_json::json!("available"));
    if items.len() > 1 {
        assert_eq!(items[1]["state"], serde_json::json!("locked"));
    }
    let node0 = items[0]["id"].as_str().unwrap().to_string();

    // 2. First node's generation — one move per request (§S18); drive it
    // through to its graded exercise.
    let (status, body) = generate_to_completion(&call, &doc_id, &node0).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("event: token"), "should stream prose");
    assert!(body.contains("event: done"), "should finish generation");

    // 3. Answer the exercise → advance (demo grades as demonstrated).
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/answer"),
        r#"{"answer":"I apply the concept to a new case like this..."}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let ans: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ans["advance"], serde_json::json!(true));

    // 4. The next item (if any) is now unlocked — the gate reacted to
    // the just-recorded `MoveGraded` without any extra bookkeeping.
    let (status, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/outline"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let outline: serde_json::Value = serde_json::from_str(&body).unwrap();
    let items = outline["items"].as_array().unwrap();
    assert_eq!(items[0]["state"], serde_json::json!("demonstrated"));
    if items.len() > 1 {
        assert_eq!(items[1]["state"], serde_json::json!("available"));
    }
}

#[tokio::test]
async fn locked_node_refuses_generation_and_skip() {
    // §S5: a node whose prerequisites aren't demonstrated yet is refused
    // both as a generation target and as a skip target — the gate is
    // enforced server-side, not just hidden in the UI.
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

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let items = created["items"].as_array().unwrap();
    assert!(
        items.len() > 1,
        "demo outline always has more than one item"
    );
    // §S16: a caller that skips the confirmation screen (no `nodes` in the
    // body) never approved the proposed prerequisites, so `create_document`'s
    // fallback must drop them rather than silently committing to them —
    // pin that here, since no other test asserts it.
    assert!(
        !items.iter().any(|it| it["title"] == "Demo prerequisite"),
        "unconfirmed prerequisites must be dropped by the create_document fallback"
    );
    let locked_id = items[1]["id"].as_str().unwrap().to_string();

    // Generation on the locked node ends the SSE stream with an error,
    // never a node.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{locked_id}/generate"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "SSE responses are always 200");
    assert!(body.contains("event: error"));
    assert!(body.contains("locked"));
    assert!(!body.contains("event: done"));

    // Skipping a node you were never able to reach is also refused.
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{locked_id}/skip"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The available first item CAN be skipped — it stays open, not
    // locked or demonstrated.
    let available_id = items[0]["id"].as_str().unwrap().to_string();
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{available_id}/skip"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let resp: serde_json::Value = serde_json::from_str(&body).unwrap();
    let items = resp["items"].as_array().unwrap();
    assert_eq!(items[0]["state"], serde_json::json!("available"));
}

#[tokio::test]
async fn revisiting_a_generated_node_reads_instead_of_regenerating() {
    // §S5: making already-generated outline items clickable opened a path
    // where regenerating would silently clobber the interaction layer
    // (§4.3's append-only guarantee). This exercises the fix end to end:
    // generate once, skip, read non-destructively, confirm the revisit
    // scheduler surfaces it, then answer and confirm it's cleared and the
    // node reads as done.
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

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    // Generate through to the graded exercise (§S18: one move per request).
    let (status, body) = generate_to_completion(&call, &doc_id, &node0).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("event: done"));

    // Regenerating the same node is refused, not silently overwritten.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "SSE responses are always 200");
    assert!(body.contains("event: error"));
    assert!(body.contains("already generated"));
    assert!(!body.contains("event: done"));

    // The read path shows it without touching anything: an active
    // exercise, not yet demonstrated.
    let (status, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["demonstrated"], serde_json::json!(false));
    assert!(!view["content_html"].as_str().unwrap().is_empty());
    assert!(view["exercise_block_id"].is_string());
    assert!(
        !view["content_html"].as_str().unwrap().contains("<form"),
        "the exercise form must be split out of the prose"
    );

    // Skip it — the revisit scheduler should now suggest it.
    call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/skip"),
        "",
    ))
    .await;
    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/outline"),
        "",
    ))
    .await;
    let outline: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        outline["suggested_revisit"],
        serde_json::json!(node0.clone())
    );

    // Answer it (demo grades as demonstrated) — the suggestion clears,
    // and the read view now shows no active exercise.
    call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/answer"),
        r#"{"answer":"I apply the concept to a new case like this..."}"#,
    ))
    .await;
    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/outline"),
        "",
    ))
    .await;
    let outline: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(outline["suggested_revisit"], serde_json::Value::Null);

    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["demonstrated"], serde_json::json!(true));
    // The id itself outlives demonstration now (an old Q&A thread anchored
    // to the exercise must still resolve on reload) — "no live exercise" is
    // read from `demonstrated`, not from this field going null.
    assert!(view["exercise_block_id"].is_string());

    // Still refused after demonstration — demonstrated is not an
    // exemption from the no-regenerate rule.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("already generated"));
}

/// §S15 item 4: a remediation failure on a `transfer`/synthesis objective
/// should surface the document's existing revisit suggestion explicitly
/// inside the remediation thread — the cheapest diagnostic that already
/// exists (`suggested_revisit`), linked into a place that never referenced
/// it before. Exercised through the real router + demo_responder: the
/// demo `test`-move contract always sets `transfer: true` on its one
/// objective and a blank answer always grades `not_demonstrated`
/// (`api/provider.rs::demo_responder`), so this reaches the real
/// structural-failure branch without any custom mock wiring.
#[tokio::test]
async fn remediation_on_a_transfer_objective_suggests_revisiting_a_skipped_node() {
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

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"calculus"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let intro = created["items"][0]["id"].as_str().unwrap().to_string();
    let intro_title = created["items"][0]["title"].as_str().unwrap().to_string();
    let core = created["items"][1]["id"].as_str().unwrap().to_string();

    // Skip the prerequisite — it becomes the document's revisit
    // suggestion (same mechanism `outline_gates_and_skip`-style tests
    // already pin), and unlocks `core`.
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{intro}/skip"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);

    // Drive `core` to its graded exercise, then fail it with a blank
    // answer — the demo `test`-move objective is `transfer: true`. `"{}"`
    // (an empty structured-answer artifact, not an empty string) is what
    // `demo_responder`'s blank check actually matches
    // (`api/provider.rs`'s `"Student's answer: {}"` pattern).
    generate_to_completion(&call, &doc_id, &core).await;
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{core}/answer"),
        r#"{"answer":"{}"}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let ans: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ans["advance"], serde_json::json!(false));

    let remediation_html = ans["remediation_html"].as_str().unwrap();
    assert!(
        remediation_html.contains("revisit-hint"),
        "expected a revisit hint in the remediation thread: {remediation_html}"
    );
    assert!(
        remediation_html.contains(&intro_title),
        "expected the skipped prerequisite's title ({intro_title}) in the hint: {remediation_html}"
    );
}

#[tokio::test]
async fn practice_is_refused_before_demonstrated_and_regrades_without_relocking() {
    // §S15 item 5: the on-demand practice valve only opens once a node is
    // past its real gate (`Demonstrated`) — calling it earlier would
    // silently replace the node's still-live real check.
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

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    generate_to_completion(&call, &doc_id, &node0).await;

    // Not demonstrated yet — refused.
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/practice"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Demonstrate it for real.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/answer"),
        r#"{"answer":"I apply the concept to a new case like this..."}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let ans: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ans["advance"], serde_json::json!(true));

    // Now practice is allowed: it overwrites the rubric sidecar with a
    // fresh `test` move, served by the SAME `exercise-frame` route the
    // real gate used, unchanged.
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/practice"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, frame_body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}/exercise-frame"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        frame_body.contains("learnive-answer") || frame_body.contains("form"),
        "expected a real sandboxed exercise frame: {frame_body}"
    );

    // Grading a practice attempt is real evidence (§4.3/§7) but must NOT
    // relock the node or route the learner anywhere — `node_states`' rank
    // keeps `Demonstrated` once reached (`events::aggregate::node_state_rank`).
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/answer"),
        r#"{"answer":"I apply the concept to a new case like this..."}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let ans: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ans["advance"], serde_json::json!(true));

    let (status, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/outline"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let outline: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        outline["items"][0]["state"],
        serde_json::json!("demonstrated")
    );
}

#[tokio::test]
async fn abandoned_practice_is_reused_not_regenerated() {
    // §S15 item 5 follow-up: a practice attempt the learner never answered
    // (navigated away mid-round) must not be silently discarded and repaid
    // for on the next "Practice again" click (§12.2 BYOK cost discipline) —
    // the sidecar's `move_id` only gets a `MoveGraded` once it's actually
    // answered, so `practice_node` reuses it instead of generating anew.
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

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    generate_to_completion(&call, &doc_id, &node0).await;
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/answer"),
        r#"{"answer":"I apply the concept to a new case like this..."}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let ans: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ans["advance"], serde_json::json!(true));

    // First "Practice again": the sidecar still holds the original PASSING
    // move, so this generates a fresh `test` move.
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/practice"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Abandoned: click "Practice again" again WITHOUT answering the first
    // practice round. This must reuse the still-ungraded sidecar rather than
    // generating (and paying for) a second one.
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/practice"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let event_log = state.store.event_log(&doc_id).unwrap();
    let test_moves_after_demonstration = event_log
        .iter()
        .unwrap()
        .filter(|e| {
            e.node_id.as_deref() == Some(node0.as_str())
                && matches!(&e.kind, crate::events::EventKind::MoveGenerated { move_type, .. }
                    if move_type == "test")
        })
        .count();
    // One `test` move for the original gate (graded, passing) + exactly one
    // more for the first practice click. The second click must NOT add a third.
    assert_eq!(
        test_moves_after_demonstration, 2,
        "abandoned practice round was regenerated instead of reused"
    );
}

#[tokio::test]
async fn a_partial_node_from_an_interrupted_generation_may_be_retried() {
    // §S6 follow-up: content now persists progressively, one move at a
    // time, so a node file existing is no longer proof the node is done —
    // a dropped connection or a page reload mid-stream leaves exactly this
    // shape behind (some content, no exercise, no `NodeGenerated` event).
    // `prepare`'s regen guard must let a fresh `/generate` call overwrite
    // it rather than refusing forever, or the node would be wedged.
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

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    // Simulate an interrupted attempt: one move's worth of content
    // persisted (`assemble_content_node`, the same shape a progressive
    // write produces), no exercise, no completion event.
    let partial =
        crate::engine::assemble_content_node(&doc_id, &node0, "<p>an interrupted move</p>")
            .unwrap();
    state.store.write_node(&partial).unwrap();

    // Retrying is allowed — not refused as "already generated" — and
    // completes a real node this time (§S18: possibly across several
    // per-move requests).
    let (status, body) = generate_to_completion(&call, &doc_id, &node0).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("already generated"), "{body}");
    assert!(body.contains("event: done"));

    // Now that it's genuinely finalized (has an exercise, `NodeGenerated`
    // fired), a further retry IS refused — the guard still protects a
    // real, complete node.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("already generated"));
}

/// §14 resilience (PLAN.md "Geração de nó não é resiliente a provider
/// lento"): a node whose `explain` move succeeded and persisted, but whose
/// FOLLOWING move then failed (e.g. `test` exceeding `COMPLETE_BUDGET`), must
/// resume from the persisted `explain` on retry — not regenerate (and
/// re-pay for) it. This simulates exactly that shape: one `MoveGenerated`
/// event logged for `explain`, matching progressively-persisted content on
/// disk, and nothing else — the state `generate_node` itself would have
/// left behind right after `explain` settles and right before a failed
/// `test` call returns (a failed generation call never reaches the event
/// append at all, `resumed_ungraded_moves`'s doc comment).
#[tokio::test]
async fn node_generation_resumes_after_an_interrupted_move() {
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

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    // The exact persistence shape `generate_node`'s loop leaves after
    // `explain` (move index 0) settles: tagged, wrapped, written.
    const MARKER: &str = "PRIOR EXPLAIN CONTENT marker-9f2";
    let tagged = crate::engine::tag_move_html(&node0, 0, &format!("<p>{MARKER}</p>"));
    let partial = crate::engine::assemble_partial_node(&doc_id, &node0, &tagged).unwrap();
    state.store.write_node_content(&partial).unwrap();

    let event_log = state.store.event_log(&doc_id).unwrap();
    event_log
        .append(
            Some(&node0),
            crate::events::EventKind::MoveGenerated {
                move_id: crate::engine::new_id(),
                move_type: "explain".to_string(),
                tactics: vec![],
                rung: "L0".to_string(),
            },
        )
        .unwrap();

    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("event: error"), "unexpected error:\n{body}");
    assert!(body.contains("event: exercise"));
    assert!(body.contains("event: done"));

    // Only ONE `explain` was ever logged — L0's move sequence is exactly
    // [explain, test] (`movement::l0_next_move`), so a resumed retry that
    // reused the persisted `explain` goes straight to `test`; one that
    // regenerated from scratch would log a second `explain`.
    let explain_moves = event_log
        .iter()
        .unwrap()
        .filter(|e| {
            matches!(&e.kind, crate::events::EventKind::MoveGenerated { move_type, .. }
                if move_type == "explain")
        })
        .count();
    assert_eq!(explain_moves, 1, "explain was regenerated, not resumed");

    // The persisted `explain` prose survived verbatim into the finalized
    // node — proof its content, not just its event, was carried forward.
    let reloaded = state.store.read_node(&doc_id, &node0).unwrap();
    assert!(
        reloaded.content.html.contains(MARKER),
        "resumed explain content missing from finalized node:\n{}",
        reloaded.content.html
    );
}

#[tokio::test]
async fn asking_works_against_a_node_still_mid_generation() {
    // §S6 follow-up, the actual point of the whole feature: `/ask` must
    // work against a node's progressively-persisted partial content, not
    // only after the whole node (through the graded move) has finished.
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

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    // A partial node — one move settled, no exercise, no `NodeGenerated`
    // event — the exact shape a still-streaming `/generate` request leaves
    // on disk after its first `move_settled` write, well before `done`.
    let partial = crate::engine::assemble_content_node(
        &doc_id,
        &node0,
        "<p>fractions split a whole into equal parts</p>",
    )
    .unwrap();
    state.store.write_node(&partial).unwrap();
    let block_id = partial.content.blocks[0].id.clone();

    // `GET` already reads it — no exercise yet, but real content.
    let (status, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(view["exercise_block_id"].is_null());
    assert!(
        view["content_html"]
            .as_str()
            .unwrap()
            .contains("equal parts")
    );

    // Asking about that partial content succeeds — the whole node need
    // not exist yet, only the block being asked about.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/ask"),
        &format!(r#"{{"question":"which parts?","anchor":{{"block_id":"{block_id}"}}}}"#),
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The interaction landed, and the partial content layer survived the
    // append untouched (`write_node_content`'s interaction-preservation).
    let reloaded = state.store.read_node(&doc_id, &node0).unwrap();
    assert_eq!(reloaded.interaction.len(), 1);
    assert!(reloaded.content.html.contains("equal parts"));
}

#[tokio::test]
async fn asking_mid_pause_does_not_desync_the_next_move_loop_request() {
    // §S18 regression, live-caught 2026-08-21: `ask_question` logs its own
    // `MoveGenerated{move_type: "respond"}` event under the SAME node_id as
    // the node it's anchored inside. Before the fix, `resumed_move_index`/
    // `resumed_ungraded_moves` (api/reading.rs) counted that event as one of
    // THIS node's own move-loop slots, so a question asked between a
    // `move_paused` and the next `/generate` call desynced the resumed
    // index — L0's fixed [explain, test] rule (`movement::l0_next_move`)
    // then saw a phantom 3rd move and errored "no next move: this node's
    // moves are already complete", permanently stalling the node before its
    // graded exercise ever generated. This pins the fix: asking mid-pause
    // must not stop the node from reaching `done`.
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

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    // First per-move request: one real move (`explain`) settles, then the
    // node pauses — the exact window `/ask` can land in under §S18.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("event: move_paused"),
        "expected the node to pause after its first move: {body}"
    );
    assert!(
        !body.contains("event: done"),
        "node finished in one move: {body}"
    );

    // Pull the block id from the (already-unescaped) persisted content
    // rather than the raw SSE bytes, which JSON-escape the quotes.
    let (status, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    let block_id = view["content_html"]
        .as_str()
        .unwrap()
        .split("data-block-id=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("a real block id in the paused move's content")
        .to_string();

    // Ask a question anchored in that first move's content while the node
    // sits paused — this is what used to desync the resume reconstruction.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/ask"),
        &format!(r#"{{"question":"why does this matter?","anchor":{{"block_id":"{block_id}"}}}}"#),
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Resume generation — must continue past the `respond` event, not error
    // out, and must still eventually reach a graded exercise.
    let (status, body) = generate_to_completion(&call, &doc_id, &node0).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("event: error"), "unexpected error:\n{body}");
    assert!(
        body.contains("event: exercise"),
        "no exercise generated:\n{body}"
    );
    assert!(body.contains("event: done"), "node never finished:\n{body}");

    let (status, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(view["exercise_block_id"].is_string());
}

#[tokio::test]
async fn csp_default_is_strict_but_exercise_frame_gets_its_own() {
    // §S10: `security::guard` now only inserts the default strict CSP
    // when a handler hasn't already set one, so `exercise_frame`'s own
    // permissive CSP (needed for its inline harness `<script>`) survives.
    // That "insert only if absent" logic has two failure directions with
    // no other coverage: a normal route silently losing the strict
    // default, or `exercise_frame` silently losing its override to the
    // default. Pin both.
    let state = test_state();
    let call = |req: Request<Body>| {
        let state = state.clone();
        async move { build_router(state).oneshot(req).await.unwrap() }
    };

    let resp = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    // The generate response is an SSE stream that only actually runs
    // (and writes the `.rubric.json` sidecar) once its body is drained —
    // and now settles at most one move per request (§S18), so drive it to
    // its graded exercise before checking anything downstream.
    loop {
        let resp = call(authed(
            "POST",
            &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
            "",
        ))
        .await;
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        if body.contains("event: done") || body.contains("event: error") {
            break;
        }
    }

    // A normal API route: the default strict app CSP.
    let resp = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    let default_csp = resp
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .expect("default CSP present")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        default_csp.contains("default-src 'self'") && !default_csp.contains("form-action"),
        "app default CSP must stay the strict app-wide default: {default_csp}"
    );

    // The exercise frame: its own, distinct, permissive CSP — not the
    // default, and not silently missing either.
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/documents/{doc_id}/nodes/{node0}/exercise-frame?token={TOKEN}"
        ))
        .header("host", HOST)
        .body(Body::empty())
        .unwrap();
    let resp = call(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let frame_csp = resp
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .expect("exercise-frame CSP present")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        frame_csp.contains("default-src 'none'") && frame_csp.contains("form-action 'none'"),
        "exercise frame needs its own, distinct CSP override: {frame_csp}"
    );
    assert_ne!(default_csp, frame_csp);
}

/// Parses a raw SSE response body and concatenates every `token` event's
/// (JSON-decoded) data, in order — the client's own accumulation logic,
/// reproduced for assertions. A single word/phrase can straddle a
/// provider chunk boundary (`MockProvider` tokenizes by word), so tests
/// must check the reassembled text, not the raw multiplexed SSE bytes.
fn collect_sse_tokens(sse_body: &str) -> String {
    let mut out = String::new();
    let mut lines = sse_body.lines().peekable();
    while let Some(line) = lines.next() {
        if line == "event: token"
            && let Some(data_line) = lines.next()
            && let Some(json) = data_line.strip_prefix("data: ")
            && let Ok(text) = serde_json::from_str::<String>(json)
        {
            out.push_str(&text);
        }
    }
    out
}

#[tokio::test]
async fn interactive_island_never_leaks_raw_script_but_is_served_sandboxed() {
    // §S11: the model opens an interactive island mid-prose
    // (`<figure data-interactive>…</figure>`) in a streamed move. End to
    // end: the live SSE `token` stream must never carry its raw
    // `<script>`; the persisted, redacted `content_html` must keep an
    // empty placeholder at the same `data-block-id`; and that block's
    // own sandboxed frame must serve the real content.
    use crate::ai::{Ai, MockProvider, Models, Provider};

    let scripted = Provider::Mock(MockProvider::scripted(|req| {
        let text = req
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if text.contains("<!--tactics:") {
            "<p>Before the island.</p>\n\
             <figure data-interactive><script>parent.postMessage({type:'ping'},'*')</script></figure>\n\
             <p>After the island.</p>\n\
             <!--tactics: worked-example-->"
                .to_string()
        } else {
            crate::api::demo_responder(req)
        }
    }));
    let ai = Ai::new(scripted, Models::single("island-demo"));

    let state = test_state_with_ai(ai);
    let call = |req: Request<Body>| {
        let state = state.clone();
        async move { build_router(state).oneshot(req).await.unwrap() }
    };

    let resp = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    let resp = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let sse_body = String::from_utf8_lossy(&bytes).into_owned();

    // The live stream must never carry the raw script, but must carry an
    // empty island placeholder somewhere in its `token` frames. Provider
    // chunks are word-sized (`MockProvider`), so reassemble the tokens
    // before checking for a multi-word phrase.
    assert!(!sse_body.contains("postMessage({type:'ping'}"));
    let streamed = collect_sse_tokens(&sse_body);
    assert!(!streamed.contains("postMessage({type:'ping'}"));
    assert!(streamed.contains("data-interactive"));
    assert!(streamed.contains("data-block-id"));
    assert!(streamed.contains("isl-"));
    assert!(streamed.contains("Before the island."));
    assert!(streamed.contains("After the island."));

    // The persisted, redacted read view: same placeholder, still no
    // script, still both surrounding paragraphs.
    let resp = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let view: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let content_html = view["content_html"].as_str().unwrap();
    assert!(!content_html.contains("postMessage({type:'ping'}"));
    assert!(!content_html.contains("<script"));
    assert!(content_html.contains("Before the island."));
    assert!(content_html.contains("After the island."));

    let id_start = content_html.find(r#"data-block-id="isl-"#).unwrap();
    let quote_start = id_start + r#"data-block-id=""#.len();
    let quote_end = content_html[quote_start..].find('"').unwrap() + quote_start;
    let block_id = &content_html[quote_start..quote_end];

    // The block's own sandboxed frame serves the real content — this is
    // the ONLY place the raw script is ever allowed to appear.
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/api/documents/{doc_id}/nodes/{node0}/blocks/{block_id}/frame?token={TOKEN}"
        ))
        .header("host", HOST)
        .body(Body::empty())
        .unwrap();
    let resp = call(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let frame_body = String::from_utf8_lossy(&bytes).into_owned();
    assert!(frame_body.contains("postMessage({type:'ping'}"));
}

#[tokio::test]
async fn outline_proposal_is_a_hard_error_on_unparseable_model_output() {
    // S27e: `propose_outline` now asks for a reading list whose LAST
    // element — the work most directly covering the objective — is never
    // optional, same "no safe empty default" reasoning the pre-pivot
    // concept-tree contract this replaced already established. A model
    // that answers with prose instead of JSON despite the contract must
    // surface as a hard error instead of silently becoming an empty list.
    use crate::ai::{Ai, MockProvider, Models, Provider};

    let scripted = Provider::Mock(MockProvider::scripted(|req| {
        let text = req
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if text.contains("propose the initial READING LIST") {
            // No JSON at all — the same shape as a stream whose only
            // content was reasoning prose, never a final tree.
            "I don't think this objective needs any structure.".to_string()
        } else {
            crate::api::demo_responder(req)
        }
    }));
    let ai = Ai::new(scripted, Models::single("outline-parse-failure-demo"));
    let state = test_state_with_ai(ai);

    let resp = build_router(state)
        .oneshot(authed(
            "POST",
            "/api/outline/propose",
            r#"{"topic":"o que é epistemologia?","objective_text":"Clarify the conditions that differentiate knowledge from belief."}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn reading_interactions_ask_annotate_and_read_to_end() {
    // §S6: selection→question / question-on-the-line (both go through
    // `/ask`, the only difference is whether the client's anchor carries
    // a quote), annotation, and the scroll-to-end signal — all modeled
    // as events (§7.1) and, for ask/annotate, as append-only interaction
    // items woven into the document (§9 "the document is the answer").
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

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;

    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    let content_html = view["content_html"].as_str().unwrap();
    let block_id = content_html
        .split("data-block-id=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("at least one frozen block")
        .to_string();

    // Question-on-the-line: no quote, just the reading line's block.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/ask"),
        &format!(r#"{{"question":"why <does> this work?","anchor":{{"block_id":"{block_id}"}}}}"#),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let ask: serde_json::Value = serde_json::from_str(&body).unwrap();
    let ask_html = ask["body_html"].as_str().unwrap();
    assert!(ask_html.contains("You asked:"));
    // The user's raw question text is escaped, never stored as live HTML.
    assert!(ask_html.contains("&lt;does&gt;"));
    assert!(!ask_html.contains("<does>"));

    // An unresolvable anchor is refused, not silently stored.
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/ask"),
        r#"{"question":"huh?","anchor":{"block_id":"not-a-real-block"}}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A blank question is refused.
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/ask"),
        &format!(r#"{{"question":"   ","anchor":{{"block_id":"{block_id}"}}}}"#),
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Annotation: the user's own note, escaped, anchored.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/annotate"),
        &format!(r#"{{"body":"<b>remember this</b>","anchor":{{"block_id":"{block_id}"}}}}"#),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let annotate: serde_json::Value = serde_json::from_str(&body).unwrap();
    let annotate_html = annotate["body_html"].as_str().unwrap();
    assert!(annotate_html.contains("&lt;b&gt;remember this&lt;/b&gt;"));

    // Scroll-to-end: a pure signal, always accepted.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/read-to-end"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"ok": true})
    );

    // Both the question and the annotation landed in the interaction
    // layer, readable back on the node.
    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    let kinds: Vec<&str> = view["interactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"qa"));
    assert!(kinds.contains(&"annotation"));

    // None of this is reachable on a node that was never generated
    // (locked, no content layer to anchor against yet).
    let node1 = created["items"][1]["id"].as_str().unwrap().to_string();
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node1}/ask"),
        r#"{"question":"early?","anchor":{"block_id":"x"}}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// §S17 regression: `Respond`'s inline `/ask` answer lands in the
/// interaction layer (`body_html`), which never passes through
/// `assemble_node`/`assemble_content_node` — so unlike every content-layer
/// move, `render_math` isn't picked up for free at assembly and has to run
/// explicitly at the `/ask` call site. Caught by review, not by the test
/// suite, when `S17` first unified this path onto the move ABI and dropped
/// the call that used to live in the deleted `engine::answer_question`.
#[tokio::test]
async fn ask_answer_renders_math_before_storing() {
    use crate::ai::{Ai, MockProvider, Models, Provider};

    let scripted = Provider::Mock(MockProvider::scripted(|req| {
        let text = req
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if text.contains("<!--tactics:") {
            return "<p>Half is $\\frac{1}{2}$ of the whole.</p>\n<!--tactics: analogy-->"
                .to_string();
        }
        crate::api::demo_responder(req)
    }));
    let ai = Ai::new(scripted, Models::single("ask-math-demo"));
    let state = test_state_with_ai(ai);
    let call = |req: Request<Body>| {
        let state = state.clone();
        async move {
            let resp = build_router(state).oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;

    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    let content_html = view["content_html"].as_str().unwrap();
    let block_id = content_html
        .split("data-block-id=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("at least one frozen block")
        .to_string();

    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/ask"),
        &format!(r#"{{"question":"why half?","anchor":{{"block_id":"{block_id}"}}}}"#),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let ask: serde_json::Value = serde_json::from_str(&body).unwrap();
    let ask_html = ask["body_html"].as_str().unwrap();
    assert!(
        ask_html.contains("<math"),
        "expected rendered MathML in stored answer, got: {ask_html}"
    );
    // The literal `$...$` delimiters are gone (MathML's own `<annotation>`
    // still carries the raw LaTeX as text, per `render_math`'s contract).
    assert!(!ask_html.contains("$\\frac"));
}

/// §S7 — evidence profile: a node closing (demonstrated) is the "fechar
/// nó" distillation trigger (§7.1). Exercises the whole chain through the
/// real router + demo_responder (which has a dedicated branch for the
/// distillation prompt, same as every other move type): the always-fresh
/// evidence table (0 LLM tokens), the rare distillation call persisting
/// traits/hypotheses to `profile.json`, GET reflecting them, and POST
/// editing them (§7.1 "inspecionável/editável") without resetting the
/// ~30-event distillation threshold a manual edit must not touch.
#[tokio::test]
async fn profile_evidence_and_distillation_flow_in_demo_mode() {
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

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    // Before any node closes, the profile endpoint still works — just empty.
    let (status, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/profile"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let profile: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(profile["evidence"], serde_json::json!(""));
    assert!(profile["traits"].as_array().unwrap().is_empty());

    // Generate + answer node0 for real (demo grades non-blank content as
    // demonstrated) — the node closing is the distillation trigger. §S18:
    // one move per request, so drive it to its graded exercise first.
    generate_to_completion(&call, &doc_id, &node0).await;
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/answer"),
        r#"{"answer":"I apply the concept to a new case like this..."}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let ans: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ans["advance"], serde_json::json!(true));

    // Distillation is now backgrounded (§14: never block answer→advance,
    // the hottest transition in the app, on a second LLM call) — give the
    // spawned task a chance to run before checking its effect.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The evidence table now carries the graded test move's tactic
    // (0-token Rust fold); the rare distillation ran and persisted.
    let (status, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/profile"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let profile: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        profile["evidence"]
            .as_str()
            .unwrap()
            .contains("worked-example: 1 demonstrated"),
        "evidence table: {profile}"
    );
    assert!(!profile["traits"].as_array().unwrap().is_empty());
    assert!(!profile["hypotheses"].as_array().unwrap().is_empty());
    let distilled_through = profile["distilled_through"].as_u64().unwrap();
    assert!(distilled_through > 0);

    // The learner edits the profile directly (§7.1 human-in-the-loop) —
    // the ~30-event distillation threshold must NOT reset on a manual edit.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/profile"),
        r#"{"traits":["user-authored trait"],"hypotheses":[]}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let edited: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(edited["traits"], serde_json::json!(["user-authored trait"]));
    assert!(edited["hypotheses"].as_array().unwrap().is_empty());
    assert_eq!(
        edited["distilled_through"].as_u64().unwrap(),
        distilled_through,
        "a user edit must not reset the distillation threshold"
    );
}

#[tokio::test]
async fn objective_and_plan_endpoints_work_in_demo_mode() {
    // §S4, exercised through the real router + demo_responder — the
    // out-of-box keyless path, not just a mocked engine call.
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

    // 1. Propose (stateless) — demo_responder's "cold start of a living
    // curriculum" branch.
    let (status, body) = call(authed(
        "POST",
        "/api/objective/propose",
        r#"{"topic":"fractions"}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let proposal: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(!proposal["text"].as_str().unwrap().is_empty());

    // 2. Confirm — locks objective version 1. Explicit, already-confirmed
    // nodes (as the real client always sends), a single flat top-level item
    // with no children — this test's `plan/decide` step below asserts an
    // exact title list, which a decomposed node's preserved sub-items would
    // otherwise pad out.
    let (status, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"fractions","objective_text":"Learn to add and subtract fractions","nodes":[{"id":"n0","title":"Fractions","action":"learn","children":[]}]}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();

    let obj_json = state
        .store
        .read_doc_file(&doc_id, "objective.json")
        .unwrap();
    let log: crate::objective::ObjectiveLog = serde_json::from_str(&obj_json).unwrap();
    assert_eq!(log.versions.len(), 1);
    assert_eq!(log.current().unwrap().version, 1);
    assert_eq!(
        log.current().unwrap().source,
        crate::objective::ObjectiveSource::ColdStart
    );

    // 3. User-initiated revision — a second, non-destructive version.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/objective"),
        r#"{"text":"Learn to add, subtract, and simplify fractions"}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let revised: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(revised["version"], serde_json::json!(2));

    let obj_json = state
        .store
        .read_doc_file(&doc_id, "objective.json")
        .unwrap();
    let log: crate::objective::ObjectiveLog = serde_json::from_str(&obj_json).unwrap();
    assert_eq!(log.versions.len(), 2, "v1 must still be present");
    assert_eq!(
        log.versions[0].text, "Learn to add and subtract fractions",
        "v1 unchanged"
    );
    assert_eq!(
        log.current().unwrap().source,
        crate::objective::ObjectiveSource::UserEdit
    );

    // 4. A `plan` proposal (synthesized directly — the demo provider
    // never proposes a structural change) resolved via `/plan/decide`.
    let proposal = serde_json::json!({
        "move_id": "mv1",
        "node_id": "n0",
        "html": "<p>rationale</p>",
        "proposed": ["Introduction", "Adding fractions", "Subtracting fractions"],
        "resolved": false,
    });
    state
        .store
        .write_doc_file(&doc_id, "outline.proposal.json", &proposal.to_string())
        .unwrap();

    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/plan/decide"),
        r#"{"approve":true}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let resp: serde_json::Value = serde_json::from_str(&body).unwrap();
    let titles: Vec<&str> = resp["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        vec!["Introduction", "Adding fractions", "Subtracting fractions"]
    );

    // Replaying the same (now-resolved) proposal must be rejected, not
    // silently re-applied.
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/plan/decide"),
        r#"{"approve":false}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// §S8: the demo responder always answers inline (no real signal to
// justify a new section) — force a spawn decision for these tests via a
// scripted `Ai` that special-cases `decide_ask_response`'s prompt and
// otherwise falls through to the real demo responder.
fn spawn_responder(req: &crate::ai::ChatRequest) -> String {
    let text = req
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if text.contains("Decide how to answer it: INLINE") {
        return r#"{"spawn":true,"title":"Fractions in depth"}"#.to_string();
    }
    crate::api::demo_responder(req)
}

fn spawn_ai() -> crate::ai::Ai {
    crate::ai::Ai::new(
        crate::ai::Provider::Mock(crate::ai::MockProvider::scripted(spawn_responder)),
        crate::ai::Models::single("demo"),
    )
}

#[tokio::test]
async fn asking_a_question_that_warrants_depth_spawns_a_real_subnode() {
    // §S8, extended by §S15: the tutor can decide a question needs a real
    // new section rather than a short inline reply — a real, versioned,
    // revisitable `Node`, present in `outline.json`, parented to the node
    // that spawned it, and now ALSO shown in the sidebar tree as its child
    // (§S15 unified `parent_id` into the universal tree/sidebar pointer).
    let state = test_state_with_ai(spawn_ai());
    let call = |req: Request<Body>| {
        let state = state.clone();
        async move {
            let resp = build_router(state).oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;

    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    let block_id = view["content_html"]
        .as_str()
        .unwrap()
        .split("data-block-id=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("at least one frozen block")
        .to_string();

    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/ask"),
        &format!(
            r#"{{"question":"how does this generalize to a harder case?","anchor":{{"block_id":"{block_id}"}}}}"#
        ),
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let ask: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ask["kind"], "spawn");
    assert_eq!(ask["title"], "Fractions in depth");
    let sub_id = ask["node_id"].as_str().unwrap().to_string();
    assert_ne!(sub_id, node0);
    assert!(!ask["content_html"].as_str().unwrap().is_empty());
    assert_eq!(ask["anchor_block"], block_id);

    // The sub-node is a real, independently fetchable node.
    let (status, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{sub_id}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let sub_view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(sub_view["title"], "Fractions in depth");
    assert!(!sub_view["content_html"].as_str().unwrap().is_empty());
    // Prose-only in this slice: no exercise/gate on a spawned sub-node.
    assert!(sub_view["exercise_block_id"].is_null());

    // Present in outline.json (parented, no prerequisites of its own —
    // §S15: never a main-graph gate).
    let outline_json = state.store.read_doc_file(&doc_id, "outline.json").unwrap();
    let outline: crate::engine::Outline = serde_json::from_str(&outline_json).unwrap();
    let sub_item = outline
        .items
        .iter()
        .find(|i| i.id == sub_id)
        .expect("sub-node must be in outline.json");
    assert_eq!(sub_item.parent_id.as_deref(), Some(node0.as_str()));
    assert!(sub_item.prerequisites.is_empty());

    // §S15: shown in the sidebar tree, nested under the node that spawned
    // it — `parent_id` now decides shape (sidebar nesting), never gating.
    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/outline"),
        "",
    ))
    .await;
    let sidebar: serde_json::Value = serde_json::from_str(&body).unwrap();
    let sidebar_items = sidebar["items"].as_array().unwrap();
    let sub_view = sidebar_items
        .iter()
        .find(|i| i["id"].as_str() == Some(sub_id.as_str()))
        .expect("sub-node must be in the sidebar tree (§S15)");
    assert_eq!(sub_view["parent_id"], node0.as_str());

    // The parent's own interaction layer references the child, not a
    // woven-in-place answer.
    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    let parent_view: serde_json::Value = serde_json::from_str(&body).unwrap();
    let qa = parent_view["interactions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "qa" && i["child_node_id"] == sub_id)
        .expect("parent must carry a qa thread pointing at the spawned sub-node");
    assert_eq!(qa["anchor_block"], block_id);
}

// §S13: `decide_move` at L1 offers `research` only on the menu built from
// `MoveContext`'s own text — check for it the same way the real prompt
// does (`, research`), so this test breaks if the menu wording ever
// changes, rather than silently drifting from what it claims to force.
fn research_once_responder(req: &crate::ai::ChatRequest) -> String {
    let text = req
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if text.contains("choosing the next move") && text.contains(", research") {
        return r#"{"move_type":"research","rationale":"test: force the research branch"}"#
            .to_string();
    }
    crate::api::demo_responder(req)
}

/// §S13: the `research` move was, until this test, verified only by code
/// review — both live trials in the session that built it had cold-start
/// grounding arrive before generation started, so `decide_move` correctly
/// never offered it and the branch in `generate_node` (acquire → emit two
/// status frames → loop back, never `render()`) never actually ran. L0
/// (this suite's default policy) never offers `research` either — its
/// fixed rule doesn't know the move exists — so this forces L1 and scripts
/// `decide_move`'s very first call to pick it, then lets the real
/// `demo_responder` drive the rest of the node once `research_attempted`
/// withdraws the option from the menu (`movement::prompt::menu`, covered
/// unit-level by `research_offered_only_when_ungrounded_and_unattempted`).
///
/// Rewritten 2026-08-29 for S27m: `test_state_with_ai` now seeds a real
/// (mock-embedder) retriever plus the two demo library PDFs, so a document
/// created via the DEFAULT `/api/documents {"topic":...}` cold-start path
/// is bibliographically sourced (`demo_responder`'s reading list) — its
/// node carries a real `source` pointer and `ground_node` grounds it for
/// real, so `research` is legitimately never offered on its menu again.
/// The old comment's premise ("tests never construct a real retriever") no
/// longer holds. This test's actual subject was always the `research` move
/// itself, not the reading-list flow, so it now supplies `nodes` directly
/// (the pre-S27e/direct-API shape, `OutlineItemType::Node` default, no
/// `source`) to get a genuinely ungrounded node without disabling the
/// retriever every other test now depends on. `state.source`/
/// `fallback_source` are swapped to `Source::Unconfigured` so (a) the
/// explicit `research` move's own `acquire()` call has something to
/// legitimately fail against, matching "no adequate source found" below,
/// and (b) `create_document`'s unconditional background `spawn_acquisition`
/// can't race in and ground the node itself first via `Source::Mock`
/// (which — unlike the old `retriever: None` no-op — would otherwise
/// sometimes succeed now that a real retriever backs it).
#[tokio::test]
async fn research_move_acquires_then_resumes_the_real_move_loop() {
    use crate::ai::{Ai, MockProvider, Models, Provider};
    use crate::movement::AgentPolicy;

    let ai = Ai::new(
        Provider::Mock(MockProvider::scripted(research_once_responder)),
        Models::single("demo"),
    );
    let mut state = test_state_with_ai(ai);
    state.source = Arc::new(Source::Unconfigured);
    state.fallback_source = Arc::new(Source::Unconfigured);
    state.policy.store(Arc::new(AgentPolicy::L1));
    let call = |req: Request<Body>| {
        let state = state.clone();
        async move {
            let resp = build_router(state).oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };

    let (_, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"fractions","nodes":[{"id":"n1","title":"Fractions","action":"learn"}]}"#,
    ))
    .await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    // §S18: research still resolves within its own request (the loop-back
    // to a real decision happens in-process, before that request's one real
    // move settles and ends the stream) — but the node as a whole may now
    // take several per-move requests to reach its graded check.
    let (status, body) = generate_to_completion(&call, &doc_id, &node0).await;
    assert_eq!(status, StatusCode::OK);

    // Both research status frames reached the client...
    assert_eq!(
        body.matches("event: research").count(),
        2,
        "expected exactly two research status frames:\n{body}"
    );
    assert!(body.contains("Looking for sources"));
    assert!(
        body.contains("No adequate source found"),
        "state.source/fallback_source are Source::Unconfigured in this test, \
         so acquisition must report failure, not silently claim a source: {body}"
    );
    // ...and the loop still closed in a graded check afterward, despite
    // research eating one of MAX_MOVES_PER_NODE's four slots.
    assert!(body.contains("event: exercise"));
    assert!(body.contains("event: done"));
    assert!(!body.contains("event: error"), "unexpected error:\n{body}");

    // Exactly one research move was logged — never a second, off-menu pick:
    // `research_attempted` withheld it from every later `decide_move` call
    // in this same node.
    let event_log = state.store.event_log(&doc_id).unwrap();
    let research_moves = event_log
        .iter()
        .unwrap()
        .filter(|e| {
            matches!(&e.kind, crate::events::EventKind::MoveGenerated { move_type, .. }
                if move_type == "research")
        })
        .count();
    assert_eq!(research_moves, 1);

    // The node still closed with real content and an active exercise —
    // research burning a slot didn't leave the node half-built.
    let (status, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["demonstrated"], serde_json::json!(false));
    assert!(view["exercise_block_id"].is_string());
    assert!(!view["content_html"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn plan_approval_preserves_spawned_subnodes() {
    // §S8/§5: a `plan` move only ever proposes titles for the main line
    // — rebuilding `outline.json` from those titles must not silently
    // drop a sub-node spawned from a question, which is never among them.
    let state = test_state_with_ai(spawn_ai());
    let call = |req: Request<Body>| {
        let state = state.clone();
        async move {
            let resp = build_router(state).oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();
    let node0_title = created["items"][0]["title"].as_str().unwrap().to_string();

    call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/nodes/{node0}"),
        "",
    ))
    .await;
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    let block_id = view["content_html"]
        .as_str()
        .unwrap()
        .split("data-block-id=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap()
        .to_string();

    let (_, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/ask"),
        &format!(r#"{{"question":"go deeper please","anchor":{{"block_id":"{block_id}"}}}}"#),
    ))
    .await;
    let ask: serde_json::Value = serde_json::from_str(&body).unwrap();
    let sub_id = ask["node_id"].as_str().unwrap().to_string();

    // Reuses node0's own title so the plan's rebuild keeps its id (the
    // model only signals a rename by an exact title match, §5's own
    // documented limit) — the point under test is the sub-node, not id
    // churn on the main line.
    let proposal = serde_json::json!({
        "move_id": "mv1",
        "node_id": node0,
        "html": "<p>rationale</p>",
        "proposed": [node0_title, "A new second concept"],
        "resolved": false,
    });
    state
        .store
        .write_doc_file(&doc_id, "outline.proposal.json", &proposal.to_string())
        .unwrap();

    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/plan/decide"),
        r#"{"approve":true}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);

    let outline_json = state.store.read_doc_file(&doc_id, "outline.json").unwrap();
    let outline: crate::engine::Outline = serde_json::from_str(&outline_json).unwrap();
    assert_eq!(
        outline.items.iter().filter(|i| i.id == sub_id).count(),
        1,
        "the spawned sub-node must survive a plan rebuild it was never part of"
    );
    let sub_item = outline.items.iter().find(|i| i.id == sub_id).unwrap();
    assert_eq!(sub_item.parent_id.as_deref(), Some(node0.as_str()));

    let main_line: Vec<&str> = outline
        .items
        .iter()
        .filter(|i| i.parent_id.is_none())
        .map(|i| i.title.as_str())
        .collect();
    assert_eq!(
        main_line,
        vec![node0_title.as_str(), "A new second concept"]
    );
}

/// §S15b — shared node, steps 1+2 (`source_doc_id`/`owner_of` + write
/// convergence): the acceptance criterion from PLAN.md is literal — the
/// same node appears in two documents, and a question/annotation made from
/// either is visible from the other. This drives the real router for both
/// documents against the SAME `AppState`/store, exactly the shape a
/// two-tab session would produce, and never touches the cross-document
/// embedding matcher (`propose_prerequisites`) — the `known` pointer is
/// crafted directly in the `POST /api/documents` body, the same shape
/// `resolve_outline_node` would have produced, so this test is independent
/// of retrieval/embeddings being configured.
#[tokio::test]
async fn a_referenced_node_converges_qa_and_annotations_across_documents() {
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

    // Owner document: one node, generated and demonstrated for real.
    let (_, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"algebra basics"}"#,
    ))
    .await;
    let owner_doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let owner_doc_id = owner_doc["doc_id"].as_str().unwrap().to_string();
    let shared_node_id = owner_doc["items"][0]["id"].as_str().unwrap().to_string();

    generate_to_completion(&call, &owner_doc_id, &shared_node_id).await;
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{owner_doc_id}/nodes/{shared_node_id}/answer"),
        r#"{"answer":"I apply the concept to a new case like this..."}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let ans: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ans["advance"], serde_json::json!(true));

    // Visiting document: confirms the SAME node as a `skip`+`known`
    // reference instead of generating a local one — same wire shape
    // `resolve_outline_node`'s `KnownMatch` produces, hand-built here since
    // no embedder is configured in the test harness.
    let visit_body = serde_json::json!({
        "topic": "integration",
        "nodes": [
            {
                "id": "prereq-slot",
                "title": "Algebra basics",
                "action": "skip",
                "known": {
                    "doc_id": owner_doc_id,
                    "doc_name": "Algebra basics",
                    "node_id": shared_node_id,
                },
                "children": [],
            },
            {
                "id": "obj-slot",
                "title": "Integration",
                "action": "learn",
                "children": [],
            },
        ],
    });
    let (status, body) = call(authed("POST", "/api/documents", &visit_body.to_string())).await;
    assert_eq!(status, StatusCode::OK, "create_document failed: {body}");
    let visit_doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let visit_doc_id = visit_doc["doc_id"].as_str().unwrap().to_string();

    // The reference materialized under the SAME id as the owner's real
    // node — not a freshly-minted local id — which is the whole point: one
    // node, one identity, two outlines.
    let visited_ids: Vec<&str> = visit_doc["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert!(
        visited_ids.contains(&shared_node_id.as_str()),
        "expected the reference to carry the owner's real node id, got {visited_ids:?}"
    );

    // Reading the reference from the visiting document resolves to the
    // owner's real content, not an empty/local stub.
    let (status, owner_view) = call(authed(
        "GET",
        &format!("/api/documents/{owner_doc_id}/nodes/{shared_node_id}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, visited_view) = call(authed(
        "GET",
        &format!("/api/documents/{visit_doc_id}/nodes/{shared_node_id}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let owner_json: serde_json::Value = serde_json::from_str(&owner_view).unwrap();
    let visited_json: serde_json::Value = serde_json::from_str(&visited_view).unwrap();
    assert_eq!(
        owner_json["content_html"], visited_json["content_html"],
        "the reference must resolve to the SAME node file as the owner"
    );
    let block_id = owner_json["content_html"]
        .as_str()
        .unwrap()
        .split("data-block-id=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("a real block id in the demonstrated node's content")
        .to_string();

    // A question asked from the VISITING document must land on the
    // owner's interaction layer, visible when the OWNER reads the node.
    let (status, ask_body) = call(authed(
        "POST",
        &format!("/api/documents/{visit_doc_id}/nodes/{shared_node_id}/ask"),
        &format!(
            r#"{{"question":"why does this step work?","anchor":{{"block_id":"{block_id}"}}}}"#
        ),
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{ask_body}");

    // An annotation from the OWNER must be visible reading through the
    // VISITING reference too (both directions, per the acceptance
    // criterion: "idem para anotações").
    let (status, annotate_body) = call(authed(
        "POST",
        &format!("/api/documents/{owner_doc_id}/nodes/{shared_node_id}/annotate"),
        &format!(r#"{{"body":"note to self","anchor":{{"block_id":"{block_id}"}}}}"#),
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{annotate_body}");

    let (status, owner_after) = call(authed(
        "GET",
        &format!("/api/documents/{owner_doc_id}/nodes/{shared_node_id}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let owner_after_json: serde_json::Value = serde_json::from_str(&owner_after).unwrap();
    let owner_interaction = owner_after_json["interactions"].to_string();
    assert!(
        owner_interaction.contains("why does this step work"),
        "question asked from the VISITING doc must appear reading the OWNER: {owner_interaction}"
    );
    assert!(
        owner_interaction.contains("note to self"),
        "annotation added on the OWNER must be present too: {owner_interaction}"
    );

    let (status, visit_after) = call(authed(
        "GET",
        &format!("/api/documents/{visit_doc_id}/nodes/{shared_node_id}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let visit_after_json: serde_json::Value = serde_json::from_str(&visit_after).unwrap();
    let visit_interaction = visit_after_json["interactions"].to_string();
    assert!(
        visit_interaction.contains("why does this step work"),
        "question asked from the VISITING doc must also show up reading through the SAME reference: {visit_interaction}"
    );
    assert!(
        visit_interaction.contains("note to self"),
        "annotation added on the OWNER must be visible reading through the VISITING reference: {visit_interaction}"
    );

    // §S15b step 4: `asked_in` is a provenance marker, present ONLY when it
    // differs from the document currently being read — reading a question
    // asked from elsewhere carries the marker, reading it through the same
    // document it was asked from does not (and vice versa for the
    // annotation, added on the OWNER).
    let owner_qa = owner_after_json["interactions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "qa")
        .expect("qa thread present");
    assert!(
        owner_qa["asked_in"].is_string(),
        "reading through the OWNER, a question asked from the VISITING doc must carry a provenance marker: {owner_qa}"
    );
    let owner_annotation = owner_after_json["interactions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "annotation")
        .expect("annotation present");
    assert!(
        owner_annotation["asked_in"].is_null(),
        "reading through the OWNER, an annotation added on the OWNER itself carries no marker: {owner_annotation}"
    );

    let visit_qa = visit_after_json["interactions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "qa")
        .expect("qa thread present");
    assert!(
        visit_qa["asked_in"].is_null(),
        "reading through the SAME document the question was asked from, no marker: {visit_qa}"
    );
    let visit_annotation = visit_after_json["interactions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "annotation")
        .expect("annotation present");
    assert!(
        visit_annotation["asked_in"].is_string(),
        "reading through the VISITING doc, an annotation added on the OWNER must carry a provenance marker: {visit_annotation}"
    );

    // §S15b step 3: the reference's own gate STATE in the VISITING
    // document's outline must fold in the owner's log too — this is the
    // literal thing step 3 exists for (content/interactions already
    // converge without it; the outline's "demonstrated" badge does not,
    // since the visiting document has no local events for a node it never
    // generated).
    let (status, visit_outline_body) = call(authed(
        "GET",
        &format!("/api/documents/{visit_doc_id}/outline"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let visit_outline: serde_json::Value = serde_json::from_str(&visit_outline_body).unwrap();
    let visit_item = visit_outline["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == shared_node_id)
        .expect("the reference must still be listed in the visiting outline");
    assert_eq!(
        visit_item["state"], "demonstrated",
        "the reference's state in the VISITING outline must fold in the owner's \
         MoveGraded event, not just the visiting document's own (empty) log: {visit_item}"
    );

    // Same fold, but on the single-node `GET .../nodes/{id}` endpoint's
    // `demonstrated` flag (drives whether the client offers a live
    // exercise) — a SEPARATE read from the outline's badge above, and it
    // used to read `doc_id`'s own (empty) log instead of the owner's.
    assert_eq!(
        visit_after_json["demonstrated"], true,
        "GET .../nodes/{{id}} must report `demonstrated` from the owner's log too, \
         or the visiting document would offer a live re-answer of an already-passed \
         exercise: {visit_after_json}"
    );
}

#[tokio::test]
async fn deleting_a_referenced_owner_is_refused_until_the_reference_is_gone() {
    // §S15b step 6: `Store::delete_document` is a plain `remove_dir_all` —
    // with a live reference that silently strands a `source_doc_id`
    // pointing at nothing. The endpoint must refuse, name the referencing
    // document, and only allow the delete once nothing points at the owner
    // anymore.
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

    let (_, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"algebra basics"}"#,
    ))
    .await;
    let owner_doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let owner_doc_id = owner_doc["doc_id"].as_str().unwrap().to_string();
    let shared_node_id = owner_doc["items"][0]["id"].as_str().unwrap().to_string();

    let visit_body = serde_json::json!({
        "topic": "integration",
        "nodes": [
            {
                "id": "prereq-slot",
                "title": "Algebra basics",
                "action": "skip",
                "known": {
                    "doc_id": owner_doc_id,
                    "doc_name": "Algebra basics",
                    "node_id": shared_node_id,
                },
                "children": [],
            },
            {
                "id": "obj-slot",
                "title": "Integration",
                "action": "learn",
                "children": [],
            },
        ],
    });
    let (status, body) = call(authed("POST", "/api/documents", &visit_body.to_string())).await;
    assert_eq!(status, StatusCode::OK, "create_document failed: {body}");
    let visit_doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let visit_doc_id = visit_doc["doc_id"].as_str().unwrap().to_string();

    // Refused while the reference is live — and the body names the
    // referencing document, not just a bare "no" (the learner has to be
    // able to act on it: which document do I delete or edit first?).
    let (status, delete_body) = call(authed(
        "DELETE",
        &format!("/api/documents/{owner_doc_id}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{delete_body}");
    assert!(
        delete_body.contains("integration") || delete_body.contains(&visit_doc_id),
        "refusal must name the referencing document: {delete_body}"
    );

    // The owner must still be there — a refused delete is a true no-op.
    let (status, _) = call(authed(
        "GET",
        &format!("/api/documents/{owner_doc_id}/outline"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);

    // Deleting the referencing document first removes the only pointer —
    // the owner is now free to go.
    let (status, _) = call(authed(
        "DELETE",
        &format!("/api/documents/{visit_doc_id}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, delete_body) = call(authed(
        "DELETE",
        &format!("/api/documents/{owner_doc_id}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{delete_body}");
}

#[tokio::test]
async fn a_question_spawned_from_a_reference_shows_up_in_the_visitors_sidebar_tree() {
    // §S15b step 5, read side (design decision 4): the write side already
    // parents a reference's spawned sub-node under the OWNER's outline
    // (verified by `asking_a_question_that_warrants_depth_spawns_a_real_
    // subnode` and step 1-2's convergence test) — but `outline_view` only
    // walked `outline.items` of the document being read, so the VISITING
    // document's sidebar tree never showed it, even though reading the
    // node itself already recursively splices it inline (`node.js`). This
    // pulls the owner's subtree in too.
    let state = test_state_with_ai(spawn_ai());
    let call = |req: Request<Body>| {
        let state = state.clone();
        async move {
            let resp = build_router(state).oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };

    let (_, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"algebra basics"}"#,
    ))
    .await;
    let owner_doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let owner_doc_id = owner_doc["doc_id"].as_str().unwrap().to_string();
    let shared_node_id = owner_doc["items"][0]["id"].as_str().unwrap().to_string();

    generate_to_completion(&call, &owner_doc_id, &shared_node_id).await;

    let visit_body = serde_json::json!({
        "topic": "integration",
        "nodes": [
            {
                "id": "prereq-slot",
                "title": "Algebra basics",
                "action": "skip",
                "known": {
                    "doc_id": owner_doc_id,
                    "doc_name": "Algebra basics",
                    "node_id": shared_node_id,
                },
                "children": [],
            },
            {
                "id": "obj-slot",
                "title": "Integration",
                "action": "learn",
                "children": [],
            },
        ],
    });
    let (status, body) = call(authed("POST", "/api/documents", &visit_body.to_string())).await;
    assert_eq!(status, StatusCode::OK, "create_document failed: {body}");
    let visit_doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let visit_doc_id = visit_doc["doc_id"].as_str().unwrap().to_string();

    // A question asked from the VISITING document, against the reference,
    // spawns a sub-node — parented (per the write side) to the shared node
    // in the OWNER's outline.
    let (status, view_body) = call(authed(
        "GET",
        &format!("/api/documents/{visit_doc_id}/nodes/{shared_node_id}"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let view: serde_json::Value = serde_json::from_str(&view_body).unwrap();
    let block_id = view["content_html"]
        .as_str()
        .unwrap()
        .split("data-block-id=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("at least one frozen block")
        .to_string();

    let (status, ask_body) = call(authed(
        "POST",
        &format!("/api/documents/{visit_doc_id}/nodes/{shared_node_id}/ask"),
        &format!(
            r#"{{"question":"how does this generalize to a harder case?","anchor":{{"block_id":"{block_id}"}}}}"#
        ),
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{ask_body}");
    let ask: serde_json::Value = serde_json::from_str(&ask_body).unwrap();
    assert_eq!(ask["kind"], "spawn");
    let sub_id = ask["node_id"].as_str().unwrap().to_string();

    // The visiting document's OWN outline must show the sub-node, parented
    // to the reference, even though the sub-node itself was never written
    // to the visiting document's outline.json.
    let (status, outline_body) = call(authed(
        "GET",
        &format!("/api/documents/{visit_doc_id}/outline"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let visit_outline: serde_json::Value = serde_json::from_str(&outline_body).unwrap();
    let sub_item = visit_outline["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == sub_id)
        .unwrap_or_else(|| {
            panic!("spawned sub-node {sub_id} must appear in the visiting outline: {visit_outline}")
        });
    assert_eq!(sub_item["parent_id"], shared_node_id);
    assert_eq!(sub_item["title"], "Fractions in depth");

    // Showing up in the tree is not enough — the visitor must actually be
    // able to open it. `owner_of_node`'s one-hop fallback (it isn't in the
    // visiting document's own outline.json, only the owner's) is what makes
    // this resolve instead of 404ing.
    let (status, sub_view_body) = call(authed(
        "GET",
        &format!("/api/documents/{visit_doc_id}/nodes/{sub_id}"),
        "",
    ))
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the visitor must be able to open the spawned sub-node, not just see it listed: {sub_view_body}"
    );
    let sub_view: serde_json::Value = serde_json::from_str(&sub_view_body).unwrap();
    assert!(
        !sub_view["content_html"].as_str().unwrap_or("").is_empty(),
        "opened sub-node must carry real content: {sub_view}"
    );
}

#[tokio::test]
async fn a_book_with_chapter_children_is_never_directly_generable_and_gates_correctly() {
    // S27g (2026-08-29, added in review before the topic-proposal slice
    // shipped): a `Book`/`Article` item that got topic-scoped `Chapter`
    // children must never itself be generated — its chapters carry the
    // actual content now — and whatever comes after it in the reading list
    // must still unlock once every chapter is settled, even though the book
    // item itself never receives a `Demonstrated` event (nothing ever
    // generates it). Without this, the reported bug ("o livro todo estava
    // sendo tratado como um nodo só") just relocates to the end of the
    // reading list instead of going away, and — the second, worse failure
    // mode this test also pins — anything gated behind the book would lock
    // forever, since a container never satisfies a plain `states.get`
    // prerequisite check.
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

    let (_, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"recursion in C","objective_text":"Understand recursion in C","nodes":[
            {"id":"book1","title":"The C Programming Language","action":"learn","item_type":"book","children":[
                {"id":"c1","title":"functions in C","action":"learn","item_type":"chapter","children":[]},
                {"id":"c2","title":"recursion in C","action":"learn","item_type":"chapter","children":[]}
            ]},
            {"id":"after","title":"After the book","action":"learn","children":[]}
        ]}"#,
    ))
    .await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();

    let find_state = |items: &serde_json::Value, id: &str| {
        items
            .as_array()
            .unwrap()
            .iter()
            .find(|it| it["id"] == id)
            .unwrap_or_else(|| panic!("no item {id} in {items}"))["state"]
            .clone()
    };

    // Neither chapter is done yet: the book is locked, not available — a
    // container never shows "available", since there is nothing to click
    // through to generate directly.
    assert_eq!(
        find_state(&created["items"], "book1"),
        serde_json::json!("locked")
    );
    assert_eq!(
        find_state(&created["items"], "after"),
        serde_json::json!("locked")
    );

    // The server refuses to generate the book directly, regardless of what
    // state the client thinks it's in — this is the enforcement point, not
    // just the display hint.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/book1/generate"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "SSE responses are always 200");
    assert!(body.contains("event: error"));
    assert!(body.contains("container"));
    assert!(!body.contains("event: done"));

    // Settle both chapters — one demonstrated via full generation, one
    // skipped, exactly like an ordinary prerequisite chain.
    generate_to_completion(&call, &doc_id, "c1").await;
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/c1/answer"),
        r#"{"answer":"I apply the concept to a new case like this..."}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let ans: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ans["advance"], serde_json::json!(true));

    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/c2/skip"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/outline"),
        "",
    ))
    .await;
    let outline: serde_json::Value = serde_json::from_str(&body).unwrap();

    // The book synthesizes "demonstrated" from its settled children, even
    // though it never received an event of its own...
    assert_eq!(
        find_state(&outline["items"], "book1"),
        serde_json::json!("demonstrated")
    );
    // ...and whatever comes after it unlocks on that synthesized state, not
    // never at all.
    assert_eq!(
        find_state(&outline["items"], "after"),
        serde_json::json!("available")
    );

    // Still refused, even fully "done" — a container is never generable,
    // not just "generable until its children finish".
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/book1/generate"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("event: error"));
    assert!(body.contains("container"));

    // Bugfix regression (2026-08-30, advisor-flagged): `resume_node_id`
    // must find `c1`'s node file, not silently stay `None` forever. `book1`
    // itself has no node file — nothing ever generates a container — so a
    // plain main-line-only `generated.contains(id)` lookup finds nothing,
    // which is exactly the bug this pins: it must fall back to the last
    // generated CHAPTER under the container.
    let (_, body) = call(authed("GET", "/api/documents", "")).await;
    let docs: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc = docs
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["doc_id"] == serde_json::json!(doc_id))
        .unwrap();
    assert_eq!(doc["resume_node_id"], serde_json::json!("c1"));
}

#[tokio::test]
async fn chapter_match_failure_offers_manual_page_or_whole_book_skip() {
    // The terminal remediation for a `Chapter` that `source::match_chapter`
    // could never place (S27g) — the user's own 2026-08-30 spec: pick the
    // page directly, skip the whole book, or restart. This test pins the
    // first two; "restart" is a plain client-side re-run of cold start with
    // the document's stored topic and has no route of its own.
    //
    // The matching pass itself needs a real PDF + confirmed TOC to exercise
    // end to end (see the S27g live test in `engine.rs`), which is out of
    // scope for a router-level test — this one starts from the OUTCOME a
    // failed match leaves behind (`expansion: expanded`,
    // `resolved_page: null`), written directly to `outline.json` the same
    // way `ensure_document_grounded` would have, and exercises everything
    // downstream of that: the view flag, and both remediation endpoints.
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

    let (_, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"recursion in C","objective_text":"Understand recursion in C","nodes":[
            {"id":"book1","title":"The C Programming Language","action":"learn","item_type":"book","children":[
                {"id":"c1","title":"functions in C","action":"learn","item_type":"chapter","children":[]},
                {"id":"c2","title":"recursion in C","action":"learn","item_type":"chapter","children":[]}
            ]},
            {"id":"after","title":"After the book","action":"learn","children":[]}
        ]}"#,
    ))
    .await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();

    // Simulate `ensure_document_grounded` having run and matched c1 but not
    // c2 — the exact shape a real match_chapter pass leaves behind.
    state
        .store
        .update_outline_file(&doc_id, |json| {
            let mut outline: crate::engine::Outline =
                serde_json::from_str(json).map_err(|e| e.to_string())?;
            for item in &mut outline.items {
                if item.id == "book1" {
                    item.expansion = crate::engine::ExpansionState::Expanded;
                }
                if item.id == "c1" {
                    item.resolved_page = Some(42);
                }
            }
            serde_json::to_string(&outline).map_err(|e| e.to_string())
        })
        .unwrap();

    let find_item = |items: &serde_json::Value, id: &str| {
        items
            .as_array()
            .unwrap()
            .iter()
            .find(|it| it["id"] == id)
            .unwrap_or_else(|| panic!("no item {id} in {items}"))
            .clone()
    };

    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/outline"),
        "",
    ))
    .await;
    let outline: serde_json::Value = serde_json::from_str(&body).unwrap();
    // c1 matched: no remediation flag.
    assert_eq!(
        find_item(&outline["items"], "c1")["chapter_match_failed"],
        serde_json::Value::Bool(false)
    );
    // c2 did not: the flag the client keys its remediation card on.
    assert_eq!(
        find_item(&outline["items"], "c2")["chapter_match_failed"],
        serde_json::Value::Bool(true)
    );

    // --- Arm 1: pick the page yourself ------------------------------------
    let (status, _) = call(authed(
        "PUT",
        &format!("/api/documents/{doc_id}/outline/c2/resolved_page"),
        r#"{"page":88}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/outline"),
        "",
    ))
    .await;
    let outline: serde_json::Value = serde_json::from_str(&body).unwrap();
    let c2 = find_item(&outline["items"], "c2");
    assert_eq!(c2["chapter_match_failed"], serde_json::Value::Bool(false));

    // Only a chapter's page can be set this way — the book id must be
    // refused, not silently accepted.
    let (status, _) = call(authed(
        "PUT",
        &format!("/api/documents/{doc_id}/outline/book1/resolved_page"),
        r#"{"page":1}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // --- Arm 2: skip the whole book, on a SECOND document -----------------
    // A fresh document instead of continuing the one above: c1 there is
    // already resolved and c2 already hand-picked, so neither would still
    // be "locked" — this arm specifically needs c2 locked (chained after
    // c1, unresolved) to prove `skip_book` does NOT go through `skip_node`'s
    // locked check.
    let (_, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"recursion in C","objective_text":"Understand recursion in C","nodes":[
            {"id":"book1","title":"The C Programming Language","action":"learn","item_type":"book","children":[
                {"id":"c1","title":"functions in C","action":"learn","item_type":"chapter","children":[]},
                {"id":"c2","title":"recursion in C","action":"learn","item_type":"chapter","children":[]}
            ]},
            {"id":"after","title":"After the book","action":"learn","children":[]}
        ]}"#,
    ))
    .await;
    let created2: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id2 = created2["doc_id"].as_str().unwrap().to_string();
    // c2 is locked (chained after c1, which has no event yet) — the case
    // that would defeat a naive "loop skip_node over the children" client.
    assert_eq!(
        find_item(&created2["items"], "c2")["state"],
        serde_json::json!("locked")
    );

    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id2}/outline/book1/skip_book"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id2}/outline"),
        "",
    ))
    .await;
    let outline2: serde_json::Value = serde_json::from_str(&body).unwrap();
    // Every child skipped, no event needed on the book id itself —
    // `effective_state` synthesizes "demonstrated" from settled children.
    assert_eq!(
        find_item(&outline2["items"], "book1")["state"],
        serde_json::json!("demonstrated")
    );
    assert_eq!(
        find_item(&outline2["items"], "after")["state"],
        serde_json::json!("available")
    );

    // A book with no chapter children yet is refused, not a silent no-op.
    let (_, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"fractions","objective_text":"Learn fractions","nodes":[{"id":"n0","title":"Fractions","action":"learn","children":[]}]}"#,
    ))
    .await;
    let created3: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id3 = created3["doc_id"].as_str().unwrap().to_string();
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id3}/outline/n0/skip_book"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn next_topic_appends_a_new_epoch_without_touching_the_first() {
    // §S15c ("what are we learning next?", PLAN.md's `TODO futuros`): once
    // a document's main line is fully demonstrated, confirming a new topic
    // must APPEND to the same document's outline/objective chain — nothing
    // about the first epoch gets rewritten or discarded (§5).
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

    let (_, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"fractions","objective_text":"Learn fractions","nodes":[{"id":"n0","title":"Fractions","action":"learn","children":[]}]}"#,
    ))
    .await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let first_node_id = created["items"][0]["id"].as_str().unwrap().to_string();

    generate_to_completion(&call, &doc_id, &first_node_id).await;
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{first_node_id}/answer"),
        r#"{"answer":"I apply the concept to a new case like this..."}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let ans: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ans["advance"], serde_json::json!(true));

    let (status, next_body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/next"),
        r#"{"topic":"integration","objective_text":"Learn integration","nodes":[{"id":"n1","title":"Integration","action":"learn","children":[]}]}"#,
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{next_body}");
    let next: serde_json::Value = serde_json::from_str(&next_body).unwrap();
    let items = next["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        2,
        "the new epoch's node must be APPENDED alongside the first, not replace it: {next_body}"
    );
    let first_item = items
        .iter()
        .find(|i| i["id"] == first_node_id)
        .expect("the first epoch's item must still be present");
    assert_eq!(
        first_item["state"], "demonstrated",
        "the first epoch's own state must be untouched by appending a second: {first_item}"
    );
    let second_item = items
        .iter()
        .find(|i| i["id"] != first_node_id)
        .expect("a second item must have been appended");
    assert_eq!(second_item["title"], "Integration");

    // The new epoch's node must be reachable through the outline endpoint
    // too, gated (trivially satisfied) on the first epoch's node — i.e. it
    // continues the SAME linear chain rather than starting a second one.
    let (status, outline_body) = call(authed(
        "GET",
        &format!("/api/documents/{doc_id}/outline"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);
    let outline: serde_json::Value = serde_json::from_str(&outline_body).unwrap();
    let second_in_outline = outline["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == second_item["id"])
        .expect("second epoch's item must be listed in the outline");
    assert_eq!(
        second_in_outline["state"], "available",
        "the new epoch's own node must be immediately available (its only \
         gate, the first epoch's node, is already demonstrated): {second_in_outline}"
    );

    // The objective chain grew by one version, tagged `NextTopic`, with the
    // first version left byte-for-byte alone (§5 non-destructive revision).
    let obj_json = state
        .store
        .read_doc_file(&doc_id, "objective.json")
        .unwrap();
    let log: crate::objective::ObjectiveLog = serde_json::from_str(&obj_json).unwrap();
    assert_eq!(log.versions.len(), 2);
    assert_eq!(log.versions[0].text, "Learn fractions");
    assert_eq!(
        log.versions[0].source,
        crate::objective::ObjectiveSource::ColdStart
    );
    assert_eq!(log.versions[1].text, "Learn integration");
    assert_eq!(
        log.versions[1].source,
        crate::objective::ObjectiveSource::NextTopic
    );
}

// S27n: `/api/library/{hash}` and `/api/library/{hash}/pdf` resolve a
// `<cite data-source-id>` content hash straight against `<data>/library/`,
// closing the defect where every citation on a real generated document
// 404'd against `state.corpus` (which a library-grounded document never
// writes into). "Done" per PLAN.md's own S27n criterion: router coverage of
// both the 404 (unknown hash) and the success (hash present in the index)
// paths — not just that the panel opens.

#[tokio::test]
async fn library_pdf_route_404s_for_a_hash_not_in_the_index() {
    let req = Request::builder()
        .uri("/api/library/does-not-exist/pdf")
        .header("host", HOST)
        .header("x-learnive-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn library_meta_route_404s_for_a_hash_not_in_the_index() {
    let req = Request::builder()
        .uri("/api/library/does-not-exist")
        .header("host", HOST)
        .header("x-learnive-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn library_routes_resolve_a_hash_present_in_the_index() {
    let state = test_state();
    let data_dir = std::path::PathBuf::from(state.data_dir.as_ref());
    let library = crate::source::LocalPdfSource::open(&data_dir).unwrap();
    // `test_state()` already wrote this fixture into the library (see
    // `write_book_pdf`'s call sites above) — reuse it rather than writing a
    // third PDF, so this test proves the real content-hash path, not a
    // hand-rolled one.
    let pdf_path = library.root().join("demo-foundations.pdf");
    let pdf_bytes = std::fs::read(&pdf_path).unwrap();
    let hash = crate::source::acervo::content_hash(&pdf_bytes);

    // Populate the index the way `validate_acervo_with_progress` does —
    // directly, rather than running a full acervo pass, since this test is
    // only exercising the two read routes, not the validation engine
    // (that's `source::acervo`'s own test module's job).
    let index_root = data_dir.join("index");
    let file_index = crate::source::acervo::LibraryFileIndex::open(&index_root).unwrap();
    file_index
        .set(
            &hash,
            "demo-foundations.pdf",
            Some("Demo Foundations"),
            Some("Demo Author"),
        )
        .unwrap();

    let app = build_router(state);

    let meta_req = Request::builder()
        .uri(format!("/api/library/{hash}"))
        .header("host", HOST)
        .header("x-learnive-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let meta_resp = app.clone().oneshot(meta_req).await.unwrap();
    assert_eq!(meta_resp.status(), StatusCode::OK);
    let meta_bytes = to_bytes(meta_resp.into_body(), usize::MAX).await.unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&meta_bytes).unwrap();
    assert_eq!(meta["title"], "Demo Foundations");
    assert_eq!(meta["filename"], "demo-foundations.pdf");

    let pdf_req = Request::builder()
        .uri(format!("/api/library/{hash}/pdf"))
        .header("host", HOST)
        .header("x-learnive-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let pdf_resp = app.oneshot(pdf_req).await.unwrap();
    assert_eq!(pdf_resp.status(), StatusCode::OK);
    assert_eq!(
        pdf_resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/pdf"
    );
    let served_bytes = to_bytes(pdf_resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(served_bytes.as_ref(), pdf_bytes.as_slice());
}
