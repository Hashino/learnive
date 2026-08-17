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
    // acquisition mocked (no network) and grounding off (no model download).
    let dir = std::env::temp_dir().join(format!("learnive-test-{}", crate::engine::new_id()));
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
        corpus: Corpus::open(&dir).unwrap(),
        retriever: None,
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

    let (_, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"fractions","name":"Fractions 101"}"#,
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

    // 2. First node's generation — streams and ends with `done`.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
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

    // Generate once.
    let (status, _) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
    assert_eq!(status, StatusCode::OK);

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
    // completes a real node this time.
    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
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
    // (and writes the `.rubric.json` sidecar) once its body is drained.
    let resp = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
    to_bytes(resp.into_body(), usize::MAX).await.unwrap();

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
    // demonstrated) — the node closing is the distillation trigger.
    call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
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

    // 2. Confirm — locks objective version 1.
    let (status, body) = call(authed(
        "POST",
        "/api/documents",
        r#"{"topic":"fractions","objective_text":"Learn to add and subtract fractions"}"#,
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
#[tokio::test]
async fn research_move_acquires_then_resumes_the_real_move_loop() {
    use crate::ai::{Ai, MockProvider, Models, Provider};
    use crate::movement::AgentPolicy;

    let ai = Ai::new(
        Provider::Mock(MockProvider::scripted(research_once_responder)),
        Models::single("demo"),
    );
    let state = test_state_with_ai(ai);
    // Tests never construct a real retriever (it'd need a model download),
    // so `acquire` takes its `retriever: None` no-op path — meaning this
    // also pins the "no adequate source found" branch of the research move,
    // not just its happy path.
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

    let (_, body) = call(authed("POST", "/api/documents", r#"{"topic":"fractions"}"#)).await;
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let doc_id = created["doc_id"].as_str().unwrap().to_string();
    let node0 = created["items"][0]["id"].as_str().unwrap().to_string();

    let (status, body) = call(authed(
        "POST",
        &format!("/api/documents/{doc_id}/nodes/{node0}/generate"),
        "",
    ))
    .await;
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
        "retriever is None in tests, so acquisition must report failure, \
         not silently claim a source: {body}"
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
