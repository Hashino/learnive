use super::reading::read_profile;
use super::*;

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::retrieval::Retriever;
use crate::source::Corpus;

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
    /// The learner's confirmed learn/review/skip choices over the tree
    /// `propose_prerequisites` returned (§S15) — empty for a caller that
    /// skipped that screen entirely (e.g. a direct API call, or an objective
    /// the learner confirmed with no prerequisites proposed), same
    /// graceful-degradation convention as `objective_text`/`name`.
    #[serde(default)]
    prerequisites: Vec<ConfirmedPrereqNode>,
}

// ---------------------------------------------------------------------------
// Prerequisites (§S15): the agent proposes a tree of prerequisite concepts
// for the confirmed objective, resolved against every other document the
// learner has (already `Demonstrated` there → suggested `review`); the
// learner confirms learn/review/skip per branch before anything generates.
// Two stateless calls bracket `create_document`, mirroring `propose_
// objective`: nothing here is persisted until the learner confirms.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ProposePrereqReq {
    topic: String,
    #[serde(default)]
    objective_text: String,
}

/// Where an already-`Demonstrated` match was found (§S15 "review" default).
/// Informational only in this slice — the confirm step below always
/// generates a fresh, local, abbreviated review node regardless of `known`;
/// it does not read from or write into the matched document. A true shared
/// node (`{owner_doc_id, node_id}` pointer, edits visible in both documents)
/// is PLAN.md's S15b, deliberately deferred — the riskiest, most invasive
/// part of this slice (a `Store` read/write indirection touching every
/// existing document), sequenced after the tree+toggle mechanism is proven
/// end to end on its own.
#[derive(Serialize, Clone)]
pub struct KnownMatch {
    doc_id: String,
    doc_name: String,
}

#[derive(Serialize, Clone)]
pub struct ProposedPrereqNode {
    /// Freshly minted here — becomes the real `OutlineItem::id` if the
    /// learner confirms this node (§S15, avoids a second id-remapping pass
    /// between propose and confirm).
    id: String,
    title: String,
    /// `"review"` when an already-`Demonstrated` match was found elsewhere,
    /// `"learn"` otherwise — a DEFAULT the client shows pre-selected, not a
    /// lock: the learner can freely override to `skip`/`learn`/`review` on
    /// any node (a false positive here must have an escape hatch).
    suggested: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    known: Option<KnownMatch>,
    children: Vec<ProposedPrereqNode>,
}

#[derive(Serialize)]
pub struct ProposePrereqResp {
    tree: Vec<ProposedPrereqNode>,
}

/// Proposes the prerequisite tree for a (possibly not-yet-created)
/// objective (§S15) — stateless, like `propose_objective`; nothing is
/// persisted until `create_document` receives the learner's confirmed
/// choices back.
pub async fn propose_prerequisites(
    State(state): State<AppState>,
    Json(body): Json<ProposePrereqReq>,
) -> Result<Json<ProposePrereqResp>, ApiError> {
    if body.topic.trim().is_empty() {
        return Err(ApiError::BadRequest("empty topic".to_string()));
    }
    let objective = if body.objective_text.trim().is_empty() {
        body.topic.clone()
    } else {
        body.objective_text.clone()
    };
    let ai = state.ai.load_full();
    // A parse failure degrades to an empty tree rather than a 502. The
    // original motivating case for this (a reasoning-heavy fast-tier model
    // routing its entire output through the streaming `reasoning` delta and
    // never reaching a final `content` chunk) is now fixed structurally —
    // `engine::collect` asks for `stream: false` instead of buffering SSE
    // (§14: this call is never rendered live anyway), which reliably gets a
    // clean split between reasoning and content. This branch stays as a
    // backstop for the residual case (the model answers with malformed JSON
    // despite the contract) rather than the primary fix. `propose_prerequisites`'s
    // own doc comment already treats an empty tree as a normal, common
    // answer — a failure to read one is indistinguishable in effect from a
    // model that genuinely found no prerequisites, so this is the same safe
    // default the parser already applies to a well-formed `[]`, not new
    // fallback behavior. A real provider/network failure
    // (`EngineError::Provider`) still surfaces: it means the `Ai` this call
    // shares with `generate_outline` is unusable, and that failure will
    // resurface moments later regardless.
    let forest = match engine::propose_prerequisites(&ai, &body.topic, &objective).await {
        Ok(forest) => forest,
        Err(crate::engine::EngineError::Parse(msg)) => {
            eprintln!(
                "propose_prerequisites: model response could not be read ({msg}) — \
                 degrading to an empty prerequisite tree"
            );
            Vec::new()
        }
        Err(e) => return Err(e.into()),
    };
    let known = known_concepts(&state)?;
    let tree = resolve_prereq_forest(&state, &forest, &known).await;
    Ok(Json(ProposePrereqResp { tree }))
}

/// One `Demonstrated` concept already in some document — the candidate pool
/// `resolve_prereq_forest` matches proposed prerequisites against.
struct KnownConcept {
    doc_id: String,
    doc_name: String,
    title: String,
}

/// Gathers every `Demonstrated` outline item across every document (§S15
/// cross-document detection). No persisted index (§4: files are the source
/// of truth, an index is a rebuildable cache) — at the node counts this app
/// deals with, embedding this set fresh on every proposal call is honest and
/// simple; a persisted title/objective index is the natural next step if
/// this ever shows up as slow.
fn known_concepts(state: &AppState) -> Result<Vec<KnownConcept>, ApiError> {
    let mut out = Vec::new();
    for doc_id in state.store.list_documents()? {
        let Ok(outline_json) = state.store.read_doc_file(&doc_id, "outline.json") else {
            continue;
        };
        let Ok(outline) = serde_json::from_str::<Outline>(&outline_json) else {
            continue;
        };
        let Ok(event_log) = state.store.event_log(&doc_id) else {
            continue;
        };
        let Ok(iter) = event_log.iter() else {
            continue;
        };
        let states = node_states(iter);
        let doc_name = document_name(state, &doc_id, &outline.topic);
        for item in &outline.items {
            if matches!(states.get(&item.id), Some(NodeState::Demonstrated)) {
                out.push(KnownConcept {
                    doc_id: doc_id.clone(),
                    doc_name: doc_name.clone(),
                    title: item.title.clone(),
                });
            }
        }
    }
    Ok(out)
}

/// A match is asserted only above this cosine similarity (§S15). Biased
/// high on purpose: because a suggested `review` disables nothing (the
/// toggle stays freely overridable), the failure mode of a too-low
/// threshold isn't a stuck UI, it's a wrong DEFAULT the learner might not
/// notice and skip material they don't actually know — better to miss a
/// real match (falls back to `learn`, the safe default) than assert a false
/// one. `retrieval`'s own corpus floor (`min_score: 0.01`) is explicitly
/// flagged in CLAUDE.md as no floor at all and untuned — deliberately not
/// reused here.
const PREREQ_MATCH_THRESHOLD: f32 = 0.86;

/// Resolves a proposed prerequisite forest against known `Demonstrated`
/// concepts (§S15): embeds every known title once, then every proposed
/// title once per node, comparing by cosine similarity. `None` embedder
/// (grounding disabled — `state.retriever` unset) degrades to every node
/// suggested `learn`, the same graceful degradation `acquire` already uses
/// for a missing retriever.
async fn resolve_prereq_forest(
    state: &AppState,
    forest: &[PrereqNode],
    known: &[KnownConcept],
) -> Vec<ProposedPrereqNode> {
    let embedder = match &state.retriever {
        Some(r) => Some(r.read().await.embedder().clone()),
        None => None,
    };
    let known_vecs = embedder.as_ref().map(|e| {
        let titles: Vec<String> = known.iter().map(|k| k.title.clone()).collect();
        e.embed_batch(&titles)
    });
    forest
        .iter()
        .map(|node| resolve_prereq_node(node, known, known_vecs.as_deref(), embedder.as_ref()))
        .collect()
}

fn resolve_prereq_node(
    node: &PrereqNode,
    known: &[KnownConcept],
    known_vecs: Option<&[Vec<f32>]>,
    embedder: Option<&crate::retrieval::Embedder>,
) -> ProposedPrereqNode {
    let matched = match (embedder, known_vecs) {
        (Some(embedder), Some(known_vecs)) if !known.is_empty() => {
            let qv = embedder.embed(&node.title);
            known
                .iter()
                .zip(known_vecs.iter())
                .map(|(k, v)| (k, crate::retrieval::cosine(&qv, v)))
                .filter(|(_, score)| *score >= PREREQ_MATCH_THRESHOLD)
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(k, _)| k)
        }
        _ => None,
    };
    let (suggested, known_view) = match matched {
        Some(k) => (
            "review",
            Some(KnownMatch {
                doc_id: k.doc_id.clone(),
                doc_name: k.doc_name.clone(),
            }),
        ),
        None => ("learn", None),
    };
    ProposedPrereqNode {
        id: engine::new_id(),
        title: node.title.clone(),
        suggested,
        known: known_view,
        children: node
            .children
            .iter()
            .map(|c| resolve_prereq_node(c, known, known_vecs, embedder))
            .collect(),
    }
}

/// The learner's confirmed choice for one prerequisite-tree node (§S15
/// toggle screen), sent back to `create_document`. `id`/`title` round-trip
/// verbatim from `ProposedPrereqNode` — the server never re-derives them.
#[derive(Deserialize, Clone)]
pub struct ConfirmedPrereqNode {
    id: String,
    title: String,
    action: PrereqAction,
    #[serde(default)]
    children: Vec<ConfirmedPrereqNode>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PrereqAction {
    Learn,
    Review,
    Skip,
}

/// Turns the learner's confirmed prerequisite tree into `OutlineItem`s
/// (§S15) — mirrors the toggle screen's own cascade rules:
///
/// - `skip` cascades to the WHOLE branch: the node and every descendant are
///   materialized but marked for a `NodeSkipped` event each (pushed onto
///   `to_skip`, appended by the caller once the document/event log exist) —
///   nothing is ever generated for any of them (§S5's existing mechanism).
/// - `review` also cascades, the opposite way: descendants are NOT
///   materialized at all — only this node, in `NodeMode::Review`. Its own
///   short pass is meant to cover the whole branch, so there is nothing left
///   for a hidden child to gate or be gated by.
/// - `learn` recurses normally; the node's `prerequisites` become its
///   children's ids (whatever their own action) so it only opens once every
///   child reaches `Demonstrated` — `Skipped` satisfies that gate too
///   (`api::reading::prepare`), so a skipped branch never locks its parent
///   forever.
///
/// Returns this level's minted ids, for the caller to wire as the
/// PARENT's `prerequisites` (top-level call: the objective's own first
/// main-line item).
fn materialize_prereq_tree(
    nodes: &[ConfirmedPrereqNode],
    parent_id: Option<&str>,
    items: &mut Vec<OutlineItem>,
    to_skip: &mut Vec<String>,
) -> Vec<String> {
    nodes
        .iter()
        .map(|node| materialize_prereq_node(node, parent_id, items, to_skip))
        .collect()
}

fn materialize_prereq_node(
    node: &ConfirmedPrereqNode,
    parent_id: Option<&str>,
    items: &mut Vec<OutlineItem>,
    to_skip: &mut Vec<String>,
) -> String {
    match node.action {
        PrereqAction::Skip => {
            to_skip.push(node.id.clone());
            materialize_skip_subtree(&node.children, Some(node.id.as_str()), items, to_skip);
            items.push(OutlineItem {
                id: node.id.clone(),
                title: node.title.clone(),
                prerequisites: Vec::new(),
                parent_id: parent_id.map(String::from),
                mode: NodeMode::Learn,
            });
        }
        PrereqAction::Review => {
            items.push(OutlineItem {
                id: node.id.clone(),
                title: node.title.clone(),
                prerequisites: Vec::new(),
                parent_id: parent_id.map(String::from),
                mode: NodeMode::Review,
            });
        }
        PrereqAction::Learn => {
            let child_ids =
                materialize_prereq_tree(&node.children, Some(node.id.as_str()), items, to_skip);
            items.push(OutlineItem {
                id: node.id.clone(),
                title: node.title.clone(),
                prerequisites: child_ids,
                parent_id: parent_id.map(String::from),
                mode: NodeMode::Learn,
            });
        }
    }
    node.id.clone()
}

/// Every descendant of a `skip`ped node is skipped too, unconditionally —
/// `action` on a descendant is ignored once an ancestor is skipped, the
/// literal "um clique, nada daquele subnodo ou dos seus próprios filhos é
/// gerado" from the co-design.
fn materialize_skip_subtree(
    nodes: &[ConfirmedPrereqNode],
    parent_id: Option<&str>,
    items: &mut Vec<OutlineItem>,
    to_skip: &mut Vec<String>,
) {
    for node in nodes {
        to_skip.push(node.id.clone());
        materialize_skip_subtree(&node.children, Some(node.id.as_str()), items, to_skip);
        items.push(OutlineItem {
            id: node.id.clone(),
            title: node.title.clone(),
            prerequisites: Vec::new(),
            parent_id: parent_id.map(String::from),
            mode: NodeMode::Learn,
        });
    }
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
    /// §S15 sidebar tree: the item this one is subordinate to (a decomposed
    /// prerequisite, or a question-spawned elaboration, §S8) — `None` for
    /// every item on the document's main line. The client nests on this;
    /// "next available" advance must still filter to `None` explicitly,
    /// since this view is no longer pre-filtered to the main line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) parent_id: Option<String>,
    /// §S15 learn/review/skip: `"review"` when this node was confirmed as a
    /// short pass rather than full learning — omitted (defaults to `"learn"`
    /// client-side) for every ordinary node, so existing/pre-S15 documents
    /// don't grow a field their outline never set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) mode: Option<&'static str>,
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
        // §S15: every item is shown now, tree-nested by `parent_id` client-
        // side — a sub-node (question-spawned §S8, or a decomposed
        // prerequisite §S15) is navigation/depth, not hidden. "Next
        // available" advance and `resume_node_id` still need main-line-only
        // (`parent_id.is_none()`); they filter explicitly at their own call
        // sites now that this view doesn't pre-filter for them.
        .map(|item| {
            let view_state = match states.get(&item.id) {
                Some(NodeState::Demonstrated) => "demonstrated",
                Some(NodeState::Attempted) | Some(NodeState::Skipped) => "available",
                None => {
                    // §S15: a `Skipped` prerequisite satisfies the gate too
                    // — see `api::reading::prepare`'s matching check.
                    let unlocked = item.prerequisites.iter().all(|p| {
                        matches!(
                            states.get(p),
                            Some(NodeState::Demonstrated) | Some(NodeState::Skipped)
                        )
                    });
                    if unlocked { "available" } else { "locked" }
                }
            };
            OutlineItemView {
                id: item.id.clone(),
                title: item.title.clone(),
                state: view_state,
                parent_id: item.parent_id.clone(),
                mode: match item.mode {
                    NodeMode::Review => Some("review"),
                    NodeMode::Learn => None,
                },
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
    let mut outline = engine::generate_outline(&ai, &body.topic, &objective_text).await?;

    // §S15: graft the learner's confirmed prerequisite tree onto the main
    // line's own first item — it only becomes available once every root of
    // the tree is `Demonstrated` (or `Skipped`, `prepare`'s gate check), and
    // the roots nest under it in the sidebar (`parent_id`), the same
    // "subordinate, doesn't gate on its own" pointer §S8 already uses for a
    // question-spawned elaboration.
    let main_first_id = outline.items.first().map(|i| i.id.clone());
    let mut prereq_items = Vec::new();
    let mut to_skip = Vec::new();
    let prereq_root_ids = materialize_prereq_tree(
        &body.prerequisites,
        main_first_id.as_deref(),
        &mut prereq_items,
        &mut to_skip,
    );
    if let Some(first) = outline.items.first_mut() {
        first.prerequisites = prereq_root_ids;
    }
    outline.items.extend(prereq_items);

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
    if !to_skip.is_empty() {
        let event_log = state.store.event_log(&doc_id)?;
        for id in &to_skip {
            if let Err(e) = event_log.append(Some(id), EventKind::NodeSkipped) {
                eprintln!("event log append failed: {e}");
            }
        }
    }
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
            // §S15: `items` now includes the whole tree, not just the main
            // line — "where reading left off" must still walk the main line
            // only (`parent_id.is_none()`), or a resume could land on a
            // prerequisite/question sub-node instead of the document's own
            // narrative.
            total: items.iter().filter(|i| i.parent_id.is_none()).count(),
            demonstrated: items
                .iter()
                .filter(|i| i.parent_id.is_none() && i.state == "demonstrated")
                .count(),
            resume_node_id: items
                .iter()
                .filter(|i| i.parent_id.is_none())
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

/// Outcome of `acquire` — enough for a caller to tell the learner what
/// happened (the `research` move, §S13) or just log it (cold-start's
/// background call, `spawn_acquisition`).
pub(super) struct AcquisitionOutcome {
    pub grounded: bool,
    pub source_title: Option<String>,
}

/// Runs source acquisition + reindex for `query_hint` (§11.1/§10): derives a
/// short catalog-search subject phrase (`engine::propose_search_subject` —
/// the backend matches TITLES, not semantic intent, so a full sentence
/// reliably misses even when the catalog covers the subject, confirmed live
/// against OpenStax), then tries each configured `Source` in the §11.1
/// fallback order (OER first, Wikipedia last) with that phrase and, failing
/// that, the raw hint verbatim. Returns as soon as one attempt lands.
///
/// No-op (returns not-grounded) when grounding is disabled (no retriever).
/// Awaited directly by the `research` move (generation must not proceed
/// without knowing whether it landed); wrapped in `tokio::spawn` by
/// `spawn_acquisition` for the fire-and-forget cold-start case, where an
/// ungrounded document is still fully usable and nothing is waiting on it.
pub(super) async fn acquire(state: &AppState, query_hint: &str) -> AcquisitionOutcome {
    let Some(retriever) = &state.retriever else {
        return AcquisitionOutcome {
            grounded: false,
            source_title: None,
        };
    };
    let ai = state.ai.load_full();
    let subject = engine::propose_search_subject(&ai, query_hint)
        .await
        .unwrap_or_default();

    let mut queries = Vec::with_capacity(2);
    if !subject.trim().is_empty() {
        queries.push(subject.as_str());
    }
    if !query_hint.trim().is_empty() && query_hint != subject {
        queries.push(query_hint);
    }

    for source in [state.source.as_ref(), state.fallback_source.as_ref()] {
        for query in &queries {
            if let Some(title) = try_acquire_from(source, &state.corpus, retriever, query).await {
                return AcquisitionOutcome {
                    grounded: true,
                    source_title: Some(title),
                };
            }
        }
    }
    AcquisitionOutcome {
        grounded: false,
        source_title: None,
    }
}

/// One search→fetch→store→reindex attempt against a single backend. `None`
/// on any failure or empty result — every failure mode here is recoverable
/// by trying the next query/backend in `acquire`'s chain, so this only logs,
/// never surfaces an error to the caller.
async fn try_acquire_from(
    source: &Source,
    corpus: &Corpus,
    retriever: &Arc<RwLock<Retriever>>,
    query: &str,
) -> Option<String> {
    let hit = match source.search(query).await {
        Ok(hits) => hits.into_iter().next()?,
        Err(e) => {
            eprintln!("acquisition search failed: {e}");
            return None;
        }
    };
    let doc = match source.fetch(&hit).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("acquisition fetch failed: {e}");
            return None;
        }
    };
    let title = doc.meta.title.clone();
    match corpus.store(&doc) {
        Ok(true) => {
            let mut r = retriever.write().await;
            if let Err(e) = r.reindex(corpus) {
                eprintln!("reindex after acquisition failed: {e}");
                return None;
            }
            eprintln!("grounded on \"{title}\" ({} chunks)", r.len());
            Some(title)
        }
        // Already in the corpus and indexed — still a successful ground.
        Ok(false) => Some(title),
        Err(e) => {
            eprintln!("corpus store failed: {e}");
            None
        }
    }
}

/// Background source acquisition (§11.1/§10), fire-and-forget: cold start
/// returns the outline immediately and content starts streaming ungrounded;
/// citations appear once `acquire` lands. Failures are logged only (see
/// `acquire`'s doc comment) — an ungrounded document is still fully usable.
fn spawn_acquisition(state: AppState, topic: String) {
    if state.retriever.is_none() {
        return;
    }
    tokio::spawn(async move {
        acquire(&state, &topic).await;
    });
}

/// The runtime acquisition backends (§11.1): OpenStax OER by default,
/// Wikipedia as the free/keyless fallback (`AppState::fallback_source`).
pub fn build_source() -> Source {
    Source::openstax()
}

/// The §11.1 fallback tier — see `source::wikipedia` module docs for why
/// this backend and not a general web-search API.
pub fn build_fallback_source() -> Source {
    Source::wikipedia()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learn(id: &str, title: &str, children: Vec<ConfirmedPrereqNode>) -> ConfirmedPrereqNode {
        ConfirmedPrereqNode {
            id: id.to_string(),
            title: title.to_string(),
            action: PrereqAction::Learn,
            children,
        }
    }

    fn leaf(id: &str, title: &str, action: PrereqAction) -> ConfirmedPrereqNode {
        ConfirmedPrereqNode {
            id: id.to_string(),
            title: title.to_string(),
            action,
            children: Vec::new(),
        }
    }

    /// §S15: a `learn` node with children gets its children's ids as its
    /// own `prerequisites` (only opens once every child is `Demonstrated`),
    /// and every child is materialized regardless of its own action.
    #[test]
    fn learn_recurses_and_gates_on_children() {
        let tree = vec![learn(
            "p1",
            "Derivatives",
            vec![
                leaf("c1", "Product rule", PrereqAction::Learn),
                leaf("c2", "Chain rule", PrereqAction::Learn),
            ],
        )];
        let mut items = Vec::new();
        let mut to_skip = Vec::new();
        let roots = materialize_prereq_tree(&tree, None, &mut items, &mut to_skip);
        assert_eq!(roots, vec!["p1".to_string()]);
        assert!(to_skip.is_empty());
        assert_eq!(items.len(), 3);
        let parent = items.iter().find(|i| i.id == "p1").unwrap();
        assert_eq!(
            parent.prerequisites,
            vec!["c1".to_string(), "c2".to_string()]
        );
        assert_eq!(parent.parent_id, None);
        for child_id in ["c1", "c2"] {
            let child = items.iter().find(|i| i.id == child_id).unwrap();
            assert_eq!(child.parent_id.as_deref(), Some("p1"));
            assert!(child.prerequisites.is_empty());
        }
    }

    /// §S15: `skip` cascades to the whole branch — every descendant is
    /// materialized (so the sidebar can show it struck through) but queued
    /// for a `NodeSkipped` event, regardless of what action it was
    /// individually given.
    #[test]
    fn skip_cascades_to_the_whole_branch() {
        let tree = vec![ConfirmedPrereqNode {
            id: "p1".to_string(),
            title: "Limits".to_string(),
            action: PrereqAction::Skip,
            children: vec![leaf("c1", "Epsilon-delta", PrereqAction::Learn)],
        }];
        let mut items = Vec::new();
        let mut to_skip = Vec::new();
        materialize_prereq_tree(&tree, None, &mut items, &mut to_skip);
        assert_eq!(items.len(), 2);
        let mut skipped = to_skip.clone();
        skipped.sort();
        assert_eq!(skipped, vec!["c1".to_string(), "p1".to_string()]);
    }

    /// §S15: `review` cascades the opposite way — the branch's descendants
    /// are never materialized at all, only the node itself, in
    /// `NodeMode::Review`.
    #[test]
    fn review_omits_children_entirely() {
        let tree = vec![ConfirmedPrereqNode {
            id: "p1".to_string(),
            title: "Algebra basics".to_string(),
            action: PrereqAction::Review,
            children: vec![leaf("c1", "Factoring", PrereqAction::Learn)],
        }];
        let mut items = Vec::new();
        let mut to_skip = Vec::new();
        materialize_prereq_tree(&tree, None, &mut items, &mut to_skip);
        assert_eq!(items.len(), 1);
        assert!(to_skip.is_empty());
        assert_eq!(items[0].id, "p1");
        assert_eq!(items[0].mode, NodeMode::Review);
        assert!(items[0].prerequisites.is_empty());
    }
}
