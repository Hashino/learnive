//! Curriculum engine (§6) and assessment (§8): turns a topic into an outline,
//! generates nodes on demand, and grades answers against a rubric **locked at
//! creation** (§8).
//!
//! Split of responsibility with §14: the prose (robust, streamed) is generated
//! by the token-by-token endpoint; here live the pure/testable parts (prompts,
//! parsing, assembly) and the non-streamed orchestrations (outline, exercise +
//! rubric, grading). The rubric is generated in a **separate** call from the
//! prose and kept server-only (§8) — the student never sees it.
//!
use rand::{Rng, distributions::Alphanumeric};
use serde::{Deserialize, Serialize};

use learnive_core::{Node, ObjectiveType, ensure_block_ids, render_math};

use crate::ai::{Ai, ChatMessage, ProviderError, Tier};

/// Short git commit hash of the running build (embedded at compile time by
/// `build.rs`, never a runtime `git` shell-out — the binary must know its own
/// build commit even run from elsewhere, or with no `.git` present at all, in
/// which case this falls back to `"unknown"`). QA/debugging traceability
/// only: stamped onto every generated node (`wrap_article`) and document
/// (`api::cold_start::create_document`), never shown to the user or exposed
/// via any API response. Deliberately a single string, not a struct — meant
/// to be extensible to a richer version scheme later without callers caring
/// what shape that takes.
pub const APP_VERSION: &str = env!("LEARNIVE_BUILD_SHA");

/// Per-objective grade (§8): not pass/fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grade {
    NotDemonstrated,
    Partial,
    Demonstrated,
}

/// An outline item (§6): a concept that becomes a node.
///
/// `id` is the node's stable identity (§S5) — assigned once, at generation
/// (or, for an item minted by an approved `plan` proposal, at approval) and
/// never reassigned even if the item's array position later changes. Node
/// files, the rubric sidecar, and event-log `node_id`s are all keyed on this,
/// not on array index — a `plan`-approved reorder must not silently make
/// `n0`'s file serve as a different concept's content.
///
/// `prerequisites` are the graph edges (§S5, "Grafo com arestas"): ids of
/// items that must be `Demonstrated` (§8/`events::aggregate::NodeState`)
/// before this one is available. `api::cold_start::materialize_outline_tree`
/// and `decide_plan_proposal` both today only ever chain items in sequence
/// (each item's sole prerequisite the previous item's id), which degenerates
/// to the old rigid one-at-a-time gate, per PLAN.md's S5 note. Multiple
/// prerequisites (a real diamond) are a data shape this already supports,
/// even though nothing generates one yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub prerequisites: Vec<String>,
    /// The tree-shaped pointer for the sidebar (§S15, extending §S8): set for
    /// a sub-node spawned from a mid-reading question OR any node decomposed
    /// by `propose_outline` (prerequisite or the objective's own topic
    /// alike — see `ProposedOutlineNode`'s doc comment), either way the id of
    /// the item it's subordinate to. `None` for every top-level item: every
    /// prerequisite root and the objective's own root, chained to each other
    /// in sequence by `api::cold_start::materialize_outline_tree`. This field
    /// decides SHAPE only (sidebar tree nesting, inline
    /// splice point) — whether an item gates anything is `prerequisites`'
    /// job alone: a question-spawned child carries none of its own (never
    /// gates its parent), while a prerequisite-tree child's id is placed in
    /// its parent's `prerequisites` at materialization time (the parent only
    /// becomes available once every child is `Demonstrated`). "Next
    /// available" advance still walks the main line only (`parent_id.is_
    /// none()`) — every call site that needs that must filter explicitly
    /// now, since `api::outline_view` no longer does (S15's sidebar shows
    /// the whole tree).
    #[serde(default)]
    pub parent_id: Option<String>,
    /// How this node's content gets generated (§S15 learn/review/skip
    /// toggle): `Learn` is full generation as always; `Review` is a short
    /// definition-only pass plus a couple of exercises, chosen by the
    /// learner for a prerequisite they believe they already know. Either way
    /// the gate is the same `Demonstrated` grade any node needs — `Review`
    /// only shrinks the volume of exposure, never the evidence bar.
    ///
    /// A `skip`ped item needs no mode of its own and is never materialized
    /// as an `OutlineItem` at all — `api::cold_start::materialize_prereq_node`
    /// discards it and its whole subtree, recording only a `NodeSkipped`
    /// event per id (§S5) so a parent gated on it can still unlock. This is
    /// deliberately stricter than "tag it and hide it in the UI": an id that
    /// never enters `outline.items` can never be looked up by
    /// `api::reading::prepare` or offered by any "next available" search in
    /// the first place, so there is no state to get confused with the
    /// unrelated mid-document "skip this node, revisit later" gesture
    /// (`NodeState::Skipped`, which deliberately stays `"available"` for the
    /// revisit scheduler). An earlier version of this used `mode: Learn` +
    /// only the event to keep a skip node from generating, which the event's
    /// shared "available" semantics defeated in practice (confirmed live,
    /// 2026-08-18: a "recursão em C" document generated content for several
    /// nodes the learner had explicitly marked skip).
    #[serde(default)]
    pub mode: NodeMode,
    /// The owning document's id, when this item is a **reference** to a node
    /// that actually lives in another document (§S15b) — `None` for every
    /// local node (still the overwhelming majority: every item that isn't a
    /// materialized `KnownMatch` skip). Resolve with [`owner_of`], never by
    /// reading this field directly, since the local case needs the caller's
    /// own `doc_id` as the fallback. `Node.doc_id` on disk is always the
    /// owner and is never rewritten — this pointer is the only new state.
    #[serde(default)]
    pub source_doc_id: Option<String>,
    /// What this item actually is (S27e, PLAN.md §27): only `Node` is
    /// directly generable — see [`OutlineItemType`]. `#[serde(default)]`
    /// reads every pre-S27e `outline.json` (no field on disk at all) as
    /// `Node`, which is exactly what every one of those items always was.
    #[serde(default)]
    pub item_type: OutlineItemType,
    /// Whether a `Book`/`Chapter` item's children have been discovered yet
    /// (S27e data shape; the discovery itself is S27g, not built here). See
    /// [`ExpansionState`]. Meaningless for a `Node`, left at the default.
    #[serde(default)]
    pub expansion: ExpansionState,
    /// The bibliographic identity + S27d verification outcome behind a
    /// `Book`/`Article` item — `None` for a `Node`/`Chapter` (a chapter
    /// inherits its parent book's identity, S27g). See [`SourcePointer`].
    /// Deliberately NOT wired into S21's grounding retrieval here — that's
    /// `grounding_for`'s job, a later slice.
    #[serde(default)]
    pub source: Option<SourcePointer>,
    /// The proposed chapter/section number for a `Chapter` item (S27g,
    /// revised 2026-08-30) — carried verbatim from `ProposedOutlineNode::
    /// chapter_number` through confirmation, `None` for a `Node`/`Book`/
    /// `Article`. Stays on the item even after matching runs (unlike
    /// `resolved_page`, nothing overwrites it) — it's what a re-match after
    /// a library change re-resolves against.
    #[serde(default)]
    pub chapter_number: Option<String>,
    /// The real physical page `source::match_chapter` placed this `Chapter`
    /// on, once the book→chapter matching pass (S27g) has run against its
    /// confirmed table of contents (S27k) — `None` until that pass runs, or
    /// forever if nothing in the real book matched this chapter's proposed
    /// number/name (degrades to whole-work-style generation, never blocks
    /// it). Citation deep-links degrade to `#page=N` off this field once
    /// S27j's PDF viewer route exists; nothing consumes it yet.
    #[serde(default)]
    pub resolved_page: Option<usize>,
}

/// What an outline item actually is (S27e, PLAN.md §27): only `Node` is
/// directly generable — `Book`/`Chapter`/`Article` are reading-list
/// containers materialized from the confirmed bibliography, whose children
/// (chapters, then concept nodes) are discovered by S27g's contextual
/// expansion.
///
/// `Chapter` **is** minted at cold start now (S27g, revised 2026-08-29 —
/// PLAN.md has the full account, including an argument the assistant made
/// and the user rejected, kept there because the wrong argument is
/// plausible and will reappear, and a second same-day revision on top of
/// that: the model no longer proposes a bare subject string, it proposes a
/// structured `{number, name}` pair). `engine::prompt::propose_outline`'s
/// `chapters` field lets the model name within-work subjects the objective
/// actually needs — each carrying an optional hierarchical `number` (e.g.
/// `"4.10"`) alongside the `name` — and `parse::outline_tree` turns each
/// into a `Chapter` child. What's still true from the reasoning this
/// comment used to make — a chapter can only be VERIFIED against a real PDF
/// table of contents, not a bibliographic catalog — is why `Chapter` still
/// isn't confirmed structure at proposal time: the model's `number`/`name`
/// pair is an assertion, not a match. Resolving it onto the real book's
/// confirmed table of contents is `source::match_chapter`
/// (`toc_confirm.rs`), run from `api::reading::ensure_document_grounded`'s
/// chapter-matching pass — no longer unbuilt (see `ChaptersProposed`
/// below). Don't read this doc comment as still forbidding chapter
/// proposal — that prohibition was rewritten specifically because it
/// conflated unverifiable STRUCTURE (still forbidden at proposal time) with
/// buildable SCOPE judgment (now allowed, and now resolvable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutlineItemType {
    Book,
    Chapter,
    Article,
    /// A directly generable concept node — every item before S27e, and
    /// every item this slice still mints itself (a §S8 spawned sub-node,
    /// `api::generation`'s `plan`-move items). Default so a pre-S27e
    /// `outline.json` with no `item_type` field deserializes as what its
    /// items always actually were.
    #[default]
    Node,
}

/// Whether a `Book`/`Chapter` [`OutlineItem`]'s children have been
/// discovered yet (S27e data shape; the discovery itself, "contextual
/// expansion", is S27g).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExpansionState {
    #[default]
    NotExpanded,
    /// S27g (added 2026-08-29): a `Book`/`Article` item whose `children`
    /// already carry `{number, name}` `Chapter` proposals straight out of
    /// `propose_outline`, but nothing has matched them against this book's
    /// real, confirmed table of contents yet. That matching pass now
    /// exists — `api::reading::ensure_document_grounded` calls
    /// `source::match_chapter` (number-first, name-fallback) against the
    /// confirmed TOC (`TocConfirmStore`), persists each child's resolved
    /// page, and advances the item to `Expanded`. Degrades silently (stays
    /// `ChaptersProposed`, children keep `resolved_page: None`) whenever a
    /// piece is missing — no confirmed TOC yet, file moved, hash mismatch —
    /// never blocks generation; a later pass can pick it back up once the
    /// missing piece shows up.
    ChaptersProposed,
    Expanded,
}

/// Which bibliographic entry a `Book`/`Article` [`OutlineItem`] refers to,
/// and what S27d's existence check said about it.
///
/// Deliberately reuses `source::ProposedItem`/`VerificationOutcome` verbatim
/// rather than inventing a parallel shape — this IS the item S27d's
/// `verify_bibliography` ran against, so a re-derived equivalent struct here
/// would just be a second thing to keep in sync with that module's contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePointer {
    pub item: crate::source::ProposedItem,
    /// `None` only transiently, inside `api::cold_start`'s propose/verify
    /// loop, for an item not yet checked — every `SourcePointer` that
    /// actually reaches `Outline::items` carries a real outcome (verified,
    /// not-found-but-kept-visible, or unavailable), never a bare pointer.
    #[serde(default)]
    pub verification: Option<crate::source::VerificationOutcome>,
}

/// Which [`SourcePointer`] grounds a given outline item (S27m, PLAN.md,
/// 2026-08-29) — the item's own, if it's a `Book`/`Article`; otherwise the
/// nearest ancestor's, found by walking `parent_id` (a `Node`/`Chapter` has
/// no source of its own — "a chapter inherits its parent book's identity",
/// generalized here to today's pre-S27g shape where a directly-generable
/// item can itself already be a `Book`/`Article`, or a spawned sub-node a
/// few `parent_id` hops below one). `None` for an item with no
/// bibliographic ancestor anywhere in its chain — the legacy/demo/pre-S27e
/// case, which S27m's grounding gate deliberately leaves untouched (its own
/// scope note: only the bibliographically-sourced path is being fixed).
pub fn resolve_grounding_source(outline: &Outline, item: &OutlineItem) -> Option<SourcePointer> {
    if let Some(ptr) = &item.source {
        return Some(ptr.clone());
    }
    let mut current = item.parent_id.clone();
    while let Some(pid) = current {
        let parent = outline.items.iter().find(|i| i.id == pid)?;
        if let Some(ptr) = &parent.source {
            return Some(ptr.clone());
        }
        current = parent.parent_id.clone();
    }
    None
}

/// Every `Book`/`Article` outline item with a bibliographic source pointer,
/// paired with the outline item id — the reading list's full acquisition
/// checklist. A `Node`/`Chapter` item has no bibliographic identity of its
/// own and is skipped.
///
/// Promoted here from a private copy in `api::acervo` (S27m, 2026-08-29) so
/// the S27f gate-report screen and S27m's document-level generation gate
/// (`api::reading::ensure_document_grounded`) read the reading list's
/// expectations through one function, not two that could silently drift.
/// Lives in `engine.rs` rather than `source::acervo` because it bridges
/// `Outline`/`OutlineItem` (owned here) with `source::ExpectedItem` — moving
/// it into `source` would invert the dependency (`engine` already depends
/// on `source` for `SourcePointer`'s `ProposedItem`, not the other way).
pub fn expected_items(outline: &Outline) -> Vec<(String, crate::source::ExpectedItem)> {
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
                crate::source::ExpectedItem {
                    title: ptr.item.title.clone(),
                    authors: ptr.item.authors.clone(),
                    kind: ptr.item.kind,
                },
            ))
        })
        .collect()
}

/// Whether an outline item can be directly generated as content (S27g,
/// 2026-08-29; generalized to `Chapter` 2026-08-30 for item 2's chapter→node
/// split). A `Node` always is — it's the atomic content unit, never a
/// container. Everything else (`Book`/`Article`/`Chapter`) is generable
/// UNLESS it actually has children in `outline.items` right now: those
/// children carry the real content, and letting the container stay
/// generable on top of them would just relocate the reported bug ("o livro
/// todo tratado como um nodo só") from the book level to the chapter level
/// instead of fixing it — a chapter that got split would otherwise still
/// generate itself as one monolithic node, then ALSO generate its subnodes.
/// A container with no children yet (a `Book`/`Article` before chapter
/// proposal, or a `Chapter` before it's had its first-visit split attempt —
/// item 2 — regardless of whether that attempt found anything) stays exactly
/// as generable as it always was: no regression for a document that never
/// needed the deeper scoping. Checked by real children (`parent_id`), not
/// `item.expansion`, so this can't desync from what actually got
/// materialized — true uniformly at any depth, so a Book→Chapter→Node chain
/// needs no special-casing here (each level asks the same question about
/// itself).
pub fn is_generable(outline: &Outline, item: &OutlineItem) -> bool {
    if item.item_type == OutlineItemType::Node {
        return true;
    }
    !outline
        .items
        .iter()
        .any(|i| i.parent_id.as_deref() == Some(item.id.as_str()))
}

/// True for a `Chapter` item `source::match_chapter` could not place
/// anywhere in its book's confirmed table of contents (S27g's matching
/// pass already ran and still left `resolved_page: None`) — promoted here
/// from `api::cold_start::outline_view` (bug reported live 2026-09-01) so
/// `api::reading::prepare` can share the exact same predicate instead of a
/// second copy silently drifting out of sync with it. That drift was a
/// real bug, not theoretical: `outline_view` computed this for the
/// sidebar's remediation badge, but `prepare`/`ground_node` never checked
/// it — a chapter with `resolved_page: None` still fell through to
/// `ground_node`'s unscoped full-book-search fallback and generated real
/// content, so a learner could open a node that already has real prose and
/// still be offered "restart this document" / "skip this chapter" by the
/// remediation modal. The user's stated invariant is that this must be
/// architecturally impossible — a node is always the correspondent of a
/// chapter or part of one, so it must never be possible to start
/// generating before that chapter's match is settled.
///
/// A chapter's parent reaching `ExpansionState::Expanded` means the
/// matching pass actually RAN (not just that chapters were proposed) — a
/// book still sitting at `ChaptersProposed` hasn't been matched yet at
/// all, and "not run yet" must not read the same as "ran and failed".
///
/// Takes a plain item slice, not `&Outline` — `cold_start::outline_view`
/// evaluates this over a list merged with cross-document reference items
/// (`owner_subtree_items`), not `outline.items` alone, so a parent lookup
/// scoped to `outline.items` would silently miss a referenced chapter's
/// real parent and under-report this predicate. `prepare` (which only
/// ever has its own document's `outline.items`) passes that directly.
pub fn chapter_match_failed(items: &[OutlineItem], item: &OutlineItem) -> bool {
    item.item_type == OutlineItemType::Chapter
        && item.resolved_page.is_none()
        && item
            .parent_id
            .as_deref()
            .and_then(|pid| items.iter().find(|i| i.id == pid))
            .is_some_and(|book| book.expansion == ExpansionState::Expanded)
}

/// A node/container's gate-relevant state, synthesizing a container's from
/// its children when it has none of its own (S27g, 2026-08-29; generalized
/// to `Chapter` 2026-08-30 for item 2). A non-generable container (see
/// [`is_generable`] — `Book`/`Article`/`Chapter` alike once it has real
/// children) never receives a `Demonstrated` event directly — nothing ever
/// generates it — so without this, whatever comes after it in the reading
/// list would stay locked forever the moment its children finish (the same
/// "container never satisfies its own gate" trap a fully-skipped book would
/// also fall into). A container counts as `Demonstrated` once every one of
/// its direct children (by `parent_id`) is itself `Demonstrated` or
/// `Skipped`, recursively — which is what makes a `Book → Chapter → Node`
/// chain work with no extra case: a mid-chain `Chapter` that split (item 2)
/// recurses one level deeper into its own `Node` children before reporting
/// up to the `Book`. A container with no children at all (shouldn't happen
/// once [`is_generable`] agrees it isn't one, but checked defensively, and
/// the ordinary case for a `Chapter` that never split) reports no state
/// rather than vacuously "done" — falling through to `states.get` above on
/// the next call up the chain would be wrong (a leaf `Chapter` genuinely has
/// no state of its own until it either generates directly or splits).
pub fn effective_state(
    outline: &Outline,
    states: &std::collections::HashMap<String, crate::events::aggregate::NodeState>,
    item_id: &str,
) -> Option<crate::events::aggregate::NodeState> {
    use crate::events::aggregate::NodeState;
    if let Some(s) = states.get(item_id) {
        return Some(*s);
    }
    let item = outline.items.iter().find(|i| i.id == item_id)?;
    if item.item_type == OutlineItemType::Node {
        return None;
    }
    let children: Vec<&OutlineItem> = outline
        .items
        .iter()
        .filter(|i| i.parent_id.as_deref() == Some(item_id))
        .collect();
    if children.is_empty() {
        return None;
    }
    let all_satisfied = children.iter().all(|c| {
        matches!(
            effective_state(outline, states, &c.id),
            Some(NodeState::Demonstrated) | Some(NodeState::Skipped)
        )
    });
    all_satisfied.then_some(NodeState::Demonstrated)
}

/// Most-recently-generated **leaf** reachable from an outline item, walking
/// `parent_id` children as deep as the tree actually goes (S27g item 2,
/// 2026-08-30) — generalizes `api::cold_start::list_documents`'s old
/// one-level-only `Book → Chapter` resume fallback (added when a
/// non-generable `Book`/`Article` container turned out to never get a node
/// file of its own, so a plain `generated.contains(id)` lookup on it was
/// always `None`) to any depth, so a `Chapter` that item 2 has since split
/// into `Node`s resumes into the right one instead of stopping one level
/// too early. Children are tried in reverse outline order (`.rev()`) — this
/// resolves ties to the *last child in outline order with any generated
/// descendant*, not necessarily the most recently generated one; it's the
/// same tie-break the one-level fallback this replaces already used, so it's
/// not a behavior change. Returns the item itself once it's directly
/// generated, regardless of depth — the base case that makes the recursion
/// correct for a plain leaf `Node` too, not just a container.
pub fn resume_leaf(
    outline: &Outline,
    generated: &std::collections::HashSet<String>,
    item_id: &str,
) -> Option<String> {
    if generated.contains(item_id) {
        return Some(item_id.to_string());
    }
    outline
        .items
        .iter()
        .filter(|i| i.parent_id.as_deref() == Some(item_id))
        .rev()
        .find_map(|c| resume_leaf(outline, generated, &c.id))
}

/// Resolves which document actually owns a node's file/event-log (§S15b) —
/// the item's own document unless it's a reference (`source_doc_id: Some`),
/// in which case the pointed-to document. Every call site that turns
/// `(doc_id, node_id)` into a node read/write/event-log operation must
/// resolve through this first; `write_node` itself stays untouched; it
/// already writes to `node.doc_id`, which is the owner by construction.
pub fn owner_of(outline: &Outline, doc_id: &str, node_id: &str) -> String {
    outline
        .items
        .iter()
        .find(|i| i.id == node_id)
        .and_then(|i| i.source_doc_id.clone())
        .unwrap_or_else(|| doc_id.to_string())
}

/// See [`OutlineItem::mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NodeMode {
    #[default]
    Learn,
    Review,
}

/// Skeleton of the living document (§6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outline {
    pub topic: String,
    pub items: Vec<OutlineItem>,
}

/// A compact, editable curriculum objective proposed from the raw cold-start
/// topic (§6.1/§S4) — not yet persisted; the client shows it for confirm/edit
/// before `create_document` locks it as version 1 (`objective::ObjectiveLog`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectiveProposal {
    pub text: String,
    /// Short display name for the living document (§S12) — what the sidebar
    /// shows and what the learner renames. Ridden along on this call rather
    /// than given one of its own: the cold start already pays for a fast-tier
    /// round trip here, and a second one just to title the document would be
    /// latency (§14) and tokens (§12.2) for a label. Empty when the model
    /// omits it; the caller falls back to the topic.
    #[serde(default)]
    pub title: String,
}

/// A rubric objective, locked at node creation (§8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RubricObjective {
    pub id: String,
    pub kind: ObjectiveType,
    pub description: String,
    /// Objective grading criterion (what counts as demonstrated).
    pub criteria: String,
    /// Transfer item (§8): requires applying to a scenario not covered in the text.
    #[serde(default)]
    pub transfer: bool,
}

/// Full rubric — server-only, never served to the client (§8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rubric {
    pub objectives: Vec<RubricObjective>,
}

/// An objective's grade after grading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectiveGrade {
    pub objective_id: String,
    pub grade: Grade,
    pub feedback: String,
}

/// Result of grading an answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assessment {
    pub grades: Vec<ObjectiveGrade>,
}

impl Assessment {
    /// Advancing requires every objective demonstrated (§8).
    pub fn all_demonstrated(&self) -> bool {
        !self.grades.is_empty() && self.grades.iter().all(|g| g.grade == Grade::Demonstrated)
    }

    /// Objectives not yet demonstrated — they trigger remediation (§8.2).
    pub fn unmet(&self) -> Vec<&ObjectiveGrade> {
        self.grades
            .iter()
            .filter(|g| g.grade != Grade::Demonstrated)
            .collect()
    }
}

/// Engine errors.
#[derive(Debug)]
pub enum EngineError {
    Provider(ProviderError),
    Parse(String),
    /// `decide_move` (movement.rs, S2) was asked for a next move but the
    /// node's moves are already complete — completion is decided by grading
    /// (`Assessment::all_demonstrated`), not by `decide_move`, so this means
    /// the caller asked out of turn.
    NoNextMove,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Provider(e) => write!(f, "provider: {e}"),
            EngineError::Parse(m) => write!(f, "model response could not be read: {m}"),
            EngineError::NoNextMove => {
                write!(f, "no next move: this node's moves are already complete")
            }
        }
    }
}

impl std::error::Error for EngineError {}

impl From<ProviderError> for EngineError {
    fn from(e: ProviderError) -> Self {
        EngineError::Provider(e)
    }
}

/// Short random ID (lowercase alphanumeric), safe as a filename/ID.
pub fn new_id() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(10)
        .map(char::from)
        .collect::<String>()
        .to_lowercase()
}

// ---------------------------------------------------------------------------
// Orchestrations (they use the provider).
// ---------------------------------------------------------------------------

/// Proposes a compact, editable curriculum objective from the raw topic
/// (§6.1 cold start, §S4) — a fast call, stateless (nothing is persisted
/// here; `api.rs::create_document` locks the confirmed/edited version).
pub async fn propose_objective(ai: &Ai, topic: &str) -> Result<ObjectiveProposal, EngineError> {
    let text = collect(ai, Tier::Fast, prompt::propose_objective(topic)).await?;
    parse::objective_proposal(&text)
}

/// Derives a real, specific book/article TITLE for source acquisition (§11)
/// from the raw topic — see `prompt::search_subject` for the exact
/// instruction. Renamed from the old `propose_search_subject` 2026-08-29: it
/// used to ask the model for a 2-4-word subject phrase ("calculus"), which is
/// what an all-fields catalog search against LibGen was matching on and how
/// a discrete-math node could acquire an unrelated Android/automata paper —
/// title-column search (`source::libgen`) needs an actual title, not a
/// subject. Fast tier: a background, non-blocking, low-stakes call.
pub async fn propose_source_title(ai: &Ai, topic: &str) -> Result<String, EngineError> {
    let text = collect(ai, Tier::Fast, prompt::search_subject(topic)).await?;
    Ok(text.trim().trim_matches('"').to_string())
}

/// Reads a PDF's printed contents/sumário page (S27k, PLAN.md, 2026-08-29) —
/// the deduction step between embedded bookmarks and the heading heuristic
/// in §11.1's TOC cascade. `pages` is `source::toc::contents_page_chunks`'s
/// output, never the book's body. Fast tier (see `prompt::propose_toc`'s
/// doc for why). The caller (the acervo gate) falls through to the heading
/// heuristic on either an empty list or an `Err`, exactly as it already
/// does when there's no contents page to try in the first place.
///
/// **One call per printed page, results concatenated in reading order.**
/// The pages of a contents run are independent lists, and sending them
/// together is what made this fail on every book (see
/// `source::toc::contents_page_chunks`). A page that fails is skipped, not
/// fatal: a partial TOC still resolves the chapters it did read, and losing
/// page 3 of 7 is a far better outcome than losing the book — which is the
/// whole reason the acervo gate has a cascade instead of one attempt.
pub async fn propose_toc(
    ai: &Ai,
    pages: &[String],
) -> Result<Vec<crate::source::toc::TocLlmEntry>, EngineError> {
    let mut all = Vec::new();
    let mut last_err = None;
    for page in pages {
        if page.trim().is_empty() {
            continue;
        }
        let messages = prompt::propose_toc(page);
        match ai
            .complete_within(Tier::Fast, messages, Some(TOC_PAGE_MAX_TOKENS))
            .await
        {
            Ok(text) => match parse::toc_entries(&text) {
                Some(entries) => all.extend(entries),
                None => last_err = Some(EngineError::Parse("no JSON".to_string())),
            },
            Err(e) => last_err = Some(e.into()),
        }
    }
    match last_err {
        // Every page failed — report the last real cause rather than an
        // empty success the caller would mistake for "this book has no TOC".
        Some(e) if all.is_empty() => Err(e),
        _ => Ok(all),
    }
}

/// Ceiling for one contents page's transcription.
///
/// Sized against the **free tier's tokens-per-minute window**, not against
/// the model's context. Measured 2026-08-30: `max_tokens` counts toward
/// that window as *requested* tokens, so an 8000-token budget against
/// Groq's 8000 TPM free limit made the request itself illegal — `413
/// rate_limit_exceeded, Requested 8813` before a single token was
/// generated. SPEC §15 makes free tiers the target, so the budget has to
/// leave room for the prompt inside the same window. 4000 covers the
/// densest page in the test library (~60 entries ≈ 1800 output tokens) with
/// the remainder as headroom for a reasoning model's thinking, which is what
/// actually overruns here.
const TOC_PAGE_MAX_TOKENS: u32 = 4000;

/// Proposes an ordered split of a chapter into atomic sub-topics (S27g item
/// 2, PLAN.md — user's words: "each chapter is represented as a node in the
/// outline but when the generation gets to that chapter the agent tries to
/// split the chapter into subnodes with atomic knowledge"). `signal_text` is
/// structural signal about the chapter's own content — heading-shaped lines
/// from `source::acervo::heuristic_toc_over`, scoped to the chapter's page
/// range, when there are any; a short cross-page prose sample otherwise (see
/// `api::reading`'s caller for which one it built) — **never** the chapter's
/// full text, which would blow the same free-tier TPM budget
/// `TOC_PAGE_MAX_TOKENS`'s doc measured. Truncated defensively here too, in
/// case a future caller forgets to bound it itself.
///
/// Returns an empty `Vec`, not an error, whenever the attempt doesn't
/// produce a real split — an unparseable response, or the model correctly
/// reporting a single cohesive topic. "Tries" is literal (PLAN.md): a
/// chapter that doesn't split stays one node, which this function's
/// contract treats as a normal outcome, not a failure. Only a genuine
/// provider-level failure (network, `429`) surfaces as `Err`; either way the
/// caller must not retry within the same visit (§14/§15: no speculative or
/// retried spend), and must still mark the chapter `Expanded`.
pub async fn propose_chapter_split(
    ai: &Ai,
    chapter_title: &str,
    signal_text: &str,
) -> Result<Vec<String>, EngineError> {
    let capped: String = signal_text
        .chars()
        .take(CHAPTER_SPLIT_INPUT_CHAR_CAP)
        .collect();
    if capped.trim().is_empty() {
        return Ok(Vec::new());
    }
    let messages = prompt::propose_chapter_split(chapter_title, &capped);
    let text = ai
        .complete_within(Tier::Fast, messages, Some(CHAPTER_SPLIT_MAX_TOKENS))
        .await?;
    Ok(parse::chapter_split(&text).unwrap_or_default())
}

/// Input cap for [`propose_chapter_split`]'s `signal_text`, in characters.
/// Heading-only signal is normally far under this; it exists mainly to
/// bound the prose-sample fallback against the free-tier TPM ceiling
/// measured for [`TOC_PAGE_MAX_TOKENS`].
const CHAPTER_SPLIT_INPUT_CHAR_CAP: usize = 6000;

/// Response budget for [`propose_chapter_split`] — a handful of short
/// titles, nowhere near [`TOC_PAGE_MAX_TOKENS`]'s ceiling for a dense
/// contents page.
const CHAPTER_SPLIT_MAX_TOKENS: u32 = 1200;

/// One item of the reading list an objective needs (S27e, PLAN.md §27,
/// replacing the pre-pivot concept-decomposition tree this type used to
/// describe alone — see git history / PLAN.md's S27e entry for that old
/// shape's contract if archaeology is ever needed).
///
/// The array this appears in is always read as ONE ordered sequence of real
/// bibliographic works — books and articles, never invented concept
/// titles — with foundational/prerequisite works first and the work(s) most
/// directly covering the objective last. Order alone carries the
/// prerequisite relationship now (PLAN.md §27 decision 3, "pré-requisito de
/// conceito não sobrevive como categoria própria"): there is no longer a
/// separate "prerequisite of concept" category. `children` used to always
/// come back empty from `propose_outline` — as of S27g (2026-08-29) it
/// carries `Chapter`-typed, `{number, name}` proposals whenever the model
/// judges only part of a work is in scope (`parse::outline_tree`'s doc
/// comment) — nothing below THOSE chapters is discovered at cold start yet;
/// matching a proposed chapter onto the real book is built
/// (`source::match_chapter`, run from
/// `api::reading::ensure_document_grounded`), but breaking a matched
/// chapter into concept nodes stays S27g's still-unbuilt contextual-
/// expansion work, against the
/// real PDF. The field was already reused structurally by
/// `api::cold_start`'s materialization before this (the sidebar tree shape),
/// so no change was needed there for `Chapter` children to just work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedOutlineNode {
    pub title: String,
    #[serde(default)]
    pub children: Vec<ProposedOutlineNode>,
    /// S27e: `Book` or `Article` for every top-level item the reading-list
    /// prompt proposes; as of S27g, a `chapters`-derived child of one of
    /// those is `Chapter` (see [`OutlineItemType`]'s doc comment). `Node`
    /// only shows up on the old, still-compiled-but-unused-by-
    /// `propose_outline` concept-tree shape this type also used to serve
    /// alone.
    #[serde(default)]
    pub item_type: OutlineItemType,
    /// The proposed chapter/section NUMBER (S27g, revised 2026-08-30) as the
    /// model recalled it — e.g. `"4"`, `"4.10"`, `"2.2.1"` — `None` for a
    /// `Book`/`Article` (only a `Chapter` child ever carries one) or for a
    /// `Chapter` the model wasn't confident enough about to number. Never
    /// treated as verified structure on its own: `source::match_chapter`
    /// resolves it against the book's real confirmed table of contents,
    /// falling back to `title` (the chapter's name) when this is absent or
    /// matches nothing.
    #[serde(default)]
    pub chapter_number: Option<String>,
    /// The proposed bibliographic identity for a `Book`/`Article` item —
    /// `None` for `Node`/`Chapter`. This alone is not yet a [`SourcePointer`]:
    /// verification hasn't run at the point `parse::outline_tree` produces
    /// this value — see [`Self::verification`].
    #[serde(default)]
    pub bibliography: Option<crate::source::ProposedItem>,
    /// S27d's existence-check outcome for `bibliography` — always `None`
    /// coming out of `parse::outline_tree` (the model never emits this;
    /// there is nothing to verify yet at parse time). `api::cold_start`
    /// fills it in, in place, right after `propose_outline` returns and
    /// before the tree reaches either `resolve_outline_forest` (the
    /// confirm-screen path) or `auto_confirm_learn` (the direct-API
    /// fallback) — both then just copy it into `SourcePointer`/
    /// `ConfirmedNode` rather than re-deriving it, same "resolved once,
    /// carried through" shape `ProposedNode`/`ConfirmedNode` already use for
    /// `known`/`suggested`.
    #[serde(default)]
    pub verification: Option<crate::source::VerificationOutcome>,
}

/// Proposes the initial reading list for an objective (S27e, PLAN.md §27,
/// replacing the pre-pivot concept-outline-by-prerequisite call this
/// function used to make — see git history for that prompt/parse if ever
/// needed). Real bibliographic works only, ordered foundational-first — see
/// [`ProposedOutlineNode`]'s doc comment for the array contract. An empty
/// result is never valid (there is always at least one work covering the
/// objective itself); a parse failure is a hard error — this is the actual
/// reading list about to be shown for confirmation, not something with a
/// safe silent default.
///
/// Verification (S27d's `verify_bibliography`, run per proposed item against
/// real catalogs) deliberately does NOT happen in here: it needs a
/// `BibliographyClient`/`BibliographyCache`, which need a data directory —
/// dragging `source`'s I/O types into `engine`'s signature for this one
/// call's benefit isn't worth it when `api::cold_start` already holds
/// `state` with both. See `api::cold_start::propose_reading_list` for the
/// propose → verify → (bounded) re-propose loop built on top of this.
///
/// `rejected` names items a prior round of this same cold start already
/// tried and had rejected by verification (S27e's bounded retry, one round —
/// same shape as `movement`'s one-repair-round convention) — empty on the
/// first call. Passing titles back lets the model avoid proposing the exact
/// same unverifiable work again instead of looping on it.
///
/// Robust tier, deliberately, same reasoning the old concept-decomposition
/// call used this tier for: real bibliographic judgment (which works are
/// genuinely foundational for this objective, in what order) is exactly the
/// kind of structural judgment call that was confirmed live (2026-08-17) to
/// need it — this is a one-time call per cold start, not a per-block one, so
/// the cost tradeoff favors correctness.
pub async fn propose_outline(
    ai: &Ai,
    topic: &str,
    objective: &str,
    rejected: &[String],
) -> Result<Vec<ProposedOutlineNode>, EngineError> {
    let text = collect(
        ai,
        Tier::Robust,
        prompt::propose_outline(topic, objective, rejected),
    )
    .await?;
    let nodes = parse::outline_tree(&text)
        .ok_or_else(|| EngineError::Parse("could not read outline tree".to_string()))?;
    if nodes.is_empty() {
        return Err(EngineError::Parse(
            "outline tree had no reading-list items".to_string(),
        ));
    }
    Ok(nodes)
}

/// Grades an answer against the locked rubric (§8). Light tier.
///
/// `reference_solution` (S16) is the model's own worked-out answer to
/// `exercise_html`, captured at generation time and never shown to the
/// student — grading against it closes the LLM-as-grader leniency gap
/// (`prompt::grading`'s doc comment has the instruction not to leak it back
/// through feedback). May be empty (a sidecar written before this field
/// existed, or a model that omitted it) — `prompt::grading` degrades to
/// rubric-only grading in that case, same behavior as before this slice.
pub async fn grade(
    ai: &Ai,
    rubric: &Rubric,
    exercise_html: &str,
    answer: &str,
    reference_solution: &str,
    locale: crate::locale::Locale,
) -> Result<Assessment, EngineError> {
    let text = collect(
        ai,
        Tier::Fast,
        prompt::grading(rubric, exercise_html, answer, reference_solution, locale),
    )
    .await?;
    parse::assessment(&text)
}

/// What the tutor decided about a question asked mid-reading (§7/§S8): answer
/// it in place (today's `/ask`, unchanged), or spawn a real sub-node because
/// the question needs more than a paragraph — a self-contained elaboration
/// that becomes part of the document (graph, revisitable), not a side chat.
///
/// Scope of this slice: a spawned sub-node is a single prose-only node (no
/// exercise/rubric, no gate) — a real, versioned, revisitable elaboration,
/// but not itself a checked concept. A question whose answer genuinely
/// requires prerequisites the learner hasn't demonstrated yet (a real
/// sub-graph with its own gated chain) is explicitly deferred — see PLAN.md's
/// S8 entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskDecision {
    Inline,
    Spawn { title: String },
}

/// Decides whether a question gets answered inline or spawns a sub-node
/// (§7/§S8). Fast tier — a cheap classification, not explanatory prose
/// (§12.1) — with one bounded repair attempt on a schema violation, same
/// convention as `movement::decide_move_ai`.
pub async fn decide_ask_response(
    ai: &Ai,
    topic: &str,
    item_title: &str,
    node_context: &str,
    reading_context: Option<&str>,
    question: &str,
) -> Result<AskDecision, EngineError> {
    let messages =
        prompt::decide_ask_response(topic, item_title, node_context, reading_context, question);
    let text = collect(ai, Tier::Fast, messages.clone()).await?;
    if let Ok(d) = parse::ask_decision(&text) {
        return Ok(d);
    }
    let repair = repair_messages(
        messages,
        &text,
        "expected JSON {\"spawn\":bool,\"title\":\"...\"}",
    );
    let text = collect(ai, Tier::Fast, repair).await?;
    parse::ask_decision(&text)
}

/// Repair round for a bare `collect` call outside `movement.rs`'s own
/// `repair_messages` (kept private there) — same one-bounded-retry
/// convention (§14).
fn repair_messages(
    mut messages: Vec<ChatMessage>,
    bad_output: &str,
    error: &str,
) -> Vec<ChatMessage> {
    messages.push(ChatMessage::assistant(bad_output.to_string()));
    messages.push(ChatMessage::user(format!(
        "That response did not parse: {error}. Respond again with ONLY the \
         corrected JSON, nothing else."
    )));
    messages
}

/// Wraps an already-processed content section (blocks already carrying
/// `data-block-id`, math already rendered to MathML, an exercise form
/// already tagged or absent) into the full node article — no processing of
/// its own. Shared by [`assemble_node`]/[`assemble_content_node`] (which do
/// that processing themselves, in one shot, for their raw inputs) and by
/// progressive per-move persistence (§S6 follow-up — `tag_move_html`),
/// which has already processed each move's HTML by the time it gets here
/// and must not run `render_math` a second time: MathML's `<annotation>`
/// carries the source LaTeX as literal text, and text containing its own
/// `$`/`\(` sequences (rare, but not impossible — e.g. currency inside a
/// worked example) would otherwise be mistaken for a second, nested formula
/// on the re-scan. `ensure_block_ids` is safe to call again (skips anything
/// already tagged) but there is nothing left for it to do here either.
fn wrap_article(
    doc_id: &str,
    node_id: &str,
    content_section_inner: &str,
) -> Result<Node, EngineError> {
    // QA/debugging traceability (not a data-contract field): the build's
    // short git SHA, as a leading HTML comment inside the content section —
    // in the spirit of the codebase's existing `<!--tactics: ...-->`
    // sentinel, but never stripped, since it isn't a model output the
    // client needs scrubbed. `ContentLayer::html` is `inner_html()`
    // verbatim, and none of `parse_content`'s selectors
    // (`[data-block-id]`/`span[data-objective-id]`/`cite`/
    // `form[data-exercise-id]`) can match a comment node, so this is inert
    // for blocks/objectives/citations/exercise and survives the
    // parse/`to_html` round-trip unchanged (see `node.rs`'s
    // `stamps_the_build_version_and_survives_round_trip`). `wrap_article` is
    // the single door every node — content-only, partial, or finalized —
    // passes through (`assemble_content_node`, `assemble_partial_node`,
    // `finalize_node`), so this stamps once, here, rather than at each caller.
    let article = format!(
        "<article data-node-id=\"{node_id}\" data-doc-id=\"{doc_id}\">\n  \
         <section data-layer=\"content\">\n  {BUILD_MARKER_PREFIX}{APP_VERSION}-->\n{content_section_inner}\n  </section>\n  \
         <section data-layer=\"interaction\"></section>\n</article>"
    );
    Node::parse(&article).map_err(|e| EngineError::Parse(e.to_string()))
}

const BUILD_MARKER_PREFIX: &str = "<!--learnive-build: ";

/// Strips a leading build-version marker [`wrap_article`] already stamped,
/// so resuming a node's content across a fresh `wrap_article` call (§14
/// resilience — `api::reading::prepare` reseeding a move loop's
/// `content_html` from a prior, interrupted attempt's progressively
/// persisted partial node, `node_generation_resumes_after_an_interrupted_move`)
/// stamps exactly one marker, not one nested inside the leftover from last
/// time. A no-op on content with no marker (a node's first-ever attempt).
pub(crate) fn strip_build_marker(content_section_inner: &str) -> &str {
    let trimmed = content_section_inner.trim_start();
    let Some(rest) = trimmed.strip_prefix(BUILD_MARKER_PREFIX) else {
        return content_section_inner;
    };
    match rest.find("-->") {
        Some(end) => rest[end + 3..].trim_start_matches('\n'),
        None => content_section_inner,
    }
}

/// Assembles a dialect node from the generated prose and the exercise (§4.2/§4.3)
/// in one shot, from raw untagged inputs. The server assigns the IDs (blocks,
/// exercise, rubric). No longer called in production: `finalize` (api/reading.rs)
/// now uses [`finalize_node`] against already-tagged, progressively-persisted
/// content (§S6 follow-up) — kept `#[cfg(test)]` as the one-shot reference shape
/// engine.rs's and movement.rs's own tests build fixtures against.
#[cfg(test)]
pub fn assemble_node(
    doc_id: &str,
    node_id: &str,
    prose_inner_html: &str,
    exercise_html: &str,
    exercise_id: &str,
    rubric_id: &str,
) -> Result<Node, EngineError> {
    let blocks = ensure_block_ids(&render_math(prose_inner_html), &format!("{node_id}-b"));
    let form = ensure_form_ids(exercise_html, exercise_id, rubric_id);
    wrap_article(doc_id, node_id, &format!("{blocks}\n{form}"))
}

/// Assembles a **content-only** node — no exercise/form (§S8 sub-nodes,
/// scoped to this slice as prose-only elaborations, never gated, and §S6
/// follow-up: also the shape of a node still mid-generation, before its
/// graded move exists). `Node`'s `content.exercise` parses to `None` when
/// there's no `<form data-exercise-id>` in the content section, so this is
/// a strict subset of `assemble_node`, not a different dialect — the same
/// node id upgrades from this shape to the full one in place when
/// `finalize` later runs, no migration needed.
pub fn assemble_content_node(
    doc_id: &str,
    node_id: &str,
    prose_inner_html: &str,
) -> Result<Node, EngineError> {
    let blocks = ensure_block_ids(&render_math(prose_inner_html), &format!("{node_id}-b"));
    wrap_article(doc_id, node_id, &blocks)
}

/// Tags one move's raw generated HTML with stable, permanent `data-block-id`s
/// and renders its math — the per-move half of progressive persistence
/// (§S6 follow-up). Each move gets its own id prefix (`{node_id}-m{index}-b`)
/// rather than the single running `{node_id}-b` prefix `assemble_node` uses
/// for a whole node at once: moves are tagged independently, in the order
/// they complete, with no shared counter between them, so per-move prefixes
/// are what keep two different moves from ever minting the same id. Once
/// tagged, a move's HTML is done being processed for good — every later
/// assembly (the next move's progressive write, `finalize`) only
/// concatenates it, via [`wrap_article`], never re-tags or re-renders it
/// (`ensure_block_ids` would just skip it; `render_math` must never see it
/// again at all, see `wrap_article`'s doc comment).
pub fn tag_move_html(node_id: &str, move_index: usize, html: &str) -> String {
    ensure_block_ids(&render_math(html), &format!("{node_id}-m{move_index}-b"))
}

/// Persists a node's content layer as it streams in, one move at a time
/// (§S6 follow-up) — every move HTML passed in must already be
/// [`tag_move_html`]-processed; this only concatenates and wraps them, via
/// [`wrap_article`], mirroring [`assemble_content_node`]'s shape without
/// redoing its processing.
pub fn assemble_partial_node(
    doc_id: &str,
    node_id: &str,
    tagged_content_html: &str,
) -> Result<Node, EngineError> {
    wrap_article(doc_id, node_id, tagged_content_html)
}

/// `finalize`'s assembly (§S6 follow-up, replacing its old direct call to
/// [`assemble_node`]): `tagged_content_html` is every prior move's HTML,
/// already [`tag_move_html`]-processed and progressively persisted as it
/// streamed — only the exercise form, generated fresh at the very end, still
/// needs tagging here. Splitting this from [`assemble_node`] rather than
/// having callers pre-render and pass a no-op-shaped input through it keeps
/// "already tagged, do not reprocess" a type-level distinction (a plain
/// `&str` vs. one more parameter easy to pass wrong) instead of a runtime one.
pub fn finalize_node(
    doc_id: &str,
    node_id: &str,
    tagged_content_html: &str,
    exercise_html: &str,
    exercise_id: &str,
    rubric_id: &str,
) -> Result<Node, EngineError> {
    let form = ensure_form_ids(exercise_html, exercise_id, rubric_id);
    wrap_article(doc_id, node_id, &format!("{tagged_content_html}\n{form}"))
}

/// A full, non-streamed completion (for calls never rendered live to the
/// reader — §14: streaming exists for TTFT on moves shown token-by-token;
/// a structured/JSON-only call like an outline or a rubric proposal was
/// always buffered whole before use anyway, so requesting `stream: true`
/// and reassembling it bought nothing). Also reused by `movement.rs` (S2) —
/// `decide_move`/`generate_move` are non-streamed the same way
/// outline/exercise/grading are.
///
/// Confirmed live (2026-08-17) this isn't just a latency nicety: against a
/// reasoning-heavy model, the streaming path routed its entire output
/// through the `reasoning` delta and only sometimes reached a final
/// `content` chunk at all, while the identical prompt via `stream: false`
/// reliably returned a clean, complete answer — `Ai::complete` sidesteps
/// that class of failure structurally instead of degrading around it.
pub(crate) async fn collect(
    ai: &Ai,
    tier: Tier,
    messages: Vec<ChatMessage>,
) -> Result<String, EngineError> {
    Ok(ai.complete(tier, messages).await?)
}

/// Injects `data-exercise-id`/`data-rubric-id`/`data-block-id` into the first
/// `<form>` (wrapping the exercise in one if there is none). The block id
/// reuses `exercise_id` verbatim — no separate numbering scheme — so the
/// exercise is a real, addressable §4.3 content-layer block like any other
/// (anchoring, the reading line), not a special case. `prose_blocks_only`
/// still keeps it out of the unsandboxed prose HTML, by the `data-exercise-id`
/// marker rather than by it lacking a block id.
fn ensure_form_ids(exercise_html: &str, exercise_id: &str, rubric_id: &str) -> String {
    let with_form = if exercise_html.contains("<form") {
        exercise_html.to_string()
    } else {
        format!("<form>{exercise_html}</form>")
    };
    match with_form.find("<form") {
        Some(pos) => {
            let insert_at = pos + "<form".len();
            let mut s = String::with_capacity(with_form.len() + 64);
            s.push_str(&with_form[..insert_at]);
            s.push_str(&format!(
                r#" data-exercise-id="{exercise_id}" data-rubric-id="{rubric_id}" data-block-id="{exercise_id}""#
            ));
            s.push_str(&with_form[insert_at..]);
            s
        }
        None => with_form,
    }
}

/// Renders a sandboxed frame's full HTML document (§4.4): the exercise (or,
/// later, any interactive block) plus a small harness for theme sync, height
/// reporting and — only when `graded` — collecting and posting the answer
/// artifact back to the parent.
///
/// This used to be built **client-side** as `iframe.srcdoc` (harness inlined
/// in the page script). `srcdoc` documents inherit the parent page's CSP — not
/// an oversight, a browser rule — so the moment the app's own CSP drops
/// `'unsafe-inline'` (a planned hardening), the inline harness `<script>`
/// here would stop running too. Serving this as a real HTTP response lets
/// the frame carry its **own** CSP header (via `security::guard`'s
/// insert-only-if-absent CSP, see that module), independent of the app
/// origin's policy. Isolation still comes from `sandbox="allow-scripts"`
/// with no `allow-same-origin` on the `<iframe>` (§3.1/§4.4) — this frame's
/// own CSP is a second, orthogonal layer, not the isolation boundary itself.
pub fn render_sandbox_frame(
    html: &str,
    theme: &str,
    graded: bool,
    locale: crate::locale::Locale,
) -> String {
    let theme = if theme == "light" { "light" } else { "dark" };
    let submit_label = crate::locale::pick(locale, "Submit answer", "Enviar resposta");
    let submit_harness = if graded {
        format!(
            r#"function collect(){{var f=document.querySelector('form');var o={{}};if(f){{new FormData(f).forEach(function(v,k){{o[k]=v;}});}}else{{var t=document.querySelector('textarea,input');if(t)o.answer=t.value;}}return o;}}
function send(){{parent.postMessage({{type:'learnive-answer',answer:JSON.stringify(collect())}},'*');}}
var form=document.querySelector('form');if(form)form.setAttribute('novalidate','');
document.addEventListener('submit',function(e){{e.preventDefault();send();}});
var sb=document.querySelector('button[type=submit],input[type=submit]');
if(!sb){{var bs=document.querySelectorAll('button');if(bs.length===1&&(form||document.querySelector('input,textarea,select')))sb=bs[0];}}
if(sb){{sb.addEventListener('click',function(e){{e.preventDefault();send();}});}}
else if(!document.querySelector('button,input[type=submit],input[type=image]')){{var p=document.createElement('p');var b=document.createElement('button');b.type='button';b.textContent='{submit_label}';b.addEventListener('click',function(e){{e.preventDefault();send();}});p.appendChild(b);document.body.appendChild(p);}}"#
        )
    } else {
        String::new()
    };
    format!(
        r#"<!doctype html><meta charset="utf-8"><style>
html,body{{margin:0}}
body{{font-family:'Ubuntu Mono','JetBrains Mono',monospace;padding:.6rem;line-height:1.5;background:#3b4252;color:#d8dee9}}
body[data-t='light']{{background:#eceff4;color:#2e3440}}
button{{font:inherit;padding:.4rem .8rem;border-radius:3px;border:1px solid #5e81ac;background:#5e81ac;color:#eceff4;cursor:pointer}}
button:hover{{background:#81a1c1;border-color:#81a1c1}}
input,textarea,select{{font:inherit;background:#434c5e;color:#d8dee9;border:1px solid #4c566a;border-radius:3px;padding:.35rem}}
textarea{{width:100%;box-sizing:border-box}}
body[data-t='light'] input,body[data-t='light'] textarea,body[data-t='light'] select{{background:#fff;color:#2e3440;border-color:#d8dee9}}
a{{color:#88c0d0}}body[data-t='light'] a{{color:#5e81ac}}
label{{display:inline-block}}
</style><body data-t="{theme}">{html}<script>(function(){{
{submit_harness}
function reportHeight(){{parent.postMessage({{type:'learnive-height',height:document.documentElement.scrollHeight}},'*');}}
function applyTheme(t){{document.body.setAttribute('data-t',t);document.documentElement.style.colorScheme=t;reportHeight();}}
window.addEventListener('message',function(e){{var d=e.data;if(d&&d.type==='learnive-theme')applyTheme(d.theme==='light'?'light':'dark');}});
if(window.ResizeObserver){{new ResizeObserver(reportHeight).observe(document.body);}}
window.addEventListener('load',reportHeight);setTimeout(reportHeight,60);
if(document.fonts&&document.fonts.ready)document.fonts.ready.then(reportHeight);
applyTheme(document.body.getAttribute('data-t'));
}})();</script>"#
    )
}

pub mod prompt;

pub mod parse;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{MockProvider, Models, Provider};

    fn outline_item(id: &str, source_doc_id: Option<&str>) -> OutlineItem {
        OutlineItem {
            id: id.to_string(),
            title: id.to_string(),
            prerequisites: Vec::new(),
            parent_id: None,
            mode: NodeMode::Learn,
            source_doc_id: source_doc_id.map(String::from),
            item_type: OutlineItemType::Node,
            expansion: ExpansionState::NotExpanded,
            source: None,
            chapter_number: None,
            resolved_page: None,
        }
    }

    #[test]
    fn owner_of_resolves_local_items_to_the_calling_document() {
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![outline_item("n1", None)],
        };
        assert_eq!(owner_of(&outline, "doc-a", "n1"), "doc-a");
    }

    #[test]
    fn owner_of_resolves_a_reference_to_its_source_document() {
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![outline_item("n1", Some("doc-owner"))],
        };
        assert_eq!(owner_of(&outline, "doc-visitor", "n1"), "doc-owner");
    }

    #[test]
    fn owner_of_falls_back_to_the_caller_for_an_unknown_id() {
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![outline_item("n1", Some("doc-owner"))],
        };
        assert_eq!(
            owner_of(&outline, "doc-visitor", "no-such-id"),
            "doc-visitor"
        );
    }

    fn book_pointer(title: &str) -> SourcePointer {
        SourcePointer {
            item: crate::source::ProposedItem {
                title: title.to_string(),
                authors: vec!["Author".to_string()],
                year: None,
                edition: None,
                identifier: None,
                kind: crate::source::SourceKind::Book,
            },
            verification: None,
        }
    }

    #[test]
    fn resolve_grounding_source_returns_a_books_own_pointer() {
        let mut book = outline_item("book1", None);
        book.item_type = OutlineItemType::Book;
        book.source = Some(book_pointer("Rosen"));
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![book.clone()],
        };
        assert_eq!(
            resolve_grounding_source(&outline, &book),
            Some(book_pointer("Rosen"))
        );
    }

    #[test]
    fn resolve_grounding_source_walks_parent_id_to_an_ancestor_book() {
        let mut book = outline_item("book1", None);
        book.item_type = OutlineItemType::Book;
        book.source = Some(book_pointer("Rosen"));
        let mut chapter = outline_item("ch1", None);
        chapter.parent_id = Some("book1".to_string());
        let mut node = outline_item("node1", None);
        node.parent_id = Some("ch1".to_string());
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![book, chapter, node.clone()],
        };
        assert_eq!(
            resolve_grounding_source(&outline, &node),
            Some(book_pointer("Rosen"))
        );
    }

    #[test]
    fn resolve_grounding_source_is_none_with_no_bibliographic_ancestor() {
        let node = outline_item("node1", None);
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![node.clone()],
        };
        assert_eq!(resolve_grounding_source(&outline, &node), None);
    }

    fn chapters_under(book_id: &str, ids: &[&str]) -> Vec<OutlineItem> {
        ids.iter()
            .map(|id| {
                let mut chapter = outline_item(id, None);
                chapter.item_type = OutlineItemType::Chapter;
                chapter.parent_id = Some(book_id.to_string());
                chapter
            })
            .collect()
    }

    #[test]
    fn is_generable_is_true_for_a_book_with_no_chapter_children() {
        let mut book = outline_item("book1", None);
        book.item_type = OutlineItemType::Book;
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![book.clone()],
        };
        assert!(is_generable(&outline, &book));
    }

    #[test]
    fn is_generable_is_false_for_a_book_with_chapter_children() {
        let mut book = outline_item("book1", None);
        book.item_type = OutlineItemType::Book;
        let mut items = vec![book.clone()];
        items.extend(chapters_under("book1", &["c1", "c2"]));
        let outline = Outline {
            topic: "t".to_string(),
            items,
        };
        assert!(!is_generable(&outline, &book));
    }

    #[test]
    fn is_generable_is_always_true_for_a_plain_node() {
        let node = outline_item("n1", None);
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![node.clone()],
        };
        assert!(is_generable(&outline, &node));
    }

    #[test]
    fn is_generable_is_true_for_a_chapter_with_no_node_children() {
        let mut book = outline_item("book1", None);
        book.item_type = OutlineItemType::Book;
        let mut items = vec![book];
        items.extend(chapters_under("book1", &["c1"]));
        let outline = Outline {
            topic: "t".to_string(),
            items,
        };
        assert!(is_generable(&outline, &outline.items[1]));
    }

    /// S27g item 2 (2026-08-30): a `Chapter` that split into `Node`
    /// children stops being generable itself — same rule as a `Book` whose
    /// `propose_outline`-time `Chapter` children arrived, generalized one
    /// level deeper.
    #[test]
    fn is_generable_is_false_for_a_chapter_with_node_children() {
        let mut book = outline_item("book1", None);
        book.item_type = OutlineItemType::Book;
        let mut chapter = outline_item("c1", None);
        chapter.item_type = OutlineItemType::Chapter;
        chapter.parent_id = Some("book1".to_string());
        let mut node = outline_item("n1", None);
        node.parent_id = Some("c1".to_string());
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![book, chapter.clone(), node],
        };
        assert!(!is_generable(&outline, &chapter));
    }

    fn node_states(
        pairs: &[(&str, crate::events::aggregate::NodeState)],
    ) -> std::collections::HashMap<String, crate::events::aggregate::NodeState> {
        pairs.iter().map(|(id, s)| (id.to_string(), *s)).collect()
    }

    #[test]
    fn effective_state_is_none_for_a_container_with_no_children_touched_yet() {
        let mut book = outline_item("book1", None);
        book.item_type = OutlineItemType::Book;
        let mut items = vec![book];
        items.extend(chapters_under("book1", &["c1", "c2"]));
        let outline = Outline {
            topic: "t".to_string(),
            items,
        };
        let states = node_states(&[]);
        assert_eq!(effective_state(&outline, &states, "book1"), None);
    }

    #[test]
    fn effective_state_is_none_while_only_some_chapters_are_done() {
        use crate::events::aggregate::NodeState;
        let mut book = outline_item("book1", None);
        book.item_type = OutlineItemType::Book;
        let mut items = vec![book];
        items.extend(chapters_under("book1", &["c1", "c2"]));
        let outline = Outline {
            topic: "t".to_string(),
            items,
        };
        let states = node_states(&[("c1", NodeState::Demonstrated)]);
        assert_eq!(effective_state(&outline, &states, "book1"), None);
    }

    #[test]
    fn effective_state_synthesizes_demonstrated_once_every_chapter_is_settled() {
        use crate::events::aggregate::NodeState;
        let mut book = outline_item("book1", None);
        book.item_type = OutlineItemType::Book;
        let mut items = vec![book];
        items.extend(chapters_under("book1", &["c1", "c2"]));
        let outline = Outline {
            topic: "t".to_string(),
            items,
        };
        // A skipped chapter satisfies the container exactly like a
        // demonstrated one — the same rule an ordinary prerequisite follows.
        let states = node_states(&[("c1", NodeState::Demonstrated), ("c2", NodeState::Skipped)]);
        assert_eq!(
            effective_state(&outline, &states, "book1"),
            Some(NodeState::Demonstrated)
        );
    }

    #[test]
    fn effective_state_falls_back_to_a_direct_lookup_for_a_plain_node() {
        use crate::events::aggregate::NodeState;
        let node = outline_item("n1", None);
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![node],
        };
        let states = node_states(&[("n1", NodeState::Attempted)]);
        assert_eq!(
            effective_state(&outline, &states, "n1"),
            Some(NodeState::Attempted)
        );
        assert_eq!(effective_state(&outline, &states, "no-such-id"), None);
    }

    /// S27g item 2 (2026-08-30): a `Book → Chapter → Node` chain where the
    /// chapter split needs no special-casing in `effective_state` itself —
    /// it recurses into the chapter's own `Node` children exactly the same
    /// way it recurses into the book's `Chapter` children.
    #[test]
    fn effective_state_recurses_through_a_split_chapter_to_its_nodes() {
        use crate::events::aggregate::NodeState;
        let mut book = outline_item("book1", None);
        book.item_type = OutlineItemType::Book;
        let mut chapter = outline_item("c1", None);
        chapter.item_type = OutlineItemType::Chapter;
        chapter.parent_id = Some("book1".to_string());
        let mut node = outline_item("n1", None);
        node.parent_id = Some("c1".to_string());
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![book, chapter, node],
        };
        // The chapter itself never generates (it split); only its node did.
        let states = node_states(&[("n1", NodeState::Demonstrated)]);
        assert_eq!(
            effective_state(&outline, &states, "c1"),
            Some(NodeState::Demonstrated)
        );
        assert_eq!(
            effective_state(&outline, &states, "book1"),
            Some(NodeState::Demonstrated)
        );
    }

    #[test]
    fn resume_leaf_returns_the_item_itself_once_generated() {
        let node = outline_item("n1", None);
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![node],
        };
        let generated: std::collections::HashSet<String> = ["n1".to_string()].into();
        assert_eq!(
            resume_leaf(&outline, &generated, "n1"),
            Some("n1".to_string())
        );
    }

    #[test]
    fn resume_leaf_descends_two_levels_into_a_split_chapters_node() {
        let mut book = outline_item("book1", None);
        book.item_type = OutlineItemType::Book;
        let mut chapter = outline_item("c1", None);
        chapter.item_type = OutlineItemType::Chapter;
        chapter.parent_id = Some("book1".to_string());
        let mut node = outline_item("n1", None);
        node.parent_id = Some("c1".to_string());
        let outline = Outline {
            topic: "t".to_string(),
            items: vec![book, chapter, node],
        };
        // Neither the book nor the chapter itself was ever generated —
        // only the node two levels down.
        let generated: std::collections::HashSet<String> = ["n1".to_string()].into();
        assert_eq!(
            resume_leaf(&outline, &generated, "book1"),
            Some("n1".to_string())
        );
    }

    #[test]
    fn resume_leaf_is_none_when_nothing_under_the_item_ever_generated() {
        let mut book = outline_item("book1", None);
        book.item_type = OutlineItemType::Book;
        let mut items = vec![book];
        items.extend(chapters_under("book1", &["c1"]));
        let outline = Outline {
            topic: "t".to_string(),
            items,
        };
        let generated: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert_eq!(resume_leaf(&outline, &generated, "book1"), None);
    }

    fn mock_ai(reply: &str) -> Ai {
        Ai::new(
            Provider::Mock(MockProvider::new(reply)),
            Models::single("mock"),
        )
    }

    /// Same test-double convention as `movement.rs`/`profile.rs`: a
    /// provider whose reply is computed from the actual request, for
    /// asserting on prompt CONTENT (e.g. S27e's rejected-title retry) —
    /// `mock_ai` above only fixes the reply, not what was asked.
    fn scripted_ai<F>(f: F) -> Ai
    where
        F: Fn(&crate::ai::ChatRequest) -> String + Send + Sync + 'static,
    {
        Ai::new(
            Provider::Mock(MockProvider::scripted(f)),
            Models::single("mock"),
        )
    }

    fn full_text(req: &crate::ai::ChatRequest) -> String {
        req.messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn strip_build_marker_removes_a_leading_stamp_only() {
        let stamped = "  <!--learnive-build: abc1234-->\n<p data-block-id=\"b1\">Hello.</p>";
        assert_eq!(
            strip_build_marker(stamped),
            "<p data-block-id=\"b1\">Hello.</p>"
        );

        // No marker present: unchanged.
        let unstamped = "<p data-block-id=\"b1\">Hello.</p>";
        assert_eq!(strip_build_marker(unstamped), unstamped);

        // A marker-shaped comment elsewhere in the content (not leading) is
        // left alone — this only ever strips the one `wrap_article` itself
        // stamps at the very front.
        let mid = "<p>text</p><!--learnive-build: abc1234-->";
        assert_eq!(strip_build_marker(mid), mid);
    }

    #[test]
    fn parse_outline_tree_json() {
        // S27e: the reading-list schema is exactly `source::ProposedItem`'s
        // own shape, one object per book/article — no `children`.
        let nodes = parse::outline_tree(
            r#"[{"title":"Calculus, Volume 1","authors":["Stewart, James"],"year":2015,"edition":"8","identifier":null,"kind":"book"},
                {"title":"On the fundamental theorem","authors":[],"year":null,"edition":null,"identifier":null,"kind":"article"}]"#,
        )
        .unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].title, "Calculus, Volume 1");
        assert_eq!(nodes[0].item_type, OutlineItemType::Book);
        assert!(nodes[0].children.is_empty());
        assert_eq!(
            nodes[0].bibliography.as_ref().unwrap().kind,
            crate::source::SourceKind::Book
        );
        assert_eq!(nodes[1].title, "On the fundamental theorem");
        assert_eq!(nodes[1].item_type, OutlineItemType::Article);

        assert!(parse::outline_tree("not json").is_none());
        // Missing required `ProposedItem` fields (no `authors`/`kind`) is a
        // parse failure too, not a silently-defaulted item — this schema is
        // strict, unlike the old free-form `{title, children}` shape.
        assert!(parse::outline_tree(r#"[{"title":"Missing fields"}]"#).is_none());
    }

    /// S27g (introduced 2026-08-29 as `topics`, reversed to number+name
    /// 2026-08-30): a non-empty `chapters` array becomes `Chapter`-typed
    /// `children`, each carrying `number` on `chapter_number` and `name` as
    /// its own `title`, with no bibliography of its own (it inherits the
    /// parent's, `resolve_grounding_source`) — a blank `name` is dropped
    /// rather than materialized as an empty-titled chapter, and a missing or
    /// blank `number` becomes `None`, never an empty string.
    #[test]
    fn parse_outline_tree_chapters_become_chapter_children() {
        let nodes = parse::outline_tree(
            r#"[{"title":"The C Programming Language","authors":["Kernighan, Brian W."],"year":1988,"edition":"2nd","identifier":null,"kind":"book","chapters":[{"number":"4","name":"functions in C"},{"number":"4.10","name":"recursion in C"},{"number":"  ","name":"  "}]},
                {"title":"Calculus, Volume 1","authors":["Stewart, James"],"year":2015,"edition":"8","identifier":null,"kind":"book","chapters":[]}]"#,
        )
        .unwrap();
        assert_eq!(nodes.len(), 2);

        assert_eq!(nodes[0].item_type, OutlineItemType::Book);
        assert_eq!(nodes[0].children.len(), 2);
        assert_eq!(nodes[0].children[0].title, "functions in C");
        assert_eq!(nodes[0].children[0].item_type, OutlineItemType::Chapter);
        assert_eq!(nodes[0].children[0].chapter_number.as_deref(), Some("4"));
        assert!(nodes[0].children[0].bibliography.is_none());
        assert_eq!(nodes[0].children[1].title, "recursion in C");
        assert_eq!(nodes[0].children[1].chapter_number.as_deref(), Some("4.10"));

        // An empty `chapters` array materializes no children at all — the
        // "whole work is in scope" case stays exactly like before S27g.
        assert!(nodes[1].children.is_empty());
    }

    #[test]
    fn parse_outline_tree_a_chapter_with_no_confident_number_gets_none() {
        let nodes = parse::outline_tree(
            r#"[{"title":"The C Programming Language","authors":["Kernighan, Brian W."],"year":1988,"edition":"2nd","identifier":null,"kind":"book","chapters":[{"number":null,"name":"basic C syntax"}]}]"#,
        )
        .unwrap();
        assert_eq!(nodes[0].children[0].chapter_number, None);
    }

    #[test]
    fn parse_assessment_json() {
        let a = parse::assessment(
            r#"{"grades":[{"objective_id":"o1","grade":"demonstrated","feedback":"ok"}]}"#,
        )
        .unwrap();
        assert!(a.all_demonstrated());
        assert!(a.unmet().is_empty());
    }

    #[test]
    fn assessment_unmet_blocks_advance() {
        let a = Assessment {
            grades: vec![
                ObjectiveGrade {
                    objective_id: "o1".into(),
                    grade: Grade::Demonstrated,
                    feedback: String::new(),
                },
                ObjectiveGrade {
                    objective_id: "o2".into(),
                    grade: Grade::Partial,
                    feedback: String::new(),
                },
            ],
        };
        assert!(!a.all_demonstrated());
        assert_eq!(a.unmet().len(), 1);
        assert_eq!(a.unmet()[0].objective_id, "o2");
    }

    #[test]
    fn assemble_node_wraps_prose_and_exercise() {
        let node = assemble_node(
            "d1",
            "n1",
            "<h2>Limits</h2><p>Explanation.</p>",
            "<form><input name=\"r\"></form>",
            "ex1",
            "ru1",
        )
        .unwrap();
        assert!(node.content.blocks.len() >= 2);
        let ex = node.content.exercise.unwrap();
        assert_eq!(ex.exercise_id, "ex1");
        assert_eq!(ex.rubric_id.as_deref(), Some("ru1"));
    }

    #[test]
    fn assemble_content_node_has_no_exercise() {
        // §S8: a spawned sub-node is prose-only in this slice — no form, no
        // gate. `Node::parse` degrades to `exercise: None` when there's no
        // `<form data-exercise-id>`, so this is a strict subset of
        // `assemble_node`'s dialect, not a different one.
        let node =
            assemble_content_node("d1", "sub1", "<h3>Deeper look</h3><p>More detail.</p>").unwrap();
        assert!(node.content.blocks.len() >= 2);
        assert!(node.content.exercise.is_none());
    }

    /// §S6 follow-up: two moves tagged independently (their own
    /// `{node_id}-m{index}-b` prefix each) must never collide, and
    /// `finalize_node` — which only tags the exercise, never re-tagging or
    /// re-rendering the already-processed moves it's handed — must produce
    /// the exact same block ids `tag_move_html` minted, not fresh ones.
    #[test]
    fn progressive_move_tagging_composes_into_the_final_node() {
        let move0 = tag_move_html("n1", 0, "<p>First move.</p>");
        let move1 = tag_move_html("n1", 1, "<p>Second move.</p>");
        assert!(move0.contains("data-block-id=\"n1-m0-b"));
        assert!(move1.contains("data-block-id=\"n1-m1-b"));

        let partial = assemble_partial_node("d1", "n1", &format!("{move0}\n{move1}")).unwrap();
        assert_eq!(partial.content.blocks.len(), 2);
        assert!(partial.content.exercise.is_none());
        let ids: Vec<&str> = partial
            .content
            .blocks
            .iter()
            .map(|b| b.id.as_str())
            .collect();

        let final_node = finalize_node(
            "d1",
            "n1",
            &format!("{move0}\n{move1}"),
            "<form><input name=\"r\"></form>",
            "n1-ex",
            "n1-ru",
        )
        .unwrap();
        // The prose blocks carry forward verbatim — same ids, not
        // re-minted — plus the exercise, tagged fresh.
        let final_ids: Vec<&str> = final_node
            .content
            .blocks
            .iter()
            .map(|b| b.id.as_str())
            .filter(|id| *id != "n1-ex")
            .collect();
        assert_eq!(final_ids, ids);
        assert_eq!(final_node.content.exercise.unwrap().exercise_id, "n1-ex");
    }

    /// §S6 follow-up: `finalize_node` must never re-render math in content
    /// it's handed — that content already went through `tag_move_html`'s
    /// `render_math` once, and MathML's `<annotation>` carries the raw
    /// LaTeX as literal text, so a second `render_math` pass over it is not
    /// guaranteed to be a no-op (a `$`/`\(`-like sequence inside the
    /// annotation could be mistaken for a second, nested formula). Proven
    /// structurally, not just for one input: the prose section of
    /// `finalize_node`'s output must be byte-identical to what
    /// `tag_move_html` produced, not merely equivalent.
    #[test]
    fn finalize_node_does_not_re_render_already_tagged_math() {
        let tagged = tag_move_html("n1", 0, r"<p>A metade é $\frac{1}{2}$ do todo.</p>");
        assert!(tagged.contains("<math"), "{tagged}");

        let node = finalize_node(
            "d1",
            "n1",
            &tagged,
            "<form><input name=\"r\"></form>",
            "n1-ex",
            "n1-ru",
        )
        .unwrap();
        assert!(
            node.content.html.contains(&tagged),
            "finalize_node must pass already-tagged prose through byte-identical, \
             not reprocess it: {}",
            node.content.html
        );
    }

    #[test]
    fn ask_decision_parses_spawn_with_title() {
        let d = parse::ask_decision(r#"{"spawn":true,"title":"Deeper dive"}"#).unwrap();
        assert_eq!(
            d,
            AskDecision::Spawn {
                title: "Deeper dive".to_string()
            }
        );
    }

    #[test]
    fn ask_decision_defaults_to_inline_on_spawn_false() {
        let d = parse::ask_decision(r#"{"spawn":false,"title":"ignored"}"#).unwrap();
        assert_eq!(d, AskDecision::Inline);
    }

    #[test]
    fn ask_decision_collapses_to_inline_when_title_is_blank() {
        // A model that says "spawn" but gives nothing to call the new
        // section must not mint an untitled node — degrade to inline rather
        // than trust half of a malformed decision.
        let d = parse::ask_decision(r#"{"spawn":true,"title":"  "}"#).unwrap();
        assert_eq!(d, AskDecision::Inline);
    }

    #[tokio::test]
    async fn decide_ask_response_via_mock() {
        let ai = mock_ai(r#"{"spawn":true,"title":"A new section"}"#);
        let decision =
            decide_ask_response(&ai, "fractions", "Equivalent fractions", "", None, "why?")
                .await
                .unwrap();
        assert_eq!(
            decision,
            AskDecision::Spawn {
                title: "A new section".to_string()
            }
        );
    }

    #[test]
    fn assemble_node_handles_multiple_sentinel_stripped_moves() {
        // Mirrors api.rs::generate_node: each streamed move's html is already
        // sentinel-stripped by movement::finish_streamed_move before it's pushed
        // onto content_html with a trailing '\n' separator, then the whole blob
        // goes through ensure_block_ids in one shot. Confirms concatenation
        // yields exactly the blocks from real elements — no phantom empty block
        // from the '\n' separators, and no ids collide across moves. 3 prose
        // blocks (h2, 2×p) plus the exercise form itself, which also carries
        // its own real data-block-id (`ensure_form_ids`) — 4 total.
        let explain_move = "<h2>Limits</h2><p>Explanation.</p>";
        let ask_move = "<p>What happens as x approaches the boundary?</p>";
        let content_html = format!("{explain_move}\n{ask_move}\n");

        let node = assemble_node(
            "d1",
            "n1",
            &content_html,
            "<form><p>What is the limit?</p><input name=\"r\"></form>",
            "ex1",
            "ru1",
        )
        .unwrap();

        assert_eq!(node.content.blocks.len(), 4);
        let ids: std::collections::HashSet<_> =
            node.content.blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids.len(), 4, "block ids must not collide across moves");
        for block in &node.content.blocks {
            assert!(
                !block.text.trim().is_empty(),
                "no empty block should be created from move-separator whitespace"
            );
        }
    }

    /// Live report (2026-08-20): the interaction-layer prose paths
    /// (remediation, mid-reading Q&A, spawned sub-nodes) and grading feedback
    /// had no language instruction at all, same gap as the move-generation
    /// prompts. §S17 moved remediation/Q&A/spawn generation onto the move
    /// ABI (`movement::prompt`, covered by its own
    /// `every_content_prompt_carries_the_locale_directive`); `grading` is
    /// the one interaction-layer prompt still living here.
    #[test]
    fn interaction_layer_prompts_carry_the_locale_directive() {
        use crate::locale::Locale;

        let rubric = Rubric { objectives: vec![] };
        let grading_pt =
            &prompt::grading(&rubric, "<form></form>", "answer", "", Locale::PtBr)[0].content;
        assert!(grading_pt.contains("Brazilian Portuguese"));
    }

    /// S16: `grade()` grading against a concrete answer key, not just rubric
    /// prose, is the leniency fix — and the model must be told never to
    /// hand that key back to the student through its own feedback text.
    #[test]
    fn grading_prompt_includes_reference_solution_with_a_no_leak_instruction() {
        let rubric = Rubric { objectives: vec![] };
        let with_solution = prompt::grading(
            &rubric,
            "<form></form>",
            "my answer",
            "the answer is 42",
            crate::locale::Locale::En,
        );
        assert!(with_solution[1].content.contains("the answer is 42"));
        assert!(with_solution[0].content.to_lowercase().contains("never"));

        // Empty reference_solution (older sidecar / model omitted it)
        // degrades cleanly — no dangling "Reference solution: " with
        // nothing after it.
        let without_solution = prompt::grading(
            &rubric,
            "<form></form>",
            "my answer",
            "",
            crate::locale::Locale::En,
        );
        assert!(!without_solution[1].content.contains("Reference solution"));
    }

    /// Shared by the live quality-iteration probes below: builds the REAL
    /// `Ai` from `.env`'s configured provider (must be sourced into the
    /// shell env first — `set -a; source .env; set +a`) — not a mock, so the
    /// probes exercise the actual model/tier each prompt runs on in
    /// production.
    fn live_ai_from_env() -> Ai {
        let base_url = std::env::var("LEARNIVE_API_BASE_URL").expect("set LEARNIVE_API_BASE_URL");
        let key = std::env::var("LEARNIVE_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());
        let fast =
            std::env::var("LEARNIVE_MODEL_FAST").unwrap_or_else(|_| "openai/gpt-4o-mini".into());
        let robust =
            std::env::var("LEARNIVE_MODEL_ROBUST").unwrap_or_else(|_| "openai/gpt-4o".into());
        Ai::new(
            Provider::OpenAiCompat(crate::ai::OpenAiCompat::new(base_url, key)),
            Models::new(fast, robust),
        )
    }

    fn print_outline_tree(nodes: &[ProposedOutlineNode], depth: usize, is_top: bool) {
        for (i, n) in nodes.iter().enumerate() {
            let tag = if is_top && i == nodes.len() - 1 {
                " [most-specific / objective work]"
            } else {
                ""
            };
            eprintln!(
                "{}- ({:?}) {}{tag}",
                "  ".repeat(depth + 1),
                n.item_type,
                n.title
            );
            print_outline_tree(&n.children, depth + 1, false);
        }
    }

    /// Live quality-iteration harness for `prompt::propose_outline` — not a
    /// correctness test (nothing to assert against), a print-and-eyeball
    /// loop for tuning the prompt against the REAL configured Robust-tier
    /// provider this call runs on. **S27e:** now judges the reading-list
    /// prompt (real books/articles, foundational-first) rather than the
    /// pre-pivot concept-decomposition tree — the case list is kept as-is
    /// (same topics/objectives are still a reasonable spread to eyeball
    /// against), only what's printed per case changed (`item_type` +
    /// title, no more prerequisite-vs-objective framing). Ignored by
    /// default: spends real tokens. Run with `cargo test -p learnive \
    /// engine::tests::outline_quality_probe -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "hits the real configured provider, spends tokens, for manual prompt tuning only"]
    async fn outline_quality_probe() {
        let ai = live_ai_from_env();

        let cases: &[(&str, &str)] = &[
            (
                // Narrow/self-contained: single objective node, no real
                // decomposition — but per the 2026-08-18 calibration, NOT an
                // empty prerequisite list either (array/vector indexing is
                // domain-specific, not universal literacy).
                "como funciona busca binária",
                "Entender como a busca binária encontra um valor em um vetor ordenado e implementá-la corretamente",
            ),
            (
                "adicionar e remover itens de uma lista em python",
                "Aprender a adicionar e remover elementos de uma lista em Python usando seus métodos principais",
            ),
            (
                // §S15's own spec example — should propose something like
                // álgebra/limites/derivadas as prerequisites, then decompose
                // "Integração" itself into antiderivadas/substituição/etc.
                "integração",
                "Aprender a calcular integrais de funções polinomiais e trigonométricas simples",
            ),
            (
                "termodinâmica para engenharia",
                "Compreender as leis da termodinâmica e aplicá-las a ciclos e sistemas de engenharia",
            ),
            (
                "genética básica",
                "Entender os princípios da herança genética: genes, alelos, dominância, e as leis de Mendel",
            ),
            (
                "aprendizado de máquina supervisionado",
                "Entender os fundamentos de aprendizado supervisionado: regressão, classificação, e como treinar e avaliar um modelo",
            ),
            (
                // Live report (2026-08-18, QA generation run, doc
                // azfnqbvwym): the model proposed "knowledge of the French
                // Revolution as a historical event" as a PREREQUISITE of a
                // document whose entire topic is the French Revolution —
                // exactly the self-as-prerequisite failure the prompt
                // explicitly forbids. Should come back with genuine
                // background (e.g. Ancien Régime social/political structure
                // as of the 1780s) or none, never the topic itself.
                "the causes of the French Revolution",
                "Analyze the interdependent conditions that led to the French Revolution",
            ),
            (
                // Live report (2026-08-18, QA generation run, doc
                // au1wfnmxnk): collapsed to a single node with the whole
                // topic pushed nowhere — the objective's own node should
                // decompose into several steps (what Big-O is -> common
                // complexity classes -> best/worst/average case -> comparing
                // growth), same textbook-chapter scale as the other broad
                // domains above, plus real prerequisite background (basic
                // math functions/growth rates, or "programming logic").
                "Big-O notation and algorithm complexity",
                "Quantify algorithm complexity using Big-O notation.",
            ),
        ];

        for (topic, objective) in cases {
            let tree = propose_outline(&ai, topic, objective, &[]).await;
            eprintln!("\n=== topic: {topic}\n    objective: {objective}");
            match tree {
                Ok(nodes) => print_outline_tree(&nodes, 0, true),
                Err(e) => eprintln!("  ERROR: {e:?}"),
            }
        }
    }

    #[tokio::test]
    async fn propose_outline_via_mock() {
        let ai = mock_ai(
            r#"[{"title":"Pré-Cálculo","authors":["Iezzi, Gelson"],"year":2013,"edition":"9","identifier":null,"kind":"book"},
               {"title":"Cálculo, Volume 1","authors":["Stewart, James"],"year":2015,"edition":"8","identifier":null,"kind":"book"}]"#,
        );
        let nodes = propose_outline(&ai, "calculus", "Learn integration", &[])
            .await
            .unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].title, "Pré-Cálculo");
        assert_eq!(nodes[0].item_type, OutlineItemType::Book);
        assert!(nodes[0].children.is_empty());
        // The last element is the work most directly covering the
        // objective — S27e: order alone carries the prerequisite chain now,
        // there is no more separate nested-children decomposition here.
        assert_eq!(nodes[1].title, "Cálculo, Volume 1");
        assert!(nodes[1].bibliography.is_some());
    }

    /// A self-contained objective still needing only one work is a
    /// single-element array — never empty.
    #[tokio::test]
    async fn propose_outline_no_prerequisites_is_just_the_objective() {
        let ai = mock_ai(
            r#"[{"title":"Le Petit Prince","authors":["Saint-Exupéry, Antoine de"],"year":1943,"edition":null,"identifier":null,"kind":"book"}]"#,
        );
        let nodes = propose_outline(&ai, "greetings", "Say hello in French", &[])
            .await
            .unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].title, "Le Petit Prince");
    }

    /// An empty array is never a valid answer here — there is always at
    /// least one work covering the objective itself — so an empty response
    /// can only mean the model got the contract wrong.
    #[tokio::test]
    async fn propose_outline_empty_array_is_a_parse_error() {
        let ai = mock_ai("[]");
        let err = propose_outline(&ai, "greetings", "Say hello in French", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::Parse(_)));
    }

    /// S27e's bounded retry: a re-ask names the rejected title in the
    /// system prompt so the model can propose a real substitute instead of
    /// looping on the same unverifiable work.
    #[tokio::test]
    async fn propose_outline_carries_rejected_titles_into_the_retry_prompt() {
        let rejected_title = "A Made-Up Textbook That Does Not Exist".to_string();
        let for_closure = rejected_title.clone();
        let ai = scripted_ai(move |req| {
            let text = full_text(req);
            assert!(
                text.contains(&for_closure),
                "the rejected title must reach the model: {text}"
            );
            r#"[{"title":"Cálculo, Volume 1","authors":["Stewart, James"],"year":2015,"edition":"8","identifier":null,"kind":"book"}]"#.to_string()
        });
        let nodes = propose_outline(
            &ai,
            "calculus",
            "Learn integration",
            std::slice::from_ref(&rejected_title),
        )
        .await
        .unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].title, "Cálculo, Volume 1");
    }

    #[tokio::test]
    async fn propose_objective_via_mock() {
        let ai = mock_ai(r#"{"text":"Learn enough discrete math to read CS papers"}"#);
        let proposal = propose_objective(&ai, "discrete math for a CS degree")
            .await
            .unwrap();
        assert_eq!(
            proposal.text,
            "Learn enough discrete math to read CS papers"
        );
    }

    #[tokio::test]
    async fn grade_via_mock() {
        let ai = mock_ai(
            r#"{"grades":[{"objective_id":"o1","grade":"demonstrated","feedback":"good"}]}"#,
        );
        let rubric = Rubric {
            objectives: vec![RubricObjective {
                id: "o1".into(),
                kind: ObjectiveType::Knowledge,
                description: "d".into(),
                criteria: "c".into(),
                transfer: false,
            }],
        };
        let a = grade(
            &ai,
            &rubric,
            "<form></form>",
            "my answer",
            "",
            crate::locale::Locale::En,
        )
        .await
        .unwrap();
        assert!(a.all_demonstrated());
    }

    /// S27g live check (2026-08-30, user request: "do some tests with a
    /// live provider and report the results"): drives the real,
    /// `{number, name}`-structured `propose_outline` against the REAL
    /// configured provider (`.env`), for a real book already in
    /// `learnive-data/library/` — Stewart's *Calculus: Early
    /// Transcendentals* — and resolves every proposed `Chapter` child
    /// against that PDF's OWN embedded bookmarks (`source::pdf::read_pdf`'s
    /// `outline`, not a mocked TOC) via `source::match_chapter`. This is
    /// deliberately the wiring test in place of a router-level integration
    /// test: unit coverage already exercises `match_chapter` in isolation
    /// (`toc_confirm::tests`), and a synthetic TOC can't surface what a
    /// REAL, long table of contents does — the short-title containment
    /// collision this same commit's `match_chapter_prefers_the_longer_of_
    /// two_containment_matches` regression test was written to catch was
    /// found by reasoning about exactly this scenario, not by a synthetic
    /// unit test alone. Not part of the normal suite — no oracle to assert
    /// a specific outcome against (model output varies run to run), and it
    /// spends real API budget. Prints a resolution-rate summary (proposed /
    /// carried a number / resolved to a page) rather than asserting one,
    /// since that number is inherently non-deterministic; the assertions
    /// that DO run are structural (the schema round-trips, matching never
    /// panics). Run with:
    /// `cargo test -p learnive --lib engine::tests::live_chapter_number_matching_against_a_real_book -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "hits the real configured AI provider and reads a real library PDF; run manually, see doc comment"]
    async fn live_chapter_number_matching_against_a_real_book() {
        let env_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env");
        crate::load_dotenv(env_path);
        let data_dir =
            std::env::temp_dir().join(format!("learnive-live-check-{}", std::process::id()));
        let config = crate::config::AppConfig::load(&data_dir);
        let secret = crate::secret::SecretStore::open(&data_dir);
        let (ai, _policy) = crate::api::build_ai(&config, &secret);

        let pdf_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../learnive-data/library/[Stewart's Calculus Series] James (James Stewart) Stewart - Calculus_ Early Transcendentals (2007, Brooks Cole).pdf"
        );
        let doc = crate::source::read_pdf(pdf_path).expect("read the real Stewart Calculus PDF");

        fn flatten(
            entries: &[crate::source::OutlineEntry],
            out: &mut Vec<crate::source::ConfirmedTocEntry>,
        ) {
            for e in entries {
                out.push(crate::source::ConfirmedTocEntry {
                    title: e.title.clone(),
                    number: None,
                    page: Some(e.page),
                    inferred: true,
                });
                flatten(&e.children, out);
            }
        }
        let mut toc_entries = Vec::new();
        flatten(&doc.outline, &mut toc_entries);
        println!(
            "\n=== Stewart Calculus: {} embedded bookmark entries (flattened) ===",
            toc_entries.len()
        );
        assert!(
            !toc_entries.is_empty(),
            "the fixture PDF has no embedded bookmarks — pick a different real book or fall back \
             to S27k's TOC-deduction cascade instead of `outline` directly"
        );

        let nodes = propose_outline(
            &ai,
            "Calculus: Early Transcendentals",
            "Understand derivatives, integrals, and the fundamental theorem of calculus well \
             enough to apply them to related-rates and optimization problems.",
            &[],
        )
        .await
        .expect("live propose_outline call");
        assert!(!nodes.is_empty(), "model proposed an empty reading list");

        let mut proposed = 0usize;
        let mut carried_number = 0usize;
        let mut resolved = 0usize;
        for book in &nodes {
            println!("--- proposed work: {} ---", book.title);
            for chapter in &book.children {
                if chapter.item_type != OutlineItemType::Chapter {
                    continue;
                }
                proposed += 1;
                if chapter.chapter_number.is_some() {
                    carried_number += 1;
                }
                let hit = crate::source::match_chapter(
                    &toc_entries,
                    chapter.chapter_number.as_deref(),
                    &chapter.title,
                );
                match &hit {
                    Some(entry) => {
                        resolved += 1;
                        println!(
                            "  [{:?}] {:?} -> MATCHED \"{}\" (page {:?})",
                            chapter.chapter_number, chapter.title, entry.title, entry.page
                        );
                    }
                    None => println!(
                        "  [{:?}] {:?} -> no match",
                        chapter.chapter_number, chapter.title
                    ),
                }
            }
        }
        println!(
            "\n=== resolution rate: {resolved}/{proposed} resolved, {carried_number}/{proposed} \
             carried a proposed number ==="
        );
    }
}

#[cfg(test)]
mod contract_tests {
    /// The math sub-contract must actually reach the model. `PROSE_HTML_CONTRACT`
    /// is a `const` that gets interpolated *into* other format strings, so a
    /// `{MATH_CONTRACT}` placeholder inside it is never expanded — it would ship
    /// to the model as the literal seven characters. This guards that.
    #[test]
    fn prose_contract_embeds_the_math_rules_not_a_placeholder() {
        let c = super::prompt::PROSE_HTML_CONTRACT;
        assert!(
            !c.contains("{MATH_CONTRACT}"),
            "unexpanded placeholder: {c}"
        );
        assert!(
            c.contains("$$"),
            "math delimiters missing from the contract"
        );
        assert!(c.contains("Markdown"), "markdown rule missing");
    }
}
