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

/// The document's own metadata sidecar (§S12) — its display name, plus
/// (added for QA/debugging traceability) the build's short git SHA at the
/// moment the document was *created*. Separate from `outline.json`:
/// renaming a document is not a curriculum change and must not
/// read-modify-write the outline the `plan` move and sub-node spawning both
/// mutate (`Store::update_outline_file`).
///
/// `built_with` is deliberately distinct from each node's own stamp
/// (`engine::wrap_article`): a document's nodes are generated incrementally
/// over time, possibly across several app versions, while the document
/// itself is created exactly once — this field records that one moment, not
/// a running "current version." Optional/defaulted so a document created
/// before this field existed still deserializes cleanly.
#[derive(Serialize, Deserialize)]
struct DocumentMeta {
    name: String,
    #[serde(default)]
    built_with: Option<String>,
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
    /// The learner's confirmed outline tree (§S15/§S16, unified
    /// 2026-08-19): `propose_outline`'s response, round-tripped back with
    /// per-node learn/review/skip choices. Every element but the LAST of
    /// the top-level array is a prerequisite; the LAST is the objective's
    /// own topic (client-locked to `learn`, see `ProposedNode`'s doc
    /// comment). Empty for a caller that skipped that screen entirely (e.g.
    /// a direct API call) — degrades to generating a fresh tree here and
    /// auto-confirming every node `learn`, same graceful-degradation
    /// convention as `objective_text`/`name` above.
    #[serde(default)]
    nodes: Vec<ConfirmedNode>,
}

// ---------------------------------------------------------------------------
// Outline proposal (§S15/§S16, unified 2026-08-19): the agent proposes the
// FULL outline in one tree — prerequisite background the objective
// presupposes, then the objective's own content, as one ordered sequence —
// resolved against every other document the learner has (already
// `Demonstrated` there → suggested `review`); the learner confirms
// learn/review/skip per branch before anything generates (the objective's
// own subtree stays locked to `learn` client-side — it's the requested
// topic, not background). A stateless call, like `propose_objective`:
// nothing here is persisted until the learner confirms via `create_document`.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ProposeOutlineReq {
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

/// One node of the proposed outline tree, resolved against known concepts
/// (§S15) — see `engine::ProposedOutlineNode`'s doc comment for the array
/// contract (every element but the last of a sibling list at the TOP level
/// is a prerequisite, the last is the objective's own topic). The client
/// renders every node the same way structurally, but locks the toggle to
/// `learn` for the last top-level node and its whole subtree — it can't be
/// skipped/reviewed, since it IS the requested topic.
#[derive(Serialize, Clone)]
pub struct ProposedNode {
    /// Freshly minted here — becomes the real `OutlineItem::id` if the
    /// learner confirms this node (§S15, avoids a second id-remapping pass
    /// between propose and confirm).
    id: String,
    title: String,
    /// `"review"` when an already-`Demonstrated` match was found elsewhere,
    /// `"learn"` otherwise — a DEFAULT the client shows pre-selected, not a
    /// lock: the learner can freely override to `skip`/`learn`/`review` on
    /// any prerequisite node (a false positive here must have an escape
    /// hatch). Ignored by the client for the objective's own subtree, which
    /// stays locked to `learn` regardless.
    suggested: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    known: Option<KnownMatch>,
    children: Vec<ProposedNode>,
}

#[derive(Serialize)]
pub struct ProposeOutlineResp {
    /// One ordered sequence — see `ProposedNode`'s doc comment.
    nodes: Vec<ProposedNode>,
}

/// Proposes the FULL outline tree for a (possibly not-yet-created)
/// objective (§S15/§S16, unified 2026-08-19) — stateless, like
/// `propose_objective`; nothing is persisted until `create_document`
/// receives the learner's confirmed choices back. A single call now (see
/// `engine::propose_outline`'s doc comment for why the old two-call design —
/// a separately-generated prerequisite forest grafted onto a
/// separately-generated main line — was the actual bug behind a live report,
/// "Funções Recursivas" prerequisites nested under an unrelated main-line
/// item), so there is no longer a parse-failure-degrades-to-empty branch
/// either: the objective's own node is always at least one element of the
/// response, so any failure here is a hard error, the same as the old
/// main-line generation's failure mode.
pub async fn propose_outline(
    State(state): State<AppState>,
    Json(body): Json<ProposeOutlineReq>,
) -> Result<Json<ProposeOutlineResp>, ApiError> {
    if body.topic.trim().is_empty() {
        return Err(ApiError::BadRequest("empty topic".to_string()));
    }
    let objective = if body.objective_text.trim().is_empty() {
        body.topic.clone()
    } else {
        body.objective_text.clone()
    };
    let ai = state.ai.load_full();
    let tree = engine::propose_outline(&ai, &body.topic, &objective).await?;
    let known = known_concepts(&state)?;
    let nodes = resolve_outline_forest(&state, &tree, &known).await;
    Ok(Json(ProposeOutlineResp { nodes }))
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

/// Resolves a proposed outline tree against known `Demonstrated` concepts
/// (§S15/§S16): embeds every known title once, then every proposed title
/// once per node — every node, prerequisite or objective alike, there is no
/// structural reason to skip the objective's own subtree here even though
/// the client locks its toggle regardless — comparing by cosine similarity.
/// `None` embedder (grounding disabled — `state.retriever` unset) degrades
/// to every node suggested `learn`, the same graceful degradation `acquire`
/// already uses for a missing retriever.
async fn resolve_outline_forest(
    state: &AppState,
    tree: &[engine::ProposedOutlineNode],
    known: &[KnownConcept],
) -> Vec<ProposedNode> {
    let embedder = match &state.retriever {
        Some(r) => Some(r.read().await.embedder().clone()),
        None => None,
    };
    let known_vecs = embedder.as_ref().map(|e| {
        let titles: Vec<String> = known.iter().map(|k| k.title.clone()).collect();
        e.embed_batch(&titles)
    });
    tree.iter()
        .map(|node| resolve_outline_node(node, known, known_vecs.as_deref(), embedder.as_ref()))
        .collect()
}

fn resolve_outline_node(
    node: &engine::ProposedOutlineNode,
    known: &[KnownConcept],
    known_vecs: Option<&[Vec<f32>]>,
    embedder: Option<&crate::retrieval::Embedder>,
) -> ProposedNode {
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
    ProposedNode {
        id: engine::new_id(),
        title: node.title.clone(),
        suggested,
        known: known_view,
        children: node
            .children
            .iter()
            .map(|c| resolve_outline_node(c, known, known_vecs, embedder))
            .collect(),
    }
}

/// The learner's confirmed choice for one outline-tree node (§S15/§S16
/// toggle screen), sent back to `create_document`. `id`/`title` round-trip
/// verbatim from `ProposedNode` — the server never re-derives them.
#[derive(Deserialize, Clone)]
pub struct ConfirmedNode {
    id: String,
    title: String,
    action: PrereqAction,
    #[serde(default)]
    children: Vec<ConfirmedNode>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PrereqAction {
    Learn,
    Review,
    Skip,
}

/// Turns the learner's confirmed outline tree into `OutlineItem`s (§S15/
/// §S16, unified 2026-08-19) — mirrors the confirmation screen's own
/// cascade rules, uniformly for prerequisite and objective nodes alike
/// (the objective's own node is simply the LAST element of the top-level
/// `nodes` list, its toggle client-locked to `learn` — nothing here treats
/// it specially, see `ProposedNode`'s doc comment):
///
/// - `skip` cascades to the WHOLE branch: the node and every descendant are
///   DISCARDED, never materialized as an `OutlineItem` at all — only queued
///   for a `NodeSkipped` event each (pushed onto `to_skip`, appended by the
///   caller once the document/event log exist), so a node gated on this id
///   can still unlock. Nothing about a skipped branch is ever shown or
///   generated (§S5) — an id that never enters `outline.items` can't be
///   looked up by `api::reading::prepare` or offered by a "next available"
///   search in the first place, deliberately stronger than tagging-and-
///   hiding (see `OutlineItem::mode`'s doc comment for why that weaker form
///   was tried and failed live, 2026-08-18).
/// - `review` also cascades, the opposite way: descendants are NOT
///   materialized at all — only this node, in `NodeMode::Review`.
/// - `learn` recurses normally: children are materialized (nested under
///   this node via `parent_id` — genuine decomposition, untouched by the
///   sequencing change below) and chained to each other in confirmed
///   order; this node's own `prerequisites` become its LAST child's id
///   (available once the whole decomposition is `Demonstrated`), or,
///   childless, whatever gate is threaded in from its own preceding
///   sibling.
///
/// SEQUENCING (`incoming_gate`, threaded through every level, not just the
/// top): every node in a sibling list — prerequisite roots, the objective's
/// own root, or one node's own children — also requires the id of the
/// sibling immediately before it, so the whole confirmed tree reads as one
/// continuous chain: prerequisite 1 -> prerequisite 2 -> ... -> the
/// objective's own (possibly decomposed) topic. This replaces the old
/// design, where every prerequisite root nested under the main line's first
/// item via `parent_id` and gated it in PARALLEL — the actual bug behind a
/// live report (2026-08-19, "Funções Recursivas"): the roots rendered in
/// the sidebar as false decomposition of an unrelated main-line item
/// instead of the sequential background topics they were.
///
/// Returns the id that should gate whatever comes next in the CALLER's own
/// sibling list (the last-processed node's id, `Skipped` included — a
/// discarded `skip` node's id is still returned, so whatever follows it
/// still gates on it even though it has no `OutlineItem` of its own;
/// `NodeSkipped` satisfies that gate immediately, so this costs no delay).
fn materialize_outline_tree(
    nodes: &[ConfirmedNode],
    parent_id: Option<&str>,
    incoming_gate: Option<String>,
    items: &mut Vec<OutlineItem>,
    to_skip: &mut Vec<String>,
) -> Option<String> {
    let mut gate = incoming_gate;
    for node in nodes {
        gate = Some(materialize_outline_node(
            node, parent_id, gate, items, to_skip,
        ));
    }
    gate
}

fn materialize_outline_node(
    node: &ConfirmedNode,
    parent_id: Option<&str>,
    incoming_gate: Option<String>,
    items: &mut Vec<OutlineItem>,
    to_skip: &mut Vec<String>,
) -> String {
    match node.action {
        PrereqAction::Skip => {
            to_skip.push(node.id.clone());
            queue_skip_subtree(&node.children, to_skip);
        }
        PrereqAction::Review => {
            items.push(OutlineItem {
                id: node.id.clone(),
                title: node.title.clone(),
                prerequisites: incoming_gate.into_iter().collect(),
                parent_id: parent_id.map(String::from),
                mode: NodeMode::Review,
            });
        }
        PrereqAction::Learn => {
            let child_exit = materialize_outline_tree(
                &node.children,
                Some(node.id.as_str()),
                incoming_gate.clone(),
                items,
                to_skip,
            );
            items.push(OutlineItem {
                id: node.id.clone(),
                title: node.title.clone(),
                prerequisites: child_exit.or(incoming_gate).into_iter().collect(),
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
/// gerado" from the co-design. Only queues `NodeSkipped` events; no
/// `OutlineItem` is ever created for any of them (see `materialize_outline_node`'s
/// `Skip` arm).
fn queue_skip_subtree(nodes: &[ConfirmedNode], to_skip: &mut Vec<String>) {
    for node in nodes {
        to_skip.push(node.id.clone());
        queue_skip_subtree(&node.children, to_skip);
    }
}

/// Mints ids and auto-confirms every node `Learn` (§S15/§S16) — the
/// fallback `create_document` uses for a caller that skipped the
/// confirmation screen entirely (e.g. a direct API call), so a freshly
/// proposed tree still has something to feed `materialize_outline_tree`.
fn auto_confirm_learn(nodes: &[engine::ProposedOutlineNode]) -> Vec<ConfirmedNode> {
    nodes
        .iter()
        .map(|n| ConfirmedNode {
            id: engine::new_id(),
            title: n.title.clone(),
            action: PrereqAction::Learn,
            children: auto_confirm_learn(&n.children),
        })
        .collect()
}

/// One outline item as shown to the client (§S5): the graph's gate, resolved.
/// `state` is `"locked"` (a prerequisite isn't `Demonstrated` yet and the
/// item was never touched), `"available"` (prerequisites met, or already
/// attempted/skipped — i.e. still worth showing as reachable), or
/// `"demonstrated"`. A §S15 prereq-tree `skip` has no state here at all: it
/// is never materialized as an `OutlineItem` (`materialize_prereq_node`),
/// so it never reaches this view in the first place.
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

    // §S15/§S16 (unified 2026-08-19): reuse the exact confirmed outline tree
    // — prerequisites, then the objective's own topic as the last top-level
    // node — already minted and shown by `propose_outline` rather than
    // generating a second, possibly different tree. Only a caller that
    // skipped that screen entirely (e.g. a direct API call) falls back to
    // generating fresh here and auto-confirming every node `learn`.
    //
    // `materialize_outline_tree` chains every top-level node to the one
    // before it (each requires the previous node's id), so the document
    // reads as one continuous sequence: prerequisite 1 -> prerequisite 2 ->
    // ... -> the objective's own (possibly decomposed) topic — see its doc
    // comment for why the old design (prerequisite roots nested under the
    // main line's first item via `parent_id`, gating it in parallel) was
    // the actual bug behind a live report, "Funções Recursivas": prereqs
    // rendered in the sidebar as false decomposition of an unrelated
    // main-line item instead of the sequential background topics they were.
    let confirmed_nodes = if body.nodes.is_empty() {
        let ai = state.ai.load_full();
        let mut tree = engine::propose_outline(&ai, &body.topic, &objective_text).await?;
        // A caller that skips the confirmation screen never saw — let alone
        // approved — the proposed prerequisites, so only auto-confirm the
        // objective's own (last) top-level node and drop the rest, rather
        // than silently committing a learner to a prerequisite chain nobody
        // reviewed.
        let objective_node = tree
            .pop()
            .ok_or_else(|| ApiError::Internal("outline proposal returned no nodes".to_string()))?;
        auto_confirm_learn(std::slice::from_ref(&objective_node))
    } else {
        body.nodes
    };
    let mut items = Vec::new();
    let mut to_skip = Vec::new();
    materialize_outline_tree(&confirmed_nodes, None, None, &mut items, &mut to_skip);
    let outline = engine::Outline {
        topic: body.topic.clone(),
        items,
    };

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
        &serde_json::to_string(&DocumentMeta {
            name: name.clone(),
            built_with: Some(engine::APP_VERSION.to_string()),
        })
        .unwrap_or_default(),
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
    // Read-modify-write, not a fresh `DocumentMeta`: a rename must not erase
    // the `built_with` stamp `create_document` set at creation time.
    let built_with = state
        .store
        .read_doc_file(&doc_id, "document.json")
        .ok()
        .and_then(|json| serde_json::from_str::<DocumentMeta>(&json).ok())
        .and_then(|meta| meta.built_with);
    state.store.write_doc_file(
        &doc_id,
        "document.json",
        &serde_json::to_string(&DocumentMeta {
            name: name.to_string(),
            built_with,
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

    fn learn(id: &str, title: &str, children: Vec<ConfirmedNode>) -> ConfirmedNode {
        ConfirmedNode {
            id: id.to_string(),
            title: title.to_string(),
            action: PrereqAction::Learn,
            children,
        }
    }

    fn leaf(id: &str, title: &str, action: PrereqAction) -> ConfirmedNode {
        ConfirmedNode {
            id: id.to_string(),
            title: title.to_string(),
            action,
            children: Vec::new(),
        }
    }

    /// §S15/§S16: a `learn` node's children are chained SEQUENTIALLY (each
    /// requires the previous sibling), and the node's own `prerequisites`
    /// become its LAST child's id only — the earlier ones are covered
    /// transitively through the chain, not listed as a parallel AND-gate.
    #[test]
    fn learn_recurses_and_chains_children_sequentially() {
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
        let exit = materialize_outline_tree(&tree, None, None, &mut items, &mut to_skip);
        assert_eq!(exit, Some("p1".to_string()));
        assert!(to_skip.is_empty());
        assert_eq!(items.len(), 3);

        let c1 = items.iter().find(|i| i.id == "c1").unwrap();
        assert!(c1.prerequisites.is_empty());
        assert_eq!(c1.parent_id.as_deref(), Some("p1"));
        let c2 = items.iter().find(|i| i.id == "c2").unwrap();
        assert_eq!(c2.prerequisites, vec!["c1".to_string()]);
        assert_eq!(c2.parent_id.as_deref(), Some("p1"));

        let parent = items.iter().find(|i| i.id == "p1").unwrap();
        assert_eq!(parent.prerequisites, vec!["c2".to_string()]);
        assert_eq!(parent.parent_id, None);
    }

    /// §S15/§S16 (unified 2026-08-19): top-level nodes — prerequisite roots
    /// and the objective's own root alike — are SIBLINGS (`parent_id:
    /// None`), chained to each other in confirmed order, never nested under
    /// one another. Regression test for the live bug report 2026-08-19
    /// ("Funções Recursivas"): "C data types"/"C functions" must never
    /// render as subitems of the unrelated main-line item "Recursion in C".
    #[test]
    fn top_level_nodes_chain_as_siblings_not_nested() {
        let tree = vec![
            leaf(
                "data_types",
                "C data types and variables",
                PrereqAction::Learn,
            ),
            learn(
                "c_functions",
                "C functions",
                vec![leaf("fn_def", "Function definition", PrereqAction::Learn)],
            ),
            learn(
                "recursion",
                "Recursion in C",
                vec![leaf("what_is", "What is recursion", PrereqAction::Learn)],
            ),
        ];
        let mut items = Vec::new();
        let mut to_skip = Vec::new();
        materialize_outline_tree(&tree, None, None, &mut items, &mut to_skip);

        for id in ["data_types", "c_functions", "recursion"] {
            let item = items.iter().find(|i| i.id == id).unwrap();
            assert_eq!(item.parent_id, None, "{id} should be top-level");
        }
        let data_types = items.iter().find(|i| i.id == "data_types").unwrap();
        assert!(data_types.prerequisites.is_empty());
        let fn_def = items.iter().find(|i| i.id == "fn_def").unwrap();
        assert_eq!(fn_def.prerequisites, vec!["data_types".to_string()]);
        let c_functions = items.iter().find(|i| i.id == "c_functions").unwrap();
        assert_eq!(c_functions.prerequisites, vec!["fn_def".to_string()]);
        let what_is = items.iter().find(|i| i.id == "what_is").unwrap();
        assert_eq!(what_is.prerequisites, vec!["c_functions".to_string()]);
        let recursion = items.iter().find(|i| i.id == "recursion").unwrap();
        assert_eq!(recursion.prerequisites, vec!["what_is".to_string()]);
    }

    /// §S15: `skip` cascades to the whole branch — the node and every
    /// descendant are DISCARDED (never materialized as an `OutlineItem`, so
    /// the sidebar can't show nor generate them) and only queued for a
    /// `NodeSkipped` event each, regardless of what action they were
    /// individually given. Whatever follows still gates on the discarded id
    /// (`materialize_outline_tree` still returns it), so a node behind this
    /// one can still unlock once the event is appended.
    #[test]
    fn skip_cascades_to_the_whole_branch() {
        let tree = vec![ConfirmedNode {
            id: "p1".to_string(),
            title: "Limits".to_string(),
            action: PrereqAction::Skip,
            children: vec![leaf("c1", "Epsilon-delta", PrereqAction::Learn)],
        }];
        let mut items = Vec::new();
        let mut to_skip = Vec::new();
        let exit = materialize_outline_tree(&tree, None, None, &mut items, &mut to_skip);
        // nothing is materialized — the whole branch is discarded
        assert_eq!(items.len(), 0);
        // but whatever comes next still gates on the (now absent) id
        assert_eq!(exit, Some("p1".to_string()));
        let mut skipped = to_skip.clone();
        skipped.sort();
        assert_eq!(skipped, vec!["c1".to_string(), "p1".to_string()]);
    }

    /// §S15: `review` cascades the opposite way — the branch's descendants
    /// are never materialized at all, only the node itself, in
    /// `NodeMode::Review`.
    #[test]
    fn review_omits_children_entirely() {
        let tree = vec![ConfirmedNode {
            id: "p1".to_string(),
            title: "Algebra basics".to_string(),
            action: PrereqAction::Review,
            children: vec![leaf("c1", "Factoring", PrereqAction::Learn)],
        }];
        let mut items = Vec::new();
        let mut to_skip = Vec::new();
        materialize_outline_tree(&tree, None, None, &mut items, &mut to_skip);
        assert_eq!(items.len(), 1);
        assert!(to_skip.is_empty());
        assert_eq!(items[0].id, "p1");
        assert_eq!(items[0].mode, NodeMode::Review);
        assert!(items[0].prerequisites.is_empty());
    }
}
