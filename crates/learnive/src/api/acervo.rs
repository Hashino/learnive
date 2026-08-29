//! S27f: the acervo-gate report, PDF↔item manual matching, and TOC
//! confirmation screens' endpoints (§11.1, PLAN.md S27f).
//!
//! Surfaces logic already built by S27c (`source::acervo`'s six-check
//! engine) and S27d (bibliographic verification) — this module adds no new
//! matching/validation logic of its own beyond the candidate-ranking helpers
//! landed alongside it in `source::acervo` (`candidate_matches`,
//! `unmatched_library_files`, `match_report`).
//!
//! **Deliberately not wired into cold start or `create_document`** — making
//! the gate mandatory/blocking is explicitly S27h's job, not this slice's.
//! These endpoints are read/act-on-demand: a document that never opens the
//! acervo screen behaves exactly as it did before this module existed.
//!
//! **Cost note:** `source::acervo::validate_acervo`/`candidate_matches`/
//! `match_report` each read and parse every PDF in the library
//! (`source::acervo::load_candidates` — full `pdf-extract` text extraction
//! plus a second `lopdf` metadata pass per file). That's fine for an
//! on-demand screen a user opens deliberately, but it is real, synchronous,
//! CPU-bound work — every handler here runs it inside
//! `tokio::task::spawn_blocking` so it never blocks the async runtime's
//! worker threads. No caching layer is added on top (out of scope for this
//! slice, flagged rather than built).
//!
//! Bibliographic items eligible for these endpoints are exactly the
//! `OutlineItem`s of kind `Book`/`Article` that carry a `source` pointer
//! (S27e) — a `Node`/`Chapter` item has no bibliographic identity of its own
//! and is skipped.

use std::fs;
use std::path::PathBuf;

use tokio::task::spawn_blocking;

use crate::source::{
    self, ConfirmedToc, ConfirmedTocEntry, ExpectedItem, LocalPdfSource, ManualMatchStore,
    OutlineEntry, SourceKind, TocConfirmStore,
};

use super::*;

fn library(state: &AppState) -> Result<LocalPdfSource, ApiError> {
    LocalPdfSource::open(state.data_dir.as_ref())
        .map_err(|e| ApiError::Internal(format!("could not open local library: {e}")))
}

fn index_cache_dir(state: &AppState) -> PathBuf {
    PathBuf::from(state.data_dir.as_ref())
        .join("index")
        .join("library")
}

fn manual_match_store(state: &AppState) -> Result<ManualMatchStore, ApiError> {
    ManualMatchStore::open(state.data_dir.as_ref())
        .map_err(|e| ApiError::Internal(format!("could not open manual-match store: {e}")))
}

fn toc_confirm_store(state: &AppState) -> Result<TocConfirmStore, ApiError> {
    TocConfirmStore::open(state.data_dir.as_ref())
        .map_err(|e| ApiError::Internal(format!("could not open TOC-confirmation store: {e}")))
}

fn load_outline(state: &AppState, doc: &str) -> Result<Outline, ApiError> {
    let json = state.store.read_doc_file(doc, "outline.json")?;
    serde_json::from_str(&json)
        .map_err(|e| ApiError::Internal(format!("corrupt outline.json: {e}")))
}

/// Every `Book`/`Article` outline item with a bibliographic source pointer,
/// paired with the outline item id the S27f screens address it by. A
/// `Node`/`Chapter` item has no bibliographic identity and is skipped.
fn expected_items(outline: &Outline) -> Vec<(String, ExpectedItem)> {
    outline
        .items
        .iter()
        .filter(|item| {
            matches!(
                item.item_type,
                OutlineItemType::Book | OutlineItemType::Article
            )
        })
        .filter_map(|item| {
            let ptr = item.source.as_ref()?;
            Some((
                item.id.clone(),
                ExpectedItem {
                    title: ptr.item.title.clone(),
                    authors: ptr.item.authors.clone(),
                    kind: ptr.item.kind,
                },
            ))
        })
        .collect()
}

fn find_expected_item(
    state: &AppState,
    doc: &str,
    item_id: &str,
) -> Result<ExpectedItem, ApiError> {
    let outline = load_outline(state, doc)?;
    expected_items(&outline)
        .into_iter()
        .find(|(id, _)| id == item_id)
        .map(|(_, item)| item)
        .ok_or_else(|| ApiError::NotFound(format!("no bibliographic item {item_id}")))
}

/// Resolves the single filename currently understood to represent an
/// expected item: a recorded manual pairing wins outright; otherwise a
/// unique automatic candidate; anything else (no candidate, or more than
/// one) is unresolved — the caller (the TOC endpoints) can't proceed without
/// the matching screen settling it first.
fn resolve_matched_filename(
    library: &LocalPdfSource,
    manual: &ManualMatchStore,
    item: &ExpectedItem,
) -> std::io::Result<Option<String>> {
    if let Some(m) = manual.get(item) {
        return Ok(Some(m.filename));
    }
    let candidates = source::acervo::candidate_matches(library, item)?;
    match candidates.len() {
        1 => Ok(Some(
            candidates.into_iter().next().expect("len == 1").filename,
        )),
        _ => Ok(None),
    }
}

fn flatten_outline(entries: &[OutlineEntry], out: &mut Vec<TocEntryResp>) {
    for e in entries {
        out.push(TocEntryResp {
            title: e.title.clone(),
            page: Some(e.page),
        });
        flatten_outline(&e.children, out);
    }
}

// -- GET /api/documents/{doc}/acervo -- the gate report ("what's missing") --

#[derive(Serialize)]
pub struct AcervoItemResp {
    pub item_id: String,
    pub title: String,
    pub authors: Vec<String>,
    pub kind: SourceKind,
    pub filename: Option<String>,
    pub presence: &'static str,
    pub identity: &'static str,
    pub identity_reason: Option<String>,
    pub text_layer: &'static str,
    pub toc: &'static str,
    pub needs_toc_confirmation: bool,
    pub page_map: &'static str,
    pub index: &'static str,
    pub passes: bool,
}

#[derive(Serialize)]
pub struct AcervoReportResp {
    pub items: Vec<AcervoItemResp>,
    pub all_pass: bool,
    /// **Always absolute** — canonicalized, never `state.data_dir` echoed
    /// as-is. `LEARNIVE_DATA_DIR` defaults to the relative string
    /// `"learnive-data"` (`app.rs`), so joining it with `library` without
    /// resolving against the working directory showed a path that was only
    /// meaningful from the server process's CWD, not to a user reading it
    /// in the browser (bug reported live, 2026-08-29, right after the path
    /// was first added for the *previous* live bug — "Missing" with no
    /// indication of where to put the file).
    pub library_path: String,
}

/// The acervo gate's own report — real and actionable, but deliberately
/// **not enforced** anywhere in the generation flow (S27h's job). Lists
/// what's missing by bibliographic title, never by filename (SPEC's own
/// wording).
pub async fn get_acervo_report(
    State(state): State<AppState>,
    Path(doc): Path<String>,
) -> Result<Json<AcervoReportResp>, ApiError> {
    let outline = load_outline(&state, &doc)?;
    let expected = expected_items(&outline);
    let lib = library(&state)?;
    // `lib.root()` is `state.data_dir`/library as configured — possibly
    // relative (default `LEARNIVE_DATA_DIR` is the relative string
    // "learnive-data"). Canonicalize so what the browser shows is a path
    // meaningful from wherever the user is, not just from the server
    // process's CWD. `LocalPdfSource::open` already created this directory,
    // so canonicalize only fails on a genuinely broken environment (e.g.
    // permissions) — fall back to the uncanonicalized path rather than
    // erroring the whole report over a display detail.
    let library_path = fs::canonicalize(lib.root())
        .unwrap_or_else(|_| lib.root().to_path_buf())
        .to_string_lossy()
        .into_owned();
    if expected.is_empty() {
        return Ok(Json(AcervoReportResp {
            items: Vec::new(),
            all_pass: true,
            library_path,
        }));
    }

    let idx_dir = index_cache_dir(&state);
    let ids: Vec<String> = expected.iter().map(|(id, _)| id.clone()).collect();
    let items_only: Vec<ExpectedItem> = expected.into_iter().map(|(_, item)| item).collect();

    let report =
        spawn_blocking(move || source::acervo::validate_acervo(&lib, &items_only, &idx_dir))
            .await
            .map_err(|e| ApiError::Internal(format!("acervo validation task panicked: {e}")))?
            .map_err(|e| ApiError::Internal(format!("acervo validation failed: {e}")))?;

    let items: Vec<AcervoItemResp> = ids
        .into_iter()
        .zip(report.items)
        .map(|(item_id, r)| {
            let (filename, presence) = match &r.presence {
                source::PresenceCheck::Found { filename } => (Some(filename.clone()), "found"),
                source::PresenceCheck::Missing => (None, "missing"),
            };
            let (identity, identity_reason) = match &r.identity {
                source::IdentityCheck::Match => ("match", None),
                source::IdentityCheck::Mismatch { reason } => ("mismatch", Some(reason.clone())),
                source::IdentityCheck::Skipped => ("skipped", None),
            };
            let text_layer = match &r.text_layer {
                source::TextLayerCheck::Extractable { .. } => "extractable",
                source::TextLayerCheck::NoText => "no_text",
                source::TextLayerCheck::Skipped => "skipped",
            };
            let needs_toc_confirmation = r.toc.needs_user_confirmation();
            let toc = match &r.toc {
                source::TocCheck::Embedded { .. } => "embedded",
                source::TocCheck::Heuristic { .. } => "heuristic",
                source::TocCheck::Unavailable => "unavailable",
                source::TocCheck::Skipped => "skipped",
            };
            let page_map = match &r.page_map {
                source::PageMapCheck::Labeled { .. } => "labeled",
                source::PageMapCheck::PhysicalOnly { .. } => "physical_only",
                source::PageMapCheck::Skipped => "skipped",
            };
            let index = match &r.index {
                source::IndexCheck::Cached { .. } => "cached",
                source::IndexCheck::Missing => "missing",
                source::IndexCheck::Skipped => "skipped",
            };
            AcervoItemResp {
                item_id,
                title: r.expected.title.clone(),
                authors: r.expected.authors.clone(),
                kind: r.expected.kind,
                filename,
                presence,
                identity,
                identity_reason,
                text_layer,
                toc,
                needs_toc_confirmation,
                page_map,
                index,
                passes: r.passes(),
            }
        })
        .collect();

    let all_pass = items.iter().all(|i| i.passes);
    Ok(Json(AcervoReportResp {
        items,
        all_pass,
        library_path,
    }))
}

// -- GET/POST /api/documents/{doc}/acervo/matches -- PDF<->item matching --

#[derive(Serialize)]
pub struct CandidateResp {
    pub filename: String,
    pub confidence: &'static str,
}

#[derive(Serialize)]
pub struct AmbiguousItemResp {
    pub item_id: String,
    pub title: String,
    pub candidates: Vec<CandidateResp>,
    pub manual_match: Option<String>,
}

#[derive(Serialize)]
pub struct AcervoMatchesResp {
    pub ambiguous: Vec<AmbiguousItemResp>,
    pub unmatched_files: Vec<String>,
}

/// Ambiguous cases only: an item with more than one plausible candidate, or
/// a library PDF that matched nothing. An item with exactly one candidate
/// (or zero — that's just `presence: missing` on the report above) is not
/// listed here; the matching screen only needs to show what actually needs a
/// human decision.
pub async fn get_acervo_matches(
    State(state): State<AppState>,
    Path(doc): Path<String>,
) -> Result<Json<AcervoMatchesResp>, ApiError> {
    let outline = load_outline(&state, &doc)?;
    let expected = expected_items(&outline);
    let lib = library(&state)?;
    let manual = manual_match_store(&state)?;

    let ids: Vec<String> = expected.iter().map(|(id, _)| id.clone()).collect();
    let items_only: Vec<ExpectedItem> = expected.iter().map(|(_, item)| item.clone()).collect();

    let (per_item, unmatched_files) =
        spawn_blocking(move || source::acervo::match_report(&lib, &items_only))
            .await
            .map_err(|e| ApiError::Internal(format!("matching task panicked: {e}")))?
            .map_err(|e| ApiError::Internal(format!("matching scan failed: {e}")))?;

    let mut ambiguous = Vec::new();
    for ((item_id, item), candidates) in ids
        .into_iter()
        .zip(expected.iter().map(|(_, i)| i))
        .zip(per_item)
    {
        if candidates.len() <= 1 {
            continue;
        }
        ambiguous.push(AmbiguousItemResp {
            item_id,
            title: item.title.clone(),
            candidates: candidates
                .into_iter()
                .map(|c| CandidateResp {
                    filename: c.filename,
                    confidence: match c.confidence {
                        source::MatchConfidence::Strong => "strong",
                        source::MatchConfidence::Weak => "weak",
                    },
                })
                .collect(),
            manual_match: manual.get(item).map(|m| m.filename),
        });
    }

    Ok(Json(AcervoMatchesResp {
        ambiguous,
        unmatched_files,
    }))
}

#[derive(Deserialize)]
pub struct SetMatchReq {
    pub item_id: String,
    pub filename: String,
}

/// Records the user's manual pairing (`ManualMatchStore`) — never fed back
/// into `source::acervo::validate_acervo`'s own matching in this slice, per
/// the module doc; the one place this slice reads it back is the TOC
/// endpoints below, via `resolve_matched_filename`.
pub async fn set_acervo_match(
    State(state): State<AppState>,
    Path(doc): Path<String>,
    Json(req): Json<SetMatchReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let item = find_expected_item(&state, &doc, &req.item_id)?;

    let lib = library(&state)?;
    let filename = req.filename.clone();
    let exists = spawn_blocking(move || lib.scan())
        .await
        .map_err(|e| ApiError::Internal(format!("scan task panicked: {e}")))?
        .map_err(|e| ApiError::Internal(format!("library scan failed: {e}")))?
        .into_iter()
        .any(|entry| entry.filename == filename);
    if !exists {
        return Err(ApiError::BadRequest(format!(
            "no such library file: {}",
            req.filename
        )));
    }

    let manual = manual_match_store(&state)?;
    manual
        .set(&item, &req.filename)
        .map_err(|e| ApiError::Internal(format!("could not save manual match: {e}")))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// -- GET/PUT /api/documents/{doc}/acervo/toc/{item} -- TOC confirmation --

#[derive(Serialize)]
pub struct TocEntryResp {
    pub title: String,
    pub page: Option<usize>,
}

#[derive(Serialize)]
pub struct TocResp {
    pub item_id: String,
    pub filename: String,
    /// `"embedded" | "heuristic" | "confirmed" | "unavailable"`.
    pub source: &'static str,
    /// False only for `"embedded"` (a real `/Outlines` tree is read-only
    /// display here, never a "confirmation" — it never needed the safety
    /// net in the first place).
    pub editable: bool,
    pub entries: Vec<TocEntryResp>,
}

pub async fn get_acervo_toc(
    State(state): State<AppState>,
    Path((doc, item_id)): Path<(String, String)>,
) -> Result<Json<TocResp>, ApiError> {
    let item = find_expected_item(&state, &doc, &item_id)?;
    let lib = library(&state)?;
    let manual = manual_match_store(&state)?;
    let toc_store = toc_confirm_store(&state)?;

    let lib2 = lib.clone();
    let item2 = item.clone();
    let filename = spawn_blocking(move || resolve_matched_filename(&lib2, &manual, &item2))
        .await
        .map_err(|e| ApiError::Internal(format!("matching task panicked: {e}")))?
        .map_err(|e| ApiError::Internal(format!("matching scan failed: {e}")))?
        .ok_or_else(|| {
            ApiError::BadRequest(
                "this item has no single matched PDF yet — resolve it on the matching screen first"
                    .to_string(),
            )
        })?;

    let path = lib.root().join(&filename);
    let (source_label, editable, entries) = spawn_blocking(move || -> Result<_, String> {
        let bytes = fs::read(&path).map_err(|e| e.to_string())?;
        let pdf = source::read_pdf(&path).map_err(|e| e.to_string())?;
        let hash = source::acervo::content_hash(&bytes);
        if let Some(confirmed) = toc_store.get(&hash) {
            let entries = confirmed
                .entries
                .into_iter()
                .map(|e| TocEntryResp {
                    title: e.title,
                    page: e.page,
                })
                .collect();
            return Ok(("confirmed", true, entries));
        }
        if !pdf.outline.is_empty() {
            let mut flat = Vec::new();
            flatten_outline(&pdf.outline, &mut flat);
            return Ok(("embedded", false, flat));
        }
        let heuristic = source::acervo::heuristic_toc(&pdf);
        if heuristic.is_empty() {
            return Ok(("unavailable", true, Vec::new()));
        }
        let entries = heuristic
            .into_iter()
            .map(|title| TocEntryResp { title, page: None })
            .collect();
        Ok(("heuristic", true, entries))
    })
    .await
    .map_err(|e| ApiError::Internal(format!("TOC-read task panicked: {e}")))?
    .map_err(ApiError::Internal)?;

    Ok(Json(TocResp {
        item_id,
        filename,
        source: source_label,
        editable,
        entries,
    }))
}

#[derive(Deserialize)]
pub struct TocEntryReq {
    pub title: String,
    #[serde(default)]
    pub page: Option<usize>,
}

#[derive(Deserialize)]
pub struct PutTocReq {
    pub entries: Vec<TocEntryReq>,
}

/// Persists the user's corrected TOC. No PDF is ever rejected for lacking
/// bookmarks (SPEC §11.1) — this only records a correction to the deduced
/// result for a later slice (S27g's contextual expansion) to read back; it
/// never blocks anything itself.
pub async fn put_acervo_toc(
    State(state): State<AppState>,
    Path((doc, item_id)): Path<(String, String)>,
    Json(req): Json<PutTocReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if req.entries.is_empty() {
        return Err(ApiError::BadRequest(
            "a table of contents needs at least one entry".to_string(),
        ));
    }
    let item = find_expected_item(&state, &doc, &item_id)?;
    let lib = library(&state)?;
    let manual = manual_match_store(&state)?;
    let toc_store = toc_confirm_store(&state)?;

    let lib2 = lib.clone();
    let item2 = item.clone();
    let filename = spawn_blocking(move || resolve_matched_filename(&lib2, &manual, &item2))
        .await
        .map_err(|e| ApiError::Internal(format!("matching task panicked: {e}")))?
        .map_err(|e| ApiError::Internal(format!("matching scan failed: {e}")))?
        .ok_or_else(|| {
            ApiError::BadRequest(
                "this item has no single matched PDF yet — resolve it on the matching screen first"
                    .to_string(),
            )
        })?;

    let path = lib.root().join(&filename);
    let entries: Vec<ConfirmedTocEntry> = req
        .entries
        .into_iter()
        .map(|e| ConfirmedTocEntry {
            title: e.title,
            page: e.page,
        })
        .collect();

    spawn_blocking(move || -> Result<(), String> {
        let bytes = fs::read(&path).map_err(|e| e.to_string())?;
        let hash = source::acervo::content_hash(&bytes);
        toc_store
            .put(&hash, &ConfirmedToc { entries })
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| ApiError::Internal(format!("TOC-write task panicked: {e}")))?
    .map_err(ApiError::Internal)?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{OutlineItem, SourcePointer};
    use crate::source::ProposedItem;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;

    const TOKEN: &str = "testtoken";
    const HOST: &str = "127.0.0.1:7420";
    const ORIGIN: &str = "http://127.0.0.1:7420";

    fn test_state() -> (tempfile::TempDir, AppState) {
        use arc_swap::ArcSwap;
        use std::collections::HashSet;
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let dir = tempfile::tempdir().expect("temp dir");
        let data_dir = dir.path().to_path_buf();
        let state = AppState {
            token: Arc::from(TOKEN),
            allowed_origins: Arc::new(HashSet::from([ORIGIN.to_string()])),
            allowed_hosts: Arc::new(HashSet::from([HOST.to_string()])),
            store: crate::store::Store::open(&data_dir).unwrap(),
            ai: Arc::new(ArcSwap::from_pointee(crate::api::demo_ai())),
            policy: Arc::new(ArcSwap::from_pointee(crate::movement::AgentPolicy::L0)),
            config: Arc::new(RwLock::new(crate::config::AppConfig::default())),
            secret: Arc::new(crate::secret::SecretStore::open(&data_dir)),
            data_dir: Arc::from(data_dir.to_string_lossy().as_ref()),
            source: Arc::new(crate::source::Source::Mock(crate::source::MockSource::new())),
            fallback_source: Arc::new(
                crate::source::Source::Mock(crate::source::MockSource::new()),
            ),
            corpus: crate::source::Corpus::open(&data_dir).unwrap(),
            retriever: None,
            bibliography_client: Arc::new(crate::source::BibliographyClient::unreachable_for_test()),
        };
        (dir, state)
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

    async fn send(state: &AppState, req: Request<Body>) -> (StatusCode, String) {
        let resp = crate::app::build_router(state.clone())
            .oneshot(req)
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn book_item(id: &str, title: &str, authors: &[&str]) -> OutlineItem {
        OutlineItem {
            id: id.to_string(),
            title: title.to_string(),
            prerequisites: vec![],
            parent_id: None,
            mode: NodeMode::default(),
            source_doc_id: None,
            item_type: OutlineItemType::Book,
            expansion: ExpansionState::default(),
            source: Some(SourcePointer {
                item: ProposedItem {
                    title: title.to_string(),
                    authors: authors.iter().map(|a| a.to_string()).collect(),
                    year: None,
                    edition: None,
                    identifier: None,
                    kind: SourceKind::Book,
                },
                verification: None,
            }),
        }
    }

    fn seed_document(state: &AppState, doc_id: &str, items: Vec<OutlineItem>) {
        state.store.create_document(doc_id).unwrap();
        let outline = Outline {
            topic: "test topic".to_string(),
            items,
        };
        state
            .store
            .write_doc_file(
                doc_id,
                "outline.json",
                &serde_json::to_string(&outline).unwrap(),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn acervo_report_lists_a_missing_book_by_title_not_filename() {
        let (_dir, state) = test_state();
        seed_document(
            &state,
            "doc1",
            vec![book_item(
                "b1",
                "Introduction to the Theory of Computation",
                &["Michael Sipser"],
            )],
        );

        let (status, body) = send(&state, authed("GET", "/api/documents/doc1/acervo", "")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let report: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(report["items"].as_array().unwrap().len(), 1);
        assert_eq!(report["items"][0]["presence"], "missing");
        assert_eq!(
            report["items"][0]["title"],
            "Introduction to the Theory of Computation"
        );
        assert!(report["items"][0]["filename"].is_null());
        assert_eq!(report["all_pass"], false);
        // Bug reported live 2026-08-29: the report said "Missing" with no
        // indication of where to put the file. `library_path` closes that.
        let expected_path =
            std::fs::canonicalize(std::path::PathBuf::from(state.data_dir.as_ref()).join("library"))
                .unwrap();
        assert_eq!(
            report["library_path"],
            expected_path.to_string_lossy().as_ref()
        );
        // Second bug, reported live the same day right after the first fix
        // landed: this must be absolute, never `state.data_dir` echoed
        // as-is — the default `LEARNIVE_DATA_DIR` is the relative string
        // "learnive-data", meaningless outside the server process's CWD.
        assert!(
            std::path::Path::new(report["library_path"].as_str().unwrap()).is_absolute(),
            "library_path must be absolute: {}",
            report["library_path"]
        );
    }

    #[tokio::test]
    async fn acervo_report_is_empty_and_passing_when_the_outline_has_no_bibliographic_items() {
        let (_dir, state) = test_state();
        seed_document(&state, "doc1", vec![]);

        let (status, body) = send(&state, authed("GET", "/api/documents/doc1/acervo", "")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let report: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(report["items"].as_array().unwrap().is_empty());
        assert_eq!(report["all_pass"], true);
        // The path must be present even on the early-return (empty-outline)
        // branch — a document that later gains a book source still needs it.
        assert!(report["library_path"].as_str().unwrap().ends_with("library"));
    }

    #[tokio::test]
    async fn acervo_report_404s_on_a_document_with_no_outline() {
        let (_dir, state) = test_state();
        let (status, _) = send(&state, authed("GET", "/api/documents/nope/acervo", "")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn set_and_read_back_a_manual_match_then_confirm_its_toc() {
        let (_dir, state) = test_state();
        seed_document(
            &state,
            "doc1",
            vec![book_item("b1", "A Book With No Bookmarks", &["An Author"])],
        );

        // Place a real (bookmark-free) PDF fixture into the library so the
        // TOC endpoints have something to read.
        let lib = LocalPdfSource::open(state.data_dir.as_ref()).unwrap();
        write_minimal_pdf(&lib.root().join("mybook.pdf"));

        // Manual match round trip.
        let (status, body) = send(
            &state,
            authed(
                "POST",
                "/api/documents/doc1/acervo/matches",
                r#"{"item_id":"b1","filename":"mybook.pdf"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let set_resp: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(set_resp["ok"], true);

        // TOC read falls back to heuristic/unavailable (fixture has no real
        // headings) but must resolve the matched filename via the manual
        // pairing just recorded, not error out as unmatched.
        let (status, body) = send(
            &state,
            authed("GET", "/api/documents/doc1/acervo/toc/b1", ""),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let toc: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(toc["filename"], "mybook.pdf");
        let toc_source = toc["source"].as_str().unwrap();
        assert!(
            matches!(toc_source, "heuristic" | "unavailable"),
            "{toc_source}"
        );

        // Confirm a corrected TOC.
        let (status, body) = send(
            &state,
            authed(
                "PUT",
                "/api/documents/doc1/acervo/toc/b1",
                r#"{"entries":[{"title":"Chapter 1","page":1},{"title":"Chapter 2","page":10}]}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let put_resp: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(put_resp["ok"], true);

        let (status, body) = send(
            &state,
            authed("GET", "/api/documents/doc1/acervo/toc/b1", ""),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let toc_again: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(toc_again["source"], "confirmed");
        assert_eq!(toc_again["entries"].as_array().unwrap().len(), 2);
        assert_eq!(toc_again["entries"][0]["title"], "Chapter 1");
        assert_eq!(toc_again["entries"][0]["page"], 1);
    }

    #[tokio::test]
    async fn matches_endpoint_surfaces_an_ambiguous_item_and_an_unmatched_file() {
        let (_dir, state) = test_state();
        seed_document(
            &state,
            "doc1",
            vec![book_item(
                "b1",
                "Introduction to the Theory of Computation",
                &["Michael Sipser"],
            )],
        );

        let lib = LocalPdfSource::open(state.data_dir.as_ref()).unwrap();
        // Two candidates for the same title (ambiguous) plus one unrelated
        // file (unmatched) — built with the same PDF-builder technique
        // `source::acervo`'s own tests use.
        write_pdf_with_metadata(
            &lib.root().join("a.pdf"),
            "Introduction to the Theory of Computation",
            "Michael Sipser",
        );
        write_pdf_with_metadata(
            &lib.root().join("b.pdf"),
            "Introduction to the Theory of Computation",
            "Someone Else",
        );
        write_pdf_with_metadata(
            &lib.root().join("unrelated.pdf"),
            "The Joy of Baking",
            "Jane Chef",
        );

        let (status, body) = send(
            &state,
            authed("GET", "/api/documents/doc1/acervo/matches", ""),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let matches: serde_json::Value = serde_json::from_str(&body).unwrap();
        let ambiguous = matches["ambiguous"].as_array().unwrap();
        assert_eq!(ambiguous.len(), 1);
        assert_eq!(ambiguous[0]["item_id"], "b1");
        assert_eq!(ambiguous[0]["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(
            matches["unmatched_files"],
            serde_json::json!(["unrelated.pdf"])
        );
    }

    #[tokio::test]
    async fn set_match_rejects_a_filename_not_in_the_library() {
        let (_dir, state) = test_state();
        seed_document(
            &state,
            "doc1",
            vec![book_item("b1", "A Book", &["An Author"])],
        );

        let (status, _) = send(
            &state,
            authed(
                "POST",
                "/api/documents/doc1/acervo/matches",
                r#"{"item_id":"b1","filename":"does-not-exist.pdf"}"#,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn toc_endpoints_refuse_an_item_with_no_resolved_match() {
        let (_dir, state) = test_state();
        seed_document(
            &state,
            "doc1",
            vec![book_item("b1", "A Book Nobody Has", &["An Author"])],
        );

        let (status, _) = send(
            &state,
            authed("GET", "/api/documents/doc1/acervo/toc/b1", ""),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    fn write_minimal_pdf(path: &std::path::Path) {
        write_pdf_with_metadata(path, "Untitled", "Nobody");
    }

    fn write_pdf_with_metadata(path: &std::path::Path, title: &str, author: &str) {
        use lopdf::{Document, Object, Stream, dictionary};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = format!("BT /F1 12 Tf 20 700 Td ({title}, by {author}.) Tj ET");
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            }),
        );
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal(title),
            "Author" => Object::string_literal(author),
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);
        doc.save(path).expect("save fixture pdf");
    }
}
