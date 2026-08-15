use super::reading::read_profile;
use super::*;

// ---------------------------------------------------------------------------
// Cold start (§6.1, §S4): topic → proposed objective → confirmed objective +
// outline. Two calls: `propose_objective` is stateless (nothing persisted,
// just a fast round-trip so the client can show an editable confirm box);
// `create_document` locks the (possibly edited) objective as version 1 and
// only then generates the outline, anchored on it.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ProposeObjectiveReq {
    topic: String,
}

#[derive(Serialize)]
pub struct ProposeObjectiveResp {
    text: String,
    /// Short document name (§S12) — proposed here, persisted by
    /// `create_document`, renameable afterwards via `rename_document`.
    title: String,
}

pub async fn propose_objective(
    State(state): State<AppState>,
    Json(body): Json<ProposeObjectiveReq>,
) -> Result<Json<ProposeObjectiveResp>, ApiError> {
    if body.topic.trim().is_empty() {
        return Err(ApiError::BadRequest("empty topic".to_string()));
    }
    let ai = state.ai.load_full();
    let proposal = engine::propose_objective(&ai, &body.topic).await?;
    Ok(Json(ProposeObjectiveResp {
        text: proposal.text,
        title: proposal.title,
    }))
}

/// The document's own metadata sidecar (§S12) — just its display name today.
/// Separate from `outline.json`: renaming a document is not a curriculum
/// change and must not read-modify-write the outline the `plan` move and
/// sub-node spawning both mutate (`Store::update_outline_file`).
#[derive(Serialize, Deserialize)]
struct DocumentMeta {
    name: String,
}

/// The document's display name, with the fallbacks a missing/blank sidecar
/// needs: a document created before §S12 has no `document.json`, so it falls
/// back to its topic, and finally to its id — never a blank entry in the
/// sidebar's document list.
fn document_name(state: &AppState, doc_id: &str, topic: &str) -> String {
    let stored = state
        .store
        .read_doc_file(doc_id, "document.json")
        .ok()
        .and_then(|json| serde_json::from_str::<DocumentMeta>(&json).ok())
        .map(|meta| meta.name);
    match stored {
        Some(name) if !name.trim().is_empty() => name,
        _ if !topic.trim().is_empty() => topic.to_string(),
        _ => doc_id.to_string(),
    }
}

#[derive(Deserialize)]
pub struct CreateReq {
    topic: String,
    /// The (possibly user-edited) objective from `propose_objective`. Empty
    /// only for a caller that skipped confirmation entirely (e.g. a direct
    /// API call) — falls back to the raw topic so the objective anchor is
    /// never blank, even though the UI always confirms (§6.1/§S4).
    #[serde(default)]
    objective_text: String,
    /// Document display name from `propose_objective` (§S12). Empty falls
    /// back to the topic, same convention as `objective_text`.
    #[serde(default)]
    name: String,
}

/// One outline item as shown to the client (§S5): the graph's gate, resolved.
/// `state` is `"locked"` (a prerequisite isn't `Demonstrated` yet and the
/// item was never touched), `"available"` (prerequisites met, or already
/// attempted/skipped — i.e. still worth showing as reachable), or
/// `"demonstrated"`.
#[derive(Serialize)]
pub struct OutlineItemView {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) state: &'static str,
}

#[derive(Serialize)]
pub struct OutlineResp {
    pub(super) items: Vec<OutlineItemView>,
    /// §S5 revisit scheduler: the currently-skipped node deferred longest,
    /// if any (`events::aggregate::revisit_suggestion`) — a spacing
    /// suggestion, not a mandate; the learner can pick any other reachable
    /// item instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) suggested_revisit: Option<String>,
}

/// §S5 revisit scheduler, wired to the response: see
/// `events::aggregate::revisit_suggestion` for the actual heuristic.
pub(super) fn suggested_revisit(
    state: &AppState,
    doc_id: &str,
) -> Result<Option<String>, ApiError> {
    let event_log = state.store.event_log(doc_id)?;
    Ok(revisit_suggestion(
        event_log
            .iter()
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    ))
}

/// Resolves every item's gate state against the event log in one fold
/// (§S5) — never a separate `progress.json`, so there is nothing to desync
/// (see `events::aggregate::node_states`'s doc comment).
pub(super) fn outline_view(
    state: &AppState,
    doc_id: &str,
    outline: &Outline,
) -> Result<Vec<OutlineItemView>, ApiError> {
    let event_log = state.store.event_log(doc_id)?;
    let states = node_states(
        event_log
            .iter()
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    Ok(outline
        .items
        .iter()
        // Sub-nodes spawned from a question (§S8) are never shown in the
        // main-line sidebar or picked as the "next available" item — they're
        // reachable only inline, spliced into the parent that spawned them.
        .filter(|item| item.parent_id.is_none())
        .map(|item| {
            let view_state = match states.get(&item.id) {
                Some(NodeState::Demonstrated) => "demonstrated",
                Some(NodeState::Attempted) | Some(NodeState::Skipped) => "available",
                None => {
                    let unlocked = item
                        .prerequisites
                        .iter()
                        .all(|p| matches!(states.get(p), Some(NodeState::Demonstrated)));
                    if unlocked { "available" } else { "locked" }
                }
            };
            OutlineItemView {
                id: item.id.clone(),
                title: item.title.clone(),
                state: view_state,
            }
        })
        .collect())
}

#[derive(Serialize)]
pub struct CreateResp {
    doc_id: String,
    name: String,
    items: Vec<OutlineItemView>,
}

pub async fn create_document(
    State(state): State<AppState>,
    Json(body): Json<CreateReq>,
) -> Result<Json<CreateResp>, ApiError> {
    if body.topic.trim().is_empty() {
        return Err(ApiError::BadRequest("empty topic".to_string()));
    }
    let objective_text = if body.objective_text.trim().is_empty() {
        body.topic.clone()
    } else {
        body.objective_text.clone()
    };

    let ai = state.ai.load_full();
    let outline = engine::generate_outline(&ai, &body.topic, &objective_text).await?;
    let doc_id = engine::new_id();
    state.store.create_document(&doc_id)?;

    let mut objective_log = ObjectiveLog::default();
    objective_log.push(objective_text.clone(), ObjectiveSource::ColdStart);
    state.store.write_doc_file(
        &doc_id,
        "objective.json",
        &serde_json::to_string(&objective_log).unwrap_or_default(),
    )?;
    state.store.write_doc_file(
        &doc_id,
        "outline.json",
        &serde_json::to_string(&outline).unwrap_or_default(),
    )?;
    let name = if body.name.trim().is_empty() {
        body.topic.trim().to_string()
    } else {
        body.name.trim().to_string()
    };
    state.store.write_doc_file(
        &doc_id,
        "document.json",
        &serde_json::to_string(&DocumentMeta { name: name.clone() }).unwrap_or_default(),
    )?;
    // Acquire a grounding source in the background (§11/§14): the outline returns
    // immediately and content starts streaming ungrounded; citations appear once
    // the source is fetched and indexed. Never blocks the user. Seeded with the
    // confirmed objective text (strictly better grounding input than the raw
    // topic, and the only text guaranteed to already reflect the user's edits).
    spawn_acquisition(state.clone(), objective_text);
    let items = outline_view(&state, &doc_id, &outline)?;
    Ok(Json(CreateResp {
        doc_id,
        name,
        items,
    }))
}

#[derive(Deserialize)]
pub struct RenameReq {
    name: String,
}

#[derive(Serialize)]
pub struct RenameResp {
    name: String,
}

/// Renames the living document (§S12). The name is a label the learner picks,
/// nothing downstream reads it — it does not touch the outline, the objective,
/// or any node, so unlike an objective revision (§S4) it needs no version
/// chain: there is no learning trajectory to preserve in a title.
pub async fn rename_document(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Json(body): Json<RenameReq>,
) -> Result<Json<RenameResp>, ApiError> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("empty document name".to_string()));
    }
    // Reject a rename of something that is not a document (no outline) —
    // otherwise this would happily mint a `document.json` inside `corpus/`.
    state.store.read_doc_file(&doc_id, "outline.json")?;
    state.store.write_doc_file(
        &doc_id,
        "document.json",
        &serde_json::to_string(&DocumentMeta {
            name: name.to_string(),
        })
        .unwrap_or_default(),
    )?;
    Ok(Json(RenameResp {
        name: name.to_string(),
    }))
}

/// Deletes a living document and everything in it (§S12).
///
/// Guarded the same way `rename_document` is — the presence of `outline.json`
/// is what makes a directory a document, and without that check this would be
/// a "delete any directory under the data dir" endpoint, `corpus/` and the
/// retrieval `index/` included. `DELETE`, never `GET` (§3.1).
///
/// The confirmation is the client's job: by the time the request arrives the
/// learner has already said yes, and a server that asked again would only be
/// able to ask about a document it can no longer show them.
pub async fn delete_document(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.store.read_doc_file(&doc_id, "outline.json")?;
    state.store.delete_document(&doc_id)?;
    Ok(Json(serde_json::json!({ "deleted": doc_id })))
}

/// One living document as shown on the cold-start screen (§S12).
#[derive(Serialize)]
pub struct DocumentSummary {
    doc_id: String,
    /// Display name (§S12) — `document_name`'s stored-name → topic → id chain.
    name: String,
    topic: String,
    /// Current objective text (§S4), for a label the learner recognizes —
    /// empty for a document whose `objective.json` predates §S4 or is
    /// unreadable, same graceful-degradation convention as `topic_and_title`.
    objective: String,
    updated_ms: u64,
    total: usize,
    demonstrated: usize,
    /// The last main-line outline item that has an actual node file on disk —
    /// where reading left off. `None` for a document whose outline was
    /// created but whose first node was never generated; the client then
    /// shows the outline and waits for a click rather than spending tokens
    /// generating on page load (§12.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    resume_node_id: Option<String>,
}

/// Lists the living documents, most-recently-touched first (§4, §S12).
///
/// This is what makes the app reopen where the last session ended: documents
/// were always persisted under `<data-dir>/<doc-id>/`, but nothing ever read
/// them back — the client could only cold-start a new one. Read-only, so GET
/// is fine (§3.1).
///
/// A subdirectory without an `outline.json` is not a document and is skipped:
/// the data directory also holds `corpus/` and `index/` (§4/§10), which are
/// siblings of the document directories.
pub async fn list_documents(
    State(state): State<AppState>,
) -> Result<Json<Vec<DocumentSummary>>, ApiError> {
    let mut docs = Vec::new();
    for doc_id in state.store.list_documents()? {
        let Ok(outline_json) = state.store.read_doc_file(&doc_id, "outline.json") else {
            continue;
        };
        let Ok(outline) = serde_json::from_str::<Outline>(&outline_json) else {
            continue;
        };
        let items = outline_view(&state, &doc_id, &outline)?;
        let generated: std::collections::HashSet<String> = state
            .store
            .list_nodes(&doc_id)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let objective = state
            .store
            .read_doc_file(&doc_id, "objective.json")
            .ok()
            .and_then(|json| serde_json::from_str::<ObjectiveLog>(&json).ok())
            .and_then(|log| log.current().map(|v| v.text.clone()))
            .unwrap_or_default();

        docs.push(DocumentSummary {
            name: document_name(&state, &doc_id, &outline.topic),
            doc_id: doc_id.clone(),
            topic: outline.topic.clone(),
            objective,
            updated_ms: state.store.doc_updated_ms(&doc_id).unwrap_or(0),
            total: items.len(),
            demonstrated: items.iter().filter(|i| i.state == "demonstrated").count(),
            resume_node_id: items
                .iter()
                .rev()
                .map(|i| i.id.clone())
                .find(|id| generated.contains(id)),
        });
    }
    // Most recently touched first — the resume order.
    docs.sort_by_key(|d| std::cmp::Reverse(d.updated_ms));
    Ok(Json(docs))
}

#[derive(Deserialize)]
pub struct ReviseObjectiveReq {
    text: String,
}

#[derive(Serialize)]
pub struct ObjectiveResp {
    text: String,
    version: u32,
}

/// User-initiated objective revision (§5/§S4): "editável a qualquer
/// momento" — the objective is not only revised by an approved `plan` move,
/// the learner can edit it directly. Appends a new `ObjectiveLog` version
/// (never overwrites) and an `ObjectiveRevised` event.
pub async fn revise_objective(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Json(body): Json<ReviseObjectiveReq>,
) -> Result<Json<ObjectiveResp>, ApiError> {
    if body.text.trim().is_empty() {
        return Err(ApiError::BadRequest("empty objective".to_string()));
    }
    let log_json = state.store.read_doc_file(&doc_id, "objective.json")?;
    let mut log: ObjectiveLog =
        serde_json::from_str(&log_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    log.push(body.text.clone(), ObjectiveSource::UserEdit);
    state.store.write_doc_file(
        &doc_id,
        "objective.json",
        &serde_json::to_string(&log).unwrap_or_default(),
    )?;

    let version = log.current().map(|v| v.version).unwrap_or(0);
    let event_log = state.store.event_log(&doc_id)?;
    if let Err(e) = event_log.append(None, EventKind::ObjectiveRevised { version }) {
        eprintln!("event log append failed: {e}");
    }

    Ok(Json(ObjectiveResp {
        text: body.text,
        version,
    }))
}

#[derive(Serialize)]
pub struct ProfileResp {
    traits: Vec<String>,
    hypotheses: Vec<String>,
    /// Always-fresh, 0-LLM-token evidence table (§7) — recomputed on every
    /// read, never itself edited by the user (only what's distilled from it
    /// is: `traits`/`hypotheses`).
    evidence: String,
    distilled_through: u32,
}

/// Read-only inspection of the evidence profile (§7.1: "perfil inspecionável
/// e editável" — the human-in-the-loop fix for drift/bad compaction).
/// `traits`/`hypotheses` are empty before the first distillation.
pub async fn get_profile(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
) -> Result<Json<ProfileResp>, ApiError> {
    let event_log = state.store.event_log(&doc_id)?;
    let table = tactic_outcomes(
        event_log
            .iter()
            .map_err(|e| ApiError::BadRequest(e.to_string()))?,
    );
    let evidence = profile::evidence_table_text(&table);
    let projection = read_profile(&state, &doc_id).unwrap_or_default();
    Ok(Json(ProfileResp {
        traits: projection.traits,
        hypotheses: projection.hypotheses,
        evidence,
        distilled_through: projection.distilled_through,
    }))
}

#[derive(Deserialize)]
pub struct ReviseProfileReq {
    #[serde(default)]
    traits: Vec<String>,
    #[serde(default)]
    hypotheses: Vec<String>,
}

/// User-initiated profile edit (§7.1: "editável a qualquer momento") — the
/// same human-in-the-loop escape hatch `revise_objective` gives the
/// objective. Overwrites `profile.json` wholesale; `distilled_through` is
/// preserved as-is (a user edit is not a distillation, so it must not reset
/// the ~30-event threshold `should_distill` tracks).
pub async fn revise_profile(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Json(body): Json<ReviseProfileReq>,
) -> Result<Json<ProfileResp>, ApiError> {
    let distilled_through = read_profile(&state, &doc_id)
        .map(|p| p.distilled_through)
        .unwrap_or(0);
    let projection = ProfileProjection {
        traits: body.traits,
        hypotheses: body.hypotheses,
        distilled_through,
    };
    state.store.write_doc_file(
        &doc_id,
        "profile.json",
        &serde_json::to_string(&projection).unwrap_or_default(),
    )?;

    let event_log = state.store.event_log(&doc_id)?;
    let table = tactic_outcomes(
        event_log
            .iter()
            .map_err(|e| ApiError::BadRequest(e.to_string()))?,
    );
    let evidence = profile::evidence_table_text(&table);
    Ok(Json(ProfileResp {
        traits: projection.traits,
        hypotheses: projection.hypotheses,
        evidence,
        distilled_through: projection.distilled_through,
    }))
}

/// Background source acquisition + reindex (§11.1/§10). No-op when grounding is
/// disabled (no retriever). Failures are logged, never surfaced to the user — an
/// ungrounded document is still fully usable.
fn spawn_acquisition(state: AppState, topic: String) {
    let Some(retriever) = state.retriever.clone() else {
        return;
    };
    let source = state.source.clone();
    let corpus = state.corpus.clone();
    tokio::spawn(async move {
        let hit = match source.search(&topic).await {
            Ok(hits) => match hits.into_iter().next() {
                Some(h) => h,
                None => return,
            },
            Err(e) => {
                eprintln!("acquisition search failed: {e}");
                return;
            }
        };
        let doc = match source.fetch(&hit).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("acquisition fetch failed: {e}");
                return;
            }
        };
        match corpus.store(&doc) {
            Ok(true) => {
                // Reindex so the new source is retrievable for grounding.
                let mut r = retriever.write().await;
                if let Err(e) = r.reindex(&corpus) {
                    eprintln!("reindex after acquisition failed: {e}");
                } else {
                    eprintln!("grounded on \"{}\" ({} chunks)", doc.meta.title, r.len());
                }
            }
            Ok(false) => {} // already in the corpus and indexed
            Err(e) => eprintln!("corpus store failed: {e}"),
        }
    });
}

/// The runtime acquisition backend (§11.1): OpenStax OER by default.
pub fn build_source() -> Source {
    Source::openstax()
}
