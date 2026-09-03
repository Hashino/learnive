use super::reading::read_profile;
use super::*;

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::retrieval::Retriever;
use crate::source::{SearchHit, Source};

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
pub(super) fn document_name(state: &AppState, doc_id: &str, topic: &str) -> String {
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
/// `review` still generates a fresh, local, abbreviated node regardless of
/// `known` (recap, not reference). `skip` on a matched node materializes a
/// real reference instead (§S15b, `materialize_outline_node`'s `Skip` arm):
/// `node_id` is the id of the ALREADY-EXISTING node in `doc_id`, round-
/// tripped back by the client on confirm so the server never has to re-
/// resolve the match a second time.
#[derive(Serialize, Deserialize, Clone)]
pub struct KnownMatch {
    doc_id: String,
    doc_name: String,
    node_id: String,
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
    /// S27e: `book`/`article` for every item the new reading-list
    /// `propose_outline` produces (see `engine::OutlineItemType`) — a future
    /// screen (S27f) can use this to render bibliographic detail; today's
    /// client just ignores fields it doesn't render.
    item_type: OutlineItemType,
    /// The proposed bibliographic identity, present whenever `item_type` is
    /// `book`/`article`.
    #[serde(skip_serializing_if = "Option::is_none")]
    bibliography: Option<crate::source::ProposedItem>,
    /// S27d's existence-check outcome for `bibliography`, once
    /// `propose_outline`'s verify step has run — `None` until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    verification: Option<crate::source::VerificationOutcome>,
    /// The proposed chapter/section number (S27g, revised 2026-08-30) —
    /// present only for a `Chapter`-typed node, shown alongside its `title`
    /// (the proposed name) so the S15 confirmation screen can display both.
    #[serde(skip_serializing_if = "Option::is_none")]
    chapter_number: Option<String>,
}

#[derive(Serialize)]
pub struct ProposeOutlineResp {
    /// One ordered sequence — see `ProposedNode`'s doc comment.
    nodes: Vec<ProposedNode>,
    /// S27e: titles that failed S27d verification even after the one
    /// bounded retry round — PLAN.md §27's "Tratamento da reprovação" is
    /// explicit that a rejection is never silently dropped, so these ride
    /// along on the response for a future screen (S27f) to show, even
    /// though every one of them also still has a corresponding item in
    /// `nodes` (with `verification: not_found`) rather than being removed
    /// from the list outright.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rejected: Vec<String>,
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
    let (tree, rejected) = propose_verified_reading_list(&state, &body.topic, &objective).await?;
    let known = known_concepts(&state)?;
    let nodes = resolve_outline_forest(&state, &tree, &known).await;
    Ok(Json(ProposeOutlineResp { nodes, rejected }))
}

/// Verifies every book/article in a freshly-proposed reading list against
/// real catalogs (S27d), filling in each item's `ProposedOutlineNode::
/// verification` in place. The client is `state.bibliography_client` (never
/// constructed fresh here) — swappable the same way `state.ai`/`state.source`
/// are, so `app::tests::test_state_with_ai` can wire a fast-failing client
/// and no integration test that creates a document ever makes a real
/// network call. The cache IS opened fresh per call (cheap, file-backed, no
/// state to keep in sync — same idiom `acervo`'s own callers use).
async fn verify_reading_list(
    state: &AppState,
    tree: &mut [engine::ProposedOutlineNode],
) -> Result<(), ApiError> {
    let cache = crate::source::BibliographyCache::open(state.data_dir.as_ref())
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    for node in tree.iter_mut() {
        let Some(item) = &node.bibliography else {
            continue;
        };
        node.verification = Some(
            crate::source::verify_bibliography(&state.bibliography_client, &cache, item).await,
        );
    }
    Ok(())
}

/// Titles whose verification came back genuinely `NotFound` — deliberately
/// excludes `Unavailable` (a catalog outage): that outcome degrades, it
/// does not block (`source::bibliography`'s own doc comment), so it must
/// never trigger a re-ask that could silently rewrite a good list just
/// because a catalog was briefly unreachable.
fn not_found_titles(tree: &[engine::ProposedOutlineNode]) -> Vec<String> {
    tree.iter()
        .filter(|n| {
            matches!(
                n.verification,
                Some(crate::source::VerificationOutcome::NotFound)
            )
        })
        .map(|n| n.title.clone())
        .collect()
}

/// Proposes the reading list and verifies it (S27d), with ONE bounded
/// retry round when anything comes back genuinely `NotFound` — same
/// one-repair-round convention as `movement::generate_move`'s schema
/// retry. The retry names the rejected titles (`engine::propose_outline`'s
/// `rejected` parameter) so the model proposes real substitutes instead of
/// looping on the same unverifiable work (PLAN.md §27, "Tratamento da
/// reprovação"). Whatever is STILL `NotFound` after the retry is never
/// dropped — it stays in the returned tree (with its `verification` set,
/// so nothing downstream mistakes it for confirmed) and its title is also
/// returned separately, for a future screen to show the learner it wasn't
/// confirmed, per PLAN.md's explicit "nunca descartada silenciosamente".
async fn propose_verified_reading_list(
    state: &AppState,
    topic: &str,
    objective: &str,
) -> Result<(Vec<engine::ProposedOutlineNode>, Vec<String>), ApiError> {
    let ai = state.ai.load_full();
    let mut tree = engine::propose_outline(&ai, topic, objective, &[]).await?;
    verify_reading_list(state, &mut tree).await?;
    let rejected = not_found_titles(&tree);
    if rejected.is_empty() {
        return Ok((tree, Vec::new()));
    }
    let mut retry_tree = engine::propose_outline(&ai, topic, objective, &rejected).await?;
    verify_reading_list(state, &mut retry_tree).await?;
    let still_rejected = not_found_titles(&retry_tree);
    Ok((retry_tree, still_rejected))
}

/// One `Demonstrated` concept already in some document — the candidate pool
/// `resolve_prereq_forest` matches proposed prerequisites against.
struct KnownConcept {
    doc_id: String,
    doc_name: String,
    node_id: String,
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
                    node_id: item.id.clone(),
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
                node_id: k.node_id.clone(),
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
        item_type: node.item_type,
        bibliography: node.bibliography.clone(),
        verification: node.verification.clone(),
        chapter_number: node.chapter_number.clone(),
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
    /// Echoed back verbatim from `ProposedNode::known` (§S15b) — only
    /// consulted when `action == Skip`: that combination materializes a
    /// REFERENCE to the already-existing node instead of discarding, using
    /// `known.node_id` as the reference item's own id (see
    /// `materialize_outline_node`). Ignored for `Learn`/`Review`, which
    /// still generate fresh local content regardless of a match.
    #[serde(default)]
    known: Option<KnownMatch>,
    #[serde(default)]
    children: Vec<ConfirmedNode>,
    /// S27e: echoed back verbatim from `ProposedNode::item_type` — the
    /// server never re-derives it, same convention as `id`/`title`.
    #[serde(default)]
    item_type: OutlineItemType,
    /// Echoed back verbatim from `ProposedNode::bibliography`.
    #[serde(default)]
    bibliography: Option<crate::source::ProposedItem>,
    /// Echoed back verbatim from `ProposedNode::verification`.
    #[serde(default)]
    verification: Option<crate::source::VerificationOutcome>,
    /// Echoed back verbatim from `ProposedNode::chapter_number` (S27g,
    /// revised 2026-08-30) — set only for a `Chapter`-typed node.
    #[serde(default)]
    chapter_number: Option<String>,
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
        // §S15b: a `skip` on a node WITH a known match materializes a
        // REFERENCE to the already-existing node instead of discarding it —
        // `source_doc_id` points at the owner, and the item's own `id` is
        // the owner's real node id (not the freshly-minted proposal id),
        // since a reference must resolve to the exact same node file
        // `owner_of`/`read_node` would find. Children are never
        // materialized here (own step, §S15b step 5: they follow the owner
        // via `parent_id`, not this tree). A plain `skip` (no match) keeps
        // today's behavior: discarded, whole branch queued for
        // `NodeSkipped`.
        PrereqAction::Skip if node.known.is_some() => {
            let known = node.known.as_ref().expect("checked by guard");
            items.push(OutlineItem {
                id: known.node_id.clone(),
                title: node.title.clone(),
                prerequisites: incoming_gate.into_iter().collect(),
                parent_id: parent_id.map(String::from),
                mode: NodeMode::Learn,
                source_doc_id: Some(known.doc_id.clone()),
                item_type: node.item_type,
                // The referenced item's real expansion state lives on the
                // OWNER document's own `OutlineItem` (not resolved here — a
                // reference only points at it, `owner_of`); left at the
                // default rather than guessed.
                expansion: ExpansionState::NotExpanded,
                source: source_pointer_from(node),
                chapter_number: node.chapter_number.clone(),
                resolved_page: None,
            });
            return known.node_id.clone();
        }
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
                source_doc_id: None,
                item_type: node.item_type,
                expansion: ExpansionState::NotExpanded,
                source: source_pointer_from(node),
                chapter_number: node.chapter_number.clone(),
                resolved_page: None,
            });
        }
        PrereqAction::Learn => {
            // S27g (2026-08-29): a `Book`/`Article` item whose `children`
            // already carry `propose_outline`'s `{number, name}` `Chapter`
            // proposals is `ChaptersProposed`, not `NotExpanded` — those
            // proposals exist and are about to be materialized right below,
            // they just haven't been matched against this book's real,
            // confirmed table of contents yet. That matching pass
            // (`source::match_chapter`, run from
            // `api::reading::ensure_document_grounded`) runs later, once a
            // confirmed TOC exists — not here, and not blocking this
            // materialization. Every other case (no children, or a
            // plain `Node`/`Chapter` item) is unaffected.
            let expansion = if matches!(
                node.item_type,
                OutlineItemType::Book | OutlineItemType::Article
            ) && !node.children.is_empty()
            {
                ExpansionState::ChaptersProposed
            } else {
                ExpansionState::NotExpanded
            };
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
                source_doc_id: None,
                item_type: node.item_type,
                expansion,
                source: source_pointer_from(node),
                chapter_number: node.chapter_number.clone(),
                resolved_page: None,
            });
        }
    }
    node.id.clone()
}

/// Builds an `OutlineItem::source` pointer from a `ConfirmedNode`'s
/// echoed-back bibliography/verification (S27e) — `None` when the node
/// carries no bibliography (a plain `Node`-typed item, e.g. one confirmed
/// via `auto_confirm_learn`'s direct-API fallback, which never populates
/// `verification`).
fn source_pointer_from(node: &ConfirmedNode) -> Option<SourcePointer> {
    node.bibliography.clone().map(|item| SourcePointer {
        item,
        verification: node.verification.clone(),
    })
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
            known: None,
            children: auto_confirm_learn(&n.children),
            item_type: n.item_type,
            bibliography: n.bibliography.clone(),
            verification: n.verification.clone(),
            chapter_number: n.chapter_number.clone(),
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
    /// True for a `Chapter` item `source::match_chapter` could not place
    /// anywhere in its book's confirmed table of contents (S27g's matching
    /// pass already ran — the parent's `expansion` is `Expanded`, not still
    /// `ChaptersProposed` — and still left `resolved_page: None`).
    /// Omitted (never `true`) for every other item, and for a chapter
    /// still waiting on that pass — a book with no confirmed TOC yet is
    /// "not resolved yet", not "failed to resolve", and showing the same
    /// remediation UI for both would train the learner to ignore it.
    /// Drives the client's terminal remediation card: pick the page by
    /// hand, skip the whole book, or restart cold start.
    #[serde(default)]
    pub(super) chapter_match_failed: bool,
    /// `book`/`chapter`/`article`/`node` (S27e's storage-level field,
    /// surfaced here 2026-09-01): the client's lazy neighbor-loading needs
    /// to know a row has NO node file behind it — a container is a reading
    /// boundary, not an unreached neighbor, so probing it only ever
    /// produced a guaranteed 404 (found live: every boot fetched the book
    /// row and logged a 404 for it).
    pub(super) item_type: OutlineItemType,
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
/// §S15b step 5 (read side): a reference's question-spawned sub-nodes —
/// and any FURTHER sub-nodes spawned under those, recursively — live only
/// in the OWNER's `outline.json`, keyed by `parent_id` chains rooted at the
/// referenced node (`ask_question`'s `Spawn` arm already writes them
/// there). Without this, `outline_view` shows a reference as a childless
/// leaf even though reading through it recursively splices spliced
/// sub-nodes (`node.js`'s `hydrateInteractions`) — the sidebar tree and the
/// document itself would disagree. Best-effort like `referencing_documents`:
/// an unreadable owner outline just yields no children, not an error.
fn owner_subtree_items(state: &AppState, owner_doc_id: &str, root_id: &str) -> Vec<OutlineItem> {
    let Ok(owner_outline_json) = state.store.read_doc_file(owner_doc_id, "outline.json") else {
        return Vec::new();
    };
    let Ok(owner_outline) = serde_json::from_str::<Outline>(&owner_outline_json) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut frontier = vec![root_id.to_string()];
    while let Some(parent) = frontier.pop() {
        for item in owner_outline
            .items
            .iter()
            .filter(|i| i.parent_id.as_deref() == Some(parent.as_str()))
        {
            frontier.push(item.id.clone());
            result.push(item.clone());
        }
    }
    result
}

pub(super) fn outline_view(
    state: &AppState,
    doc_id: &str,
    outline: &Outline,
) -> Result<Vec<OutlineItemView>, ApiError> {
    // §S15b step 3: folds this document's own log AND every referenced
    // owner's log — see `folded_node_states`'s doc comment. Covers states
    // for the owner-subtree items merged in below too: it folds the
    // OWNER'S WHOLE log, not just the referenced id's own events.
    let states = super::reading::folded_node_states(state, doc_id, outline)?;

    let mut items: Vec<OutlineItem> = outline.items.clone();
    let mut seen: std::collections::HashSet<String> = items.iter().map(|i| i.id.clone()).collect();
    for item in &outline.items {
        if let Some(owner) = &item.source_doc_id {
            // Two references into the same owner document, one an ancestor
            // of the other, would otherwise emit the shared descendants
            // twice — dedup by id, first occurrence wins.
            for extra in owner_subtree_items(state, owner, &item.id) {
                if seen.insert(extra.id.clone()) {
                    items.push(extra);
                }
            }
        }
    }

    Ok(items
        .iter()
        // §S15: every item is shown now, tree-nested by `parent_id` client-
        // side — a sub-node (question-spawned §S8, or a decomposed
        // prerequisite §S15) is navigation/depth, not hidden. "Next
        // available" advance and `resume_node_id` still need main-line-only
        // (`parent_id.is_none()`); they filter explicitly at their own call
        // sites now that this view doesn't pre-filter for them.
        .map(|item| {
            // General rule (user's stated wording, 2026-09-01): an item
            // with children reads "demonstrated" once all of them are
            // done/skipped, no type-based exception — `engine::effective_state`
            // handles that recursively for any item type, not just the
            // non-generable `Book`/`Chapter` containers this used to be
            // scoped to (see that function's doc comment). Below that:
            // `"available"` once the item has its own direct progress OR
            // any started descendant (`engine::subtree_started` — bug
            // reported live 2026-09-01: a container used to read "locked"
            // the whole time it was in progress, wrong on its own terms —
            // the learner had already opened it — and, via CSS's opacity
            // cascade on the container's `<li>`, faded its own
            // already-unlocked children too); otherwise gated on its own
            // prerequisites, same as any leaf node (a §S15 `Skipped`
            // prerequisite satisfies the gate too, and `effective_state`
            // — not a plain `states.get` — resolves a prerequisite that is
            // itself a container/parent through its own children).
            let view_state = match engine::effective_state(outline, &states, &item.id) {
                Some(NodeState::Demonstrated) => "demonstrated",
                Some(NodeState::Attempted) | Some(NodeState::Skipped) => "available",
                _ if engine::subtree_started(outline, &states, &item.id) => "available",
                _ => {
                    let unlocked = item.prerequisites.iter().all(|p| {
                        matches!(
                            engine::effective_state(outline, &states, p),
                            Some(NodeState::Demonstrated) | Some(NodeState::Skipped)
                        )
                    });
                    if unlocked { "available" } else { "locked" }
                }
            };
            // Promoted to `engine::chapter_match_failed` (bug reported live
            // 2026-09-01) — `prepare` now shares this exact predicate
            // instead of only this view computing it. See that function's
            // doc comment for the drift bug this closes.
            let chapter_match_failed = engine::chapter_match_failed(&items, item);
            OutlineItemView {
                id: item.id.clone(),
                title: item.title.clone(),
                state: view_state,
                parent_id: item.parent_id.clone(),
                mode: match item.mode {
                    NodeMode::Review => Some("review"),
                    NodeMode::Learn => None,
                },
                chapter_match_failed,
                item_type: item.item_type,
            }
        })
        .collect())
}

#[derive(Serialize)]
pub struct CreateResp {
    doc_id: String,
    name: String,
    items: Vec<OutlineItemView>,
    /// S27e: only ever non-empty for the direct-API fallback (`body.nodes`
    /// empty) — a caller that went through `/api/outline/propose` first
    /// already saw any rejection there (`ProposeOutlineResp::rejected`)
    /// before confirming, so this is empty in that path. Never a reason to
    /// fail the request: PLAN.md §27's "nunca descartada silenciosamente"
    /// means visible, not blocking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rejected: Vec<String>,
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
    let (confirmed_nodes, rejected) = if body.nodes.is_empty() {
        let (tree, rejected) =
            propose_verified_reading_list(&state, &body.topic, &objective_text).await?;
        // S27e: a caller that skips the confirmation screen never saw — let
        // alone reviewed — the proposed reading list, but there is no
        // longer a separate "unreviewed prerequisites" category to drop
        // (PLAN.md §27 decision 3: the list's own order IS the prerequisite
        // chain) — auto-confirming just the last item, the way the old
        // concept-tree fallback dropped every prerequisite and kept only
        // the objective's own node, would silently discard every
        // foundational work instead. Auto-confirm the whole list, in order.
        (auto_confirm_learn(&tree), rejected)
    } else {
        (body.nodes, Vec::new())
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
        rejected,
    }))
}

#[derive(Deserialize)]
pub struct NextTopicReq {
    topic: String,
    /// Same convention as `CreateReq::objective_text` — empty only for a
    /// caller that skipped confirmation.
    #[serde(default)]
    objective_text: String,
    /// Same convention as `CreateReq::nodes` — the confirmed prerequisite
    /// tree from `propose_outline` (reused as-is: it's stateless and
    /// already scans every document's `Demonstrated` items, this one
    /// included, for cross-epoch matches). Empty falls back to proposing
    /// fresh and auto-confirming just the topic's own root.
    #[serde(default)]
    nodes: Vec<ConfirmedNode>,
}

#[derive(Serialize)]
pub struct NextTopicResp {
    items: Vec<OutlineItemView>,
    /// S27e — see `CreateResp::rejected`'s doc comment; same rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rejected: Vec<String>,
}

/// "What are we learning next?" (§S15c, decided in PLAN.md's `TODO
/// futuros` — the UI screen itself is transitory, never persisted; this
/// endpoint is the one *persisted* consequence of confirming it). Reuses
/// `propose_objective`/`propose_outline` unchanged (both are already
/// document-agnostic) and `create_document`'s own materialization
/// (`materialize_outline_tree`) — the only real difference from cold start
/// is that this APPENDS to an existing document's outline/objective chain
/// instead of creating a new one, so no prior work is discarded or
/// rewritten (§5).
pub async fn next_topic(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
    Json(body): Json<NextTopicReq>,
) -> Result<Json<NextTopicResp>, ApiError> {
    if body.topic.trim().is_empty() {
        return Err(ApiError::BadRequest("empty topic".to_string()));
    }
    let objective_text = if body.objective_text.trim().is_empty() {
        body.topic.clone()
    } else {
        body.objective_text.clone()
    };

    // Surfaces a 404 for an unknown document before anything else runs.
    let outline_json = state.store.read_doc_file(&doc_id, "outline.json")?;
    let existing: Outline =
        serde_json::from_str(&outline_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let (confirmed_nodes, rejected) = if body.nodes.is_empty() {
        let (tree, rejected) =
            propose_verified_reading_list(&state, &body.topic, &objective_text).await?;
        // S27e: same reasoning as `create_document`'s identical fallback —
        // auto-confirm the whole reading list, in order, not just the last
        // item (see that function's comment for why).
        (auto_confirm_learn(&tree), rejected)
    } else {
        (body.nodes, Vec::new())
    };

    // Continues the main line's single sequential chain (materialize_
    // outline_tree's own doc comment) rather than starting a second,
    // ungated one: gate the new chunk's first node on the CURRENT last
    // main-line item. Trivially satisfied — reaching this endpoint means
    // every main-line item already is `Demonstrated` — but keeps the whole
    // document reading as one continuous sequence.
    let incoming_gate = existing
        .items
        .iter()
        .rev()
        .find(|i| i.parent_id.is_none())
        .map(|i| i.id.clone());

    let mut new_items = Vec::new();
    let mut to_skip = Vec::new();
    materialize_outline_tree(
        &confirmed_nodes,
        None,
        incoming_gate,
        &mut new_items,
        &mut to_skip,
    );

    state.store.update_outline_file(&doc_id, |json| {
        let mut outline: Outline = serde_json::from_str(json).map_err(|e| e.to_string())?;
        outline.items.extend(new_items.clone());
        serde_json::to_string(&outline).map_err(|e| e.to_string())
    })?;

    let log_json = state.store.read_doc_file(&doc_id, "objective.json")?;
    let mut log: ObjectiveLog =
        serde_json::from_str(&log_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    log.push(objective_text.clone(), ObjectiveSource::NextTopic);
    state.store.write_doc_file(
        &doc_id,
        "objective.json",
        &serde_json::to_string(&log).unwrap_or_default(),
    )?;

    if !to_skip.is_empty() {
        let event_log = state.store.event_log(&doc_id)?;
        for id in &to_skip {
            if let Err(e) = event_log.append(Some(id), EventKind::NodeSkipped) {
                eprintln!("event log append failed: {e}");
            }
        }
    }

    // Same background grounding as `create_document` — the new epoch's
    // objective is the best available seed text.
    spawn_acquisition(state.clone(), objective_text);

    let outline_json = state.store.read_doc_file(&doc_id, "outline.json")?;
    let outline: Outline =
        serde_json::from_str(&outline_json).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let items = outline_view(&state, &doc_id, &outline)?;
    Ok(Json(NextTopicResp { items, rejected }))
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

    // §S15b step 6: a reference (`source_doc_id`) only stays resolvable
    // because `Node::doc_id` is never rewritten — the owner's files ARE the
    // node. Deleting the owner out from under a live reference would leave
    // it pointing at nothing, silently, the next time anyone reads it.
    // Refusing is the correct behavior at the cost this slice is willing to
    // pay; promoting ownership to a referent is the expensive version and
    // isn't needed yet.
    let referencing = referencing_documents(&state, &doc_id)?;
    if !referencing.is_empty() {
        return Err(ApiError::Conflict(format!(
            "cannot delete: still referenced by {}",
            referencing.join(", ")
        )));
    }

    state.store.delete_document(&doc_id)?;
    Ok(Json(serde_json::json!({ "deleted": doc_id })))
}

/// Documents (other than `doc_id` itself) whose outline references a node
/// owned by `doc_id` — §S15b step 6's delete guard. Best-effort: a sibling
/// document with an unreadable/unparsable `outline.json` is skipped rather
/// than blocking the delete on an unrelated corruption.
fn referencing_documents(state: &AppState, doc_id: &str) -> Result<Vec<String>, ApiError> {
    let mut names = Vec::new();
    for other_id in state.store.list_documents()? {
        if other_id == doc_id {
            continue;
        }
        let Ok(outline_json) = state.store.read_doc_file(&other_id, "outline.json") else {
            continue;
        };
        let Ok(outline) = serde_json::from_str::<Outline>(&outline_json) else {
            continue;
        };
        let references = outline
            .items
            .iter()
            .any(|i| i.source_doc_id.as_deref() == Some(doc_id));
        if references {
            names.push(document_name(state, &other_id, ""));
        }
    }
    Ok(names)
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

/// The document's resume target: the newest main-line item whose subtree has
/// an actual node file on disk, walking main-line items newest first (§S15 —
/// main-line only, so a resume can't land on an unrelated sub-node), then
/// descending each item's own subtree via [`engine::resume_leaf`].
///
/// A generated-but-now-**locked** leaf is not a resume target (bug reported
/// live 2026-09-03): its prerequisites no longer hold, so `prepare` refuses
/// (or redirects into a child the client then mounts above the reader's
/// actual position) — the worst possible "welcome back". Locked leaves are
/// skipped and the walk keeps going backwards; if every main-line leaf is
/// locked, `None` sends the client to its own fallback (the first
/// `available` item across the whole tree), which is where a document whose
/// frontier moved into a prerequisite tree actually lives.
fn resume_target(
    items: &[OutlineItemView],
    outline: &Outline,
    generated: &std::collections::HashSet<String>,
) -> Option<String> {
    items
        .iter()
        .filter(|i| i.parent_id.is_none())
        .rev()
        .find_map(|i| {
            engine::resume_leaf(outline, generated, &i.id).filter(|leaf| {
                items
                    .iter()
                    .any(|it| &it.id == leaf && it.state != "locked")
            })
        })
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
            // Bugfix (2026-08-30, advisor-flagged after S27g's container
            // gate landed): a non-generable container (`engine::is_generable`
            // — `Book`/`Article`, and now `Chapter` once item 2's split
            // gives it children too) never gets a node file of its own —
            // nothing ever generates it, its children do — so a plain
            // main-line `generated.contains(id)` lookup was silently `None`
            // for every topic-scoped document forever, and "resume where
            // you left off" never fired. Walk main-line items newest first;
            // for a container that itself was never generated, descend into
            // its actual children via `engine::resume_leaf` — depth-general
            // (not hardcoded to one level) so a split `Chapter`'s own `Node`
            // children resume correctly too, still scoped to that one
            // item's own subtree, so this can't land on an unrelated
            // sub-node the way removing the `parent_id.is_none()` filter
            // entirely would.
            // A generated-but-now-LOCKED leaf is not a resume target (bug
            // reported live 2026-09-03): its prerequisites no longer hold,
            // so `prepare` refuses (or redirects into a child the client
            // then mounts above the reader's actual position) — the worst
            // possible "welcome back". See `resume_target`.
            resume_node_id: resume_target(&items, &outline, &generated),
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
/// real book/article title (`engine::propose_source_title` — renamed
/// 2026-08-29 from `propose_search_subject`; searching by *subject* against
/// an all-fields index is what let a discrete-math node acquire an unrelated
/// Android/automata paper, the wrong-book bug S27m closed at the grounding
/// gate but not at the source), then tries each configured `Source` in order
/// (`state.source` then `state.fallback_source`) with that title, searched
/// against LibGen's title column specifically (`source::libgen`). Falls back
/// to the raw hint verbatim if the title search yields nothing — deliberately:
/// a hallucinated or unmatched title should mean *no book*, not a resurrected
/// subject-phrase guess, so this fallback is expected to usually also miss.
/// An acquisition that lands nothing here is not silently absorbed later: it
/// leaves `research_attempted` set and grounding empty, which is what makes
/// S27m's document-level gate refuse the node rather than generate ungrounded
/// prose. Returns as soon as one attempt lands. The primary (LibGen) and
/// fallback (Sci-Hub) backends are both tried; Sci-Hub still only accepts a
/// DOI query and is unaffected by this title change (open question, not yet
/// decided: resolving title→DOI via `source::bibliography`'s Crossref lookup
/// so Sci-Hub can be reached from a title too — see PLAN.md's S27m note).
/// When neither mirror answers (offline / blocked / no result) this degrades
/// to not-grounded — same as the no-retriever case below. Acquisition is a
/// best-effort enhancement, so every failure mode here is recoverable and
/// never surfaced as an error to the caller.
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
    // Both acquisition slots empty (the 2026-09-01 unplug) → there is nothing
    // to search, and `propose_source_title` below is a paid model call that
    // would only name queries for backends that don't exist. The `research`
    // move — this function's other consumer — then reports "no adequate
    // source found" instantly instead of after a mirror timeout (§12.2: never
    // spend a call to learn what the type already knows).
    if matches!(*state.source, Source::Unconfigured)
        && matches!(*state.fallback_source, Source::Unconfigured)
    {
        return AcquisitionOutcome {
            grounded: false,
            source_title: None,
        };
    }
    let ai = state.ai.load_full();
    let title = engine::propose_source_title(&ai, query_hint)
        .await
        .unwrap_or_default();

    let mut queries = Vec::with_capacity(2);
    if !title.trim().is_empty() {
        queries.push(title.as_str());
    }
    if !query_hint.trim().is_empty() && query_hint != title {
        queries.push(query_hint);
    }

    // Try every (backend, query), and within a query EVERY ranked hit — the
    // best textbook first, then the next, … — until one actually downloads.
    // A single mirror download can reset mid-stream (LibGen is flaky on larger
    // PDFs), so falling through to the next candidate is what lets grounding
    // land a real textbook instead of silently giving up on the first reset.
    // `ranked_hits` already puts textbooks ahead of journal articles.
    for source in [&state.source, &state.fallback_source] {
        for query in &queries {
            let Ok(hits) = source.search(query).await else {
                continue;
            };
            for hit in crate::source::ranked_hits(&hits) {
                if let Some(title) = fetch_and_store(state, source, retriever, &hit).await {
                    return AcquisitionOutcome {
                        grounded: true,
                        source_title: Some(title),
                    };
                }
            }
        }
    }
    AcquisitionOutcome {
        grounded: false,
        source_title: None,
    }
}

/// Downloads, stores, and reindexes a chosen hit. `None` on any failure —
/// every failure mode is recoverable by the caller trying another
/// hit/query/backend.
async fn fetch_and_store(
    state: &AppState,
    source: &Arc<Source>,
    retriever: &Arc<RwLock<Retriever>>,
    hit: &SearchHit,
) -> Option<String> {
    let corpus = &state.corpus;
    let doc = match source.fetch(hit).await {
        Ok(d) => d,
        // Download/normalize failure (e.g. mirror reset mid-stream) — log it
        // so a flaky acquisition is visible, then let the caller try the next
        // ranked candidate instead of failing silently.
        Err(e) => {
            eprintln!("acquisition: fetch failed for \"{}\": {e}", hit.title);
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

/// The runtime acquisition backend (§11.1). Origin is deliberately open
/// (`source::mod` doc comment) — the earlier OpenStax/Wikipedia backends were
/// deleted, and although the façade still ships LibGen and Sci-Hub backends,
/// **both slots are UNPLUGGED as of 2026-09-01 (user decision)**:
/// `build_source`/`build_fallback_source` return `Source::Unconfigured`
/// unless the user points them at a mirror via `LEARNIVE_LIBGEN_URL` /
/// `LEARNIVE_SCIHUB_URL` — the env vars are the only plug. The built-in
/// default mirror lists that used to make the slot always-on are out of the
/// build path (retained below, unreferenced): live QA that day showed the
/// always-on default burning a cold start on 7 mirror rejections (HTTP
/// 500/503) before one download landed, and grounding on real documents
/// comes from the local library (route B) anyway — remote
/// search-and-download must be an explicit act, never a background default.
/// `Source::Unconfigured`'s calls fail fast with `SourceError::Unconfigured`,
/// and `acquire` short-circuits before even proposing a search title.
// Retained for re-plugging (2026-09-01): candidate roots tried in order
// (whichever answers first wins). They rotate constantly, so REFRESH these
// lists when plugging back in — these are what was live at unplug time.
// Re-plug by restoring the `unwrap_or_else(DEFAULT_…)` fallbacks in
// `build_source`/`build_fallback_source`, or just set the env vars.
// `sci-hub.ee` is a reliably-ungated mirror (no Cloudflare interstitial) that
// works where `.se`/`.st`/`.wf` are challenged. libgen.im/libgen.li are tried
// first because they answer from more networks; libgen.is/rs/st are the older
// generation that still works where reachable.
#[allow(dead_code)]
const DEFAULT_LIBGEN_URLS: &str =
    "https://libgen.li,https://libgen.im,https://libgen.is,https://libgen.rs,https://libgen.st";
#[allow(dead_code)]
const DEFAULT_SCIHUB_URLS: &str = "https://sci-hub.ee,https://sci-hub.se,https://sci-hub.st,https://sci-hub.wf,https://sci-hub.ren";

/// The §11.1 primary acquisition backend: LibGen (books) — currently
/// UNPLUGGED (see the module doc above): `Source::Unconfigured` unless
/// `LEARNIVE_LIBGEN_URL` is set. The fallback (`build_fallback_source`,
/// Sci-Hub for papers) is tried when this yields nothing.
pub fn build_source() -> Source {
    match std::env::var("LEARNIVE_LIBGEN_URL") {
        Ok(urls) if !urls.trim().is_empty() => {
            Source::LibGen(crate::source::LibGenSource::new(urls))
        }
        _ => Source::Unconfigured,
    }
}

/// The §11.1 fallback tier: Sci-Hub (papers) — currently UNPLUGGED, same
/// terms as `build_source` above.
pub fn build_fallback_source() -> Source {
    match std::env::var("LEARNIVE_SCIHUB_URL") {
        Ok(urls) if !urls.trim().is_empty() => {
            Source::SciHub(crate::source::SciHubSource::new(urls))
        }
        _ => Source::Unconfigured,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learn(id: &str, title: &str, children: Vec<ConfirmedNode>) -> ConfirmedNode {
        ConfirmedNode {
            id: id.to_string(),
            title: title.to_string(),
            action: PrereqAction::Learn,
            known: None,
            children,
            item_type: OutlineItemType::Node,
            bibliography: None,
            verification: None,
            chapter_number: None,
        }
    }

    fn leaf(id: &str, title: &str, action: PrereqAction) -> ConfirmedNode {
        ConfirmedNode {
            id: id.to_string(),
            title: title.to_string(),
            action,
            known: None,
            children: Vec::new(),
            item_type: OutlineItemType::Node,
            bibliography: None,
            verification: None,
            chapter_number: None,
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
            known: None,
            children: vec![leaf("c1", "Epsilon-delta", PrereqAction::Learn)],
            item_type: OutlineItemType::Node,
            bibliography: None,
            verification: None,
            chapter_number: None,
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

    /// §S15b: `skip` on a node WITH a known match materializes a REFERENCE
    /// instead of discarding — the acceptance-criterion mechanism. The
    /// item's own id must be the OWNER's real node id (`known.node_id`),
    /// not the freshly-minted proposal id, since a reference has to resolve
    /// to the exact node file `owner_of`/`read_node` would find; children
    /// are never materialized locally (they follow the owner, §S15b step
    /// 5, a separate mechanism).
    #[test]
    fn skip_on_a_known_match_materializes_a_reference_not_a_discard() {
        let tree = vec![ConfirmedNode {
            id: "proposal-id".to_string(),
            title: "Algebra basics".to_string(),
            action: PrereqAction::Skip,
            known: Some(KnownMatch {
                doc_id: "other-doc".to_string(),
                doc_name: "Other document".to_string(),
                node_id: "real-node-id".to_string(),
            }),
            children: vec![leaf("c1", "Factoring", PrereqAction::Learn)],
            item_type: OutlineItemType::Book,
            bibliography: None,
            verification: None,
            chapter_number: None,
        }];
        let mut items = Vec::new();
        let mut to_skip = Vec::new();
        let exit = materialize_outline_tree(&tree, None, None, &mut items, &mut to_skip);

        assert_eq!(items.len(), 1, "a reference IS materialized, not discarded");
        assert!(to_skip.is_empty(), "a reference is not a NodeSkipped event");
        assert_eq!(items[0].id, "real-node-id");
        assert_eq!(items[0].source_doc_id.as_deref(), Some("other-doc"));
        // S27e: item_type is echoed through from the confirmed node, not
        // dropped by the reference-materialization path.
        assert_eq!(items[0].item_type, OutlineItemType::Book);
        // whatever follows gates on the real node id, not the proposal id
        assert_eq!(exit, Some("real-node-id".to_string()));
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
            known: None,
            children: vec![leaf("c1", "Factoring", PrereqAction::Learn)],
            item_type: OutlineItemType::Node,
            bibliography: None,
            verification: None,
            chapter_number: None,
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

    /// S27e: a reading-list-shaped confirmed tree (flat, `Book`-typed items,
    /// no `children` — exactly what `resolve_outline_forest` produces from
    /// `engine::propose_outline`'s new contract) materializes ONLY the
    /// reading-list items themselves, sequentially chained, each carrying
    /// its bibliographic `source` pointer and `expansion: NotExpanded` —
    /// nothing below a book is invented or eagerly materialized, since
    /// there is nothing in `children` to recurse into in the first place.
    /// This is `materialize_outline_tree`'s "stops materializing everything
    /// up front" behavior (PLAN.md §27): it was never special-cased logic
    /// to remove, it falls out of the reading list never having nested
    /// children to begin with.
    #[test]
    fn materialize_outline_tree_stops_at_the_reading_list_itself() {
        fn book(id: &str, title: &str) -> ConfirmedNode {
            ConfirmedNode {
                id: id.to_string(),
                title: title.to_string(),
                action: PrereqAction::Learn,
                known: None,
                children: Vec::new(),
                item_type: OutlineItemType::Book,
                bibliography: Some(crate::source::ProposedItem {
                    title: title.to_string(),
                    authors: vec!["Author, Some".to_string()],
                    year: Some(2020),
                    edition: None,
                    identifier: None,
                    kind: crate::source::SourceKind::Book,
                }),
                verification: Some(crate::source::VerificationOutcome::Verified {
                    catalog: crate::source::Catalog::OpenLibrary,
                    matched_title: title.to_string(),
                }),
                chapter_number: None,
            }
        }
        let tree = vec![book("b1", "Pré-Cálculo"), book("b2", "Cálculo, Volume 1")];
        let mut items = Vec::new();
        let mut to_skip = Vec::new();
        materialize_outline_tree(&tree, None, None, &mut items, &mut to_skip);

        assert_eq!(
            items.len(),
            2,
            "only the two reading-list items, nothing invented below them"
        );
        assert!(to_skip.is_empty());
        // Sequentially chained — the second item's own prerequisite is the
        // first item's id, same "list order IS the prerequisite chain" rule
        // as any other main-line sequence.
        assert!(items[0].prerequisites.is_empty());
        assert_eq!(items[1].prerequisites, vec!["b1".to_string()]);
        for item in &items {
            assert_eq!(item.item_type, OutlineItemType::Book);
            assert_eq!(item.expansion, ExpansionState::NotExpanded);
            let source = item
                .source
                .as_ref()
                .expect("book item carries a source pointer");
            assert!(matches!(
                source.verification,
                Some(crate::source::VerificationOutcome::Verified { .. })
            ));
        }
    }

    /// S27g (2026-08-29): a `Book` item whose `children` carry topic-scoped
    /// `Chapter` proposals materializes as `ChaptersProposed` (matching
    /// against the real book hasn't run yet), and the chapters themselves
    /// materialize as real `OutlineItem`s — chained sequentially to each
    /// other (the "prerequisite path within the work", PLAN.md), parented
    /// to the book via `parent_id`, and with no `source` of their own (they
    /// inherit the book's via `resolve_grounding_source`). The book's own
    /// gate becomes its LAST chapter's id, same "learn recurses, parent
    /// gates on last child" rule any other decomposed node already follows.
    #[test]
    fn book_with_chapter_children_materializes_as_chapters_proposed() {
        fn chapter(id: &str, number: Option<&str>, title: &str) -> ConfirmedNode {
            ConfirmedNode {
                id: id.to_string(),
                title: title.to_string(),
                action: PrereqAction::Learn,
                known: None,
                children: Vec::new(),
                item_type: OutlineItemType::Chapter,
                bibliography: None,
                verification: None,
                chapter_number: number.map(String::from),
            }
        }
        let book = ConfirmedNode {
            id: "b1".to_string(),
            title: "The C Programming Language".to_string(),
            action: PrereqAction::Learn,
            known: None,
            children: vec![
                chapter("c1", Some("4"), "functions in C"),
                chapter("c2", Some("4.10"), "recursion in C"),
            ],
            item_type: OutlineItemType::Book,
            bibliography: Some(crate::source::ProposedItem {
                title: "The C Programming Language".to_string(),
                authors: vec!["Kernighan, Brian W.".to_string()],
                year: Some(1988),
                edition: Some("2nd".to_string()),
                identifier: None,
                kind: crate::source::SourceKind::Book,
            }),
            verification: Some(crate::source::VerificationOutcome::Verified {
                catalog: crate::source::Catalog::OpenLibrary,
                matched_title: "The C Programming Language".to_string(),
            }),
            chapter_number: None,
        };
        let mut items = Vec::new();
        let mut to_skip = Vec::new();
        materialize_outline_tree(&[book], None, None, &mut items, &mut to_skip);

        assert_eq!(items.len(), 3, "the book plus its two chapters");
        let book_item = items.iter().find(|i| i.id == "b1").unwrap();
        assert_eq!(book_item.expansion, ExpansionState::ChaptersProposed);
        assert_eq!(book_item.prerequisites, vec!["c2".to_string()]);
        assert!(book_item.source.is_some());

        let c1 = items.iter().find(|i| i.id == "c1").unwrap();
        let c2 = items.iter().find(|i| i.id == "c2").unwrap();
        assert_eq!(c1.item_type, OutlineItemType::Chapter);
        assert_eq!(c1.parent_id, Some("b1".to_string()));
        assert!(c1.prerequisites.is_empty());
        // S27g (revised 2026-08-30): the proposed chapter/section number is
        // carried through materialization untouched, matching-independent.
        assert_eq!(c1.chapter_number.as_deref(), Some("4"));
        assert_eq!(c2.chapter_number.as_deref(), Some("4.10"));
        assert_eq!(c1.resolved_page, None, "no matching pass has run yet");
        assert!(
            c1.source.is_none(),
            "a chapter inherits the book's source, it doesn't carry its own"
        );
        assert_eq!(c2.parent_id, Some("b1".to_string()));
        assert_eq!(
            c2.prerequisites,
            vec!["c1".to_string()],
            "chapters chain sequentially, same as any other decomposition"
        );
    }

    /// Isolates the exact distinction S27e's bounded retry depends on: a
    /// genuine `NotFound` is a rejection (retry-worthy), a catalog outage
    /// (`Unavailable`) is a degradation, never a rejection — retrying on
    /// `Unavailable` would silently rewrite a good list on a network hiccup
    /// (bibliography.rs's own doc comment, PLAN.md §27).
    #[test]
    fn not_found_titles_excludes_catalog_unavailable() {
        fn node_with(
            title: &str,
            verification: crate::source::VerificationOutcome,
        ) -> engine::ProposedOutlineNode {
            engine::ProposedOutlineNode {
                title: title.to_string(),
                children: Vec::new(),
                item_type: OutlineItemType::Book,
                chapter_number: None,
                bibliography: None,
                verification: Some(verification),
            }
        }
        let tree = vec![
            node_with(
                "Really Not Found",
                crate::source::VerificationOutcome::NotFound,
            ),
            node_with(
                "Catalog Was Down",
                crate::source::VerificationOutcome::Unavailable {
                    errors: vec!["timeout".to_string()],
                },
            ),
            node_with(
                "Confirmed",
                crate::source::VerificationOutcome::Verified {
                    catalog: crate::source::Catalog::OpenLibrary,
                    matched_title: "Confirmed".to_string(),
                },
            ),
        ];
        assert_eq!(
            not_found_titles(&tree),
            vec!["Really Not Found".to_string()]
        );
    }

    /// S27e regression: the direct-API fallback (`create_document`/
    /// `next_topic` with `body.nodes` empty) used to keep only the last
    /// top-level node — correct for the old concept-tree contract, where
    /// that was "the objective's own topic" and everything before it was an
    /// unreviewed prerequisite safe to drop. Under the reading list there is
    /// no such category (PLAN.md §27 decision 3): dropping every item but
    /// the last would silently discard every foundational work. The whole
    /// list must be auto-confirmed, in order.
    #[test]
    fn auto_confirm_learn_keeps_the_whole_reading_list_in_order() {
        let tree = vec![
            engine::ProposedOutlineNode {
                title: "Foundational Work".to_string(),
                children: Vec::new(),
                item_type: OutlineItemType::Book,
                chapter_number: None,
                bibliography: None,
                verification: None,
            },
            engine::ProposedOutlineNode {
                title: "Objective Work".to_string(),
                children: Vec::new(),
                item_type: OutlineItemType::Book,
                chapter_number: None,
                bibliography: None,
                verification: None,
            },
        ];
        let confirmed = auto_confirm_learn(&tree);
        assert_eq!(confirmed.len(), 2, "both items kept, not just the last");
        assert_eq!(confirmed[0].title, "Foundational Work");
        assert_eq!(confirmed[1].title, "Objective Work");
        assert!(confirmed.iter().all(|n| n.action == PrereqAction::Learn));
    }

    // -- resume_target (S32, bug reported live 2026-09-03) ------------------

    fn view(id: &str, parent: Option<&str>, state: &'static str) -> OutlineItemView {
        OutlineItemView {
            id: id.to_string(),
            title: id.to_string(),
            state,
            parent_id: parent.map(str::to_string),
            mode: None,
            chapter_match_failed: false,
            item_type: OutlineItemType::Node,
        }
    }

    fn empty_outline() -> Outline {
        Outline {
            topic: "t".to_string(),
            items: Vec::new(),
        }
    }

    /// A generated-but-now-locked main-line leaf must not become the resume
    /// target: its prerequisites no longer hold, `prepare` refuses it, and
    /// the client's page-load auto-generate then mounts a doomed "generating"
    /// section above the reader's actual position (reported live: a node
    /// generating above an already generated node).
    #[test]
    fn resume_skips_a_generated_but_locked_main_line_leaf() {
        let items = vec![view("n02", None, "locked")];
        let generated: std::collections::HashSet<String> = ["n02".to_string()].into();
        assert_eq!(resume_target(&items, &empty_outline(), &generated), None);
    }

    /// The walk keeps going backwards past a locked leaf: an older main-line
    /// leaf that is actually reachable still resumes.
    #[test]
    fn resume_walks_back_past_a_locked_leaf_to_an_unlocked_one() {
        let items = vec![view("n01", None, "available"), view("n02", None, "locked")];
        let generated: std::collections::HashSet<String> =
            ["n01".to_string(), "n02".to_string()].into();
        assert_eq!(
            resume_target(&items, &empty_outline(), &generated),
            Some("n01".to_string())
        );
    }

    /// A reachable (unlocked) generated leaf resumes as before — the filter
    /// must not regress the normal "continue where you left off" case.
    #[test]
    fn resume_still_picks_an_unlocked_generated_leaf() {
        let items = vec![view("n01", None, "available")];
        let generated: std::collections::HashSet<String> = ["n01".to_string()].into();
        assert_eq!(
            resume_target(&items, &empty_outline(), &generated),
            Some("n01".to_string())
        );
    }
}
