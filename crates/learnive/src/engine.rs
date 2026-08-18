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
/// before this one is available. A linear outline (no diamonds — everything
/// `generate_outline`/`decide_plan_proposal` produce today) is just a chain,
/// each item's sole prerequisite the previous item's id; that degenerates to
/// the old rigid one-at-a-time gate, per PLAN.md's S5 note. Multiple
/// prerequisites (a real diamond) are a data shape this already supports,
/// even though nothing generates one yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub prerequisites: Vec<String>,
    /// The tree-shaped pointer for the sidebar (§S15, extending §S8): set for
    /// a sub-node spawned from a mid-reading question OR a prerequisite
    /// decomposed by `propose_prerequisites`, either way the id of the item
    /// it's subordinate to. `None` for every item on the document's main
    /// line. This field decides SHAPE only (sidebar tree nesting, inline
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
    /// only shrinks the volume of exposure, never the evidence bar. A
    /// `skip`ped item needs no mode of its own: it is never generated at
    /// all, recorded instead as a `NodeSkipped` event (§S5).
    #[serde(default)]
    pub mode: NodeMode,
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

/// Exercise + rubric generated together (§8), in a separate call from the prose (§14).
#[derive(Debug, Clone)]
pub struct ExerciseAndRubric {
    pub exercise_html: String,
    pub rubric: Rubric,
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

/// Derives a short catalog-search phrase for source acquisition (§11) from
/// the raw topic — see `prompt::search_subject` for why this can't just reuse
/// the objective text. Fast tier: a background, non-blocking, low-stakes call.
pub async fn propose_search_subject(ai: &Ai, topic: &str) -> Result<String, EngineError> {
    let text = collect(ai, Tier::Fast, prompt::search_subject(topic)).await?;
    Ok(text.trim().trim_matches('"').to_string())
}

/// Generates the initial outline from the topic, anchored on the confirmed
/// objective (§6, §6.1, §S4). Light tier (planning).
pub async fn generate_outline(
    ai: &Ai,
    topic: &str,
    objective: &str,
) -> Result<Outline, EngineError> {
    let text = collect(ai, Tier::Fast, prompt::outline(topic, objective)).await?;
    let titles =
        parse::outline(&text).ok_or_else(|| EngineError::Parse("empty outline".to_string()))?;
    Ok(Outline {
        topic: topic.to_string(),
        items: linear_items(titles),
    })
}

/// Builds a linear prerequisite chain from titles (§S5): each item's sole
/// prerequisite is the previous item's freshly minted id, the first item has
/// none. Shared by `generate_outline` and an approved `plan` proposal
/// (`api::decide_plan_proposal`) — both today only ever produce a flat,
/// diamond-free outline.
pub fn linear_items(titles: Vec<String>) -> Vec<OutlineItem> {
    let mut items = Vec::with_capacity(titles.len());
    let mut prev_id: Option<String> = None;
    for title in titles {
        let id = new_id();
        items.push(OutlineItem {
            id: id.clone(),
            title,
            prerequisites: prev_id.into_iter().collect(),
            parent_id: None,
            mode: NodeMode::Learn,
        });
        prev_id = Some(id);
    }
    items
}

/// One node of the prerequisite tree an objective presupposes (§S15) —
/// titles only, no ids: `api::cold_start` mints those once the tree is
/// resolved against existing documents, so the client has something stable
/// to toggle and confirm against, and materialization decides gating
/// (`OutlineItem::prerequisites`) from tree structure, not from anything
/// carried here. Most concepts have no children; a concept only gets them
/// when it's genuinely a bundle of separable sub-skills that must each be
/// demonstrated on their own (the same granularity judgement `outline()`
/// already applies at the top level, recursive here at every level).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrereqNode {
    pub title: String,
    #[serde(default)]
    pub children: Vec<PrereqNode>,
}

/// Proposes the tree of prerequisite concepts an objective presupposes
/// (§S15) — a second, separate call from [`generate_outline`], with its own,
/// more generous discipline: `outline()`'s "eager generation" guard exists
/// to stop a direct answer to the learner's question from ballooning into an
/// unrelated curriculum, but a genuine prerequisite (limits/derivatives on
/// the way to integration) might need real decomposition of its own. The
/// confirmation screen this feeds — a whole tree, visible and podable branch
/// by branch before a single token of content gets generated — is the real
/// backstop against runaway breadth, not prompt restraint (§S15 "como isto
/// não vira Principia Mathematica"). An empty result (`[]`) is a normal,
/// common answer — most objectives need no prerequisites beyond common
/// knowledge — not a parse failure.
///
/// Robust tier, deliberately, though this is a titles-only call that would
/// otherwise look like "frequent cheap task" (§12.1 Fast territory): confirmed
/// live (2026-08-17) that the configured Fast-tier model — a free reasoning
/// model — reliably answered "no prerequisites" for exactly the example
/// from S15's own spec ("integração" → algebra/limites/derivadas), 0-for-6
/// across raw non-streamed provider calls and the app's own code path,
/// while the Robust-tier model produced the correct decomposition 2-for-2 on
/// the identical prompt. This is a one-time call per cold start (not
/// per-block like exercise generation), so the cost tradeoff favors
/// correctness: a wrong answer here isn't graded and corrected later like a
/// bad exercise — it silently skips the whole toggle-list confirmation step
/// this session was called in to fix in the first place.
pub async fn propose_prerequisites(
    ai: &Ai,
    topic: &str,
    objective: &str,
) -> Result<Vec<PrereqNode>, EngineError> {
    let text = collect(
        ai,
        Tier::Robust,
        prompt::propose_prerequisites(topic, objective),
    )
    .await?;
    parse::prereq_tree(&text)
        .ok_or_else(|| EngineError::Parse("could not read prerequisite tree".to_string()))
}

/// Grades an answer against the locked rubric (§8). Light tier.
pub async fn grade(
    ai: &Ai,
    rubric: &Rubric,
    exercise_html: &str,
    answer: &str,
) -> Result<Assessment, EngineError> {
    let text = collect(
        ai,
        Tier::Fast,
        prompt::grading(rubric, exercise_html, answer),
    )
    .await?;
    parse::assessment(&text)
}

/// Remediation conversation on failure (§8.2): explains the concept in the
/// exercise's context and proposes a new similar problem whose similarity grows
/// with each attempt (`attempt`). Robust tier (it is teaching/prose). Returns HTML.
pub async fn remediate(
    ai: &Ai,
    item_title: &str,
    chapter_html: &str,
    exercise_html: &str,
    answer: &str,
    unmet: &[&ObjectiveGrade],
    attempt: u32,
) -> Result<String, EngineError> {
    let unmet_summary = unmet
        .iter()
        .map(|g| format!("- {}: {}", g.objective_id, g.feedback))
        .collect::<Vec<_>>()
        .join("\n");
    let html = collect(
        ai,
        Tier::Robust,
        prompt::remediation(
            item_title,
            chapter_html,
            exercise_html,
            answer,
            &unmet_summary,
            attempt,
        ),
    )
    .await?;
    // Rendered here, not at assembly: this prose goes straight into the
    // append-only interaction layer (§4.3) as `body_html` and never passes
    // through `assemble_node`, which is where every other path picks up math
    // rendering. Applying it in both places would be a wasted second pass.
    Ok(render_math(&html))
}

/// Answers a question asked mid-reading (§S6, §9 "the document is the
/// answer"): either about a text selection or, with no selection, the
/// current reading line — the caller resolves which block/quote either way,
/// this only needs the resulting text. Robust tier: a genuine question gets
/// the same explanatory-prose treatment as `explain`/`confront` (§12.1), not
/// the fast tier reserved for cheap/structured tasks. Returns sanitized-at-
/// render HTML (`PROSE_HTML_CONTRACT`), landed in the interaction layer by
/// the caller — never in the frozen content layer.
pub async fn answer_question(
    ai: &Ai,
    topic: &str,
    item_title: &str,
    node_context: &str,
    reading_context: Option<&str>,
    question: &str,
) -> Result<String, EngineError> {
    let html = collect(
        ai,
        Tier::Robust,
        prompt::answer_question(topic, item_title, node_context, reading_context, question),
    )
    .await?;
    // Interaction layer, not assembled — same reason as `remediate`.
    Ok(render_math(&html))
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

/// Generates a spawned sub-node's prose (§7/§S8): a self-contained
/// elaboration answering the question directly, written to stand on its own
/// once spliced inline — not a reply that only makes sense next to the
/// question. Robust tier, same as `answer_question` (genuine explanatory
/// prose, §12.1), same `PROSE_HTML_CONTRACT`.
pub async fn generate_subnode_prose(
    ai: &Ai,
    topic: &str,
    sub_title: &str,
    parent_title: &str,
    node_context: &str,
    reading_context: Option<&str>,
    question: &str,
) -> Result<String, EngineError> {
    collect(
        ai,
        Tier::Robust,
        prompt::subnode_prose(
            topic,
            sub_title,
            parent_title,
            node_context,
            reading_context,
            question,
        ),
    )
    .await
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

/// Generates the NEW gradeable practice problem for the remediation loop (§8.2):
/// a sandboxed exercise + freshly locked rubric, similar to the failed one with
/// similarity increasing per `attempt`. Light tier (§12.1). This *replaces* the
/// node's active rubric so the next submission grades the new problem.
pub async fn generate_remediation_exercise(
    ai: &Ai,
    item_title: &str,
    failed_exercise: &str,
    attempt: u32,
    sources: &str,
) -> Result<ExerciseAndRubric, EngineError> {
    let text = collect(
        ai,
        Tier::Fast,
        prompt::remediation_exercise(item_title, failed_exercise, attempt, sources),
    )
    .await?;
    parse::exercise_rubric(&text)
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
    let article = format!(
        "<article data-node-id=\"{node_id}\" data-doc-id=\"{doc_id}\">\n  \
         <section data-layer=\"content\">\n{content_section_inner}\n  </section>\n  \
         <section data-layer=\"interaction\"></section>\n</article>"
    );
    Node::parse(&article).map_err(|e| EngineError::Parse(e.to_string()))
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

    fn mock_ai(reply: &str) -> Ai {
        Ai::new(
            Provider::Mock(MockProvider::new(reply)),
            Models::single("mock"),
        )
    }

    #[test]
    fn parse_outline_json_and_fallback() {
        let items = parse::outline(r#"["Intro", "Limits", "Derivatives"]"#).unwrap();
        assert_eq!(items, vec!["Intro", "Limits", "Derivatives"]);

        let items = parse::outline("- Intro\n- Limits\n2. Derivatives").unwrap();
        assert_eq!(items, vec!["Intro", "Limits", "Derivatives"]);
    }

    #[test]
    fn parse_exercise_rubric_with_fences() {
        let text = r#"```json
{"exercise_html":"<form><input name=\"a\"></form>",
 "objectives":[{"id":"o1","kind":"application","description":"apply","criteria":"gets a new case right","transfer":true}]}
```"#;
        let er = parse::exercise_rubric(text).unwrap();
        assert!(er.exercise_html.contains("<form>"));
        assert_eq!(er.rubric.objectives.len(), 1);
        assert_eq!(er.rubric.objectives[0].kind, ObjectiveType::Application);
        assert!(er.rubric.objectives[0].transfer);
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

    #[test]
    fn sanitized_surfaces_carry_the_prose_contract() {
        // Prose and remediation go to the app origin and are sanitized, so the
        // model must be told the contract (otherwise it generates something that
        // disappears).
        let rem_sys = &prompt::remediation("c", "<p>chapter</p>", "<form></form>", "a", "o1: x", 2)
            [0]
        .content;
        assert!(rem_sys.contains(prompt::PROSE_HTML_CONTRACT));

        // The exercise runs in the sandbox: opposite contract (may use JS, must postMessage).
        let ex_sys = &prompt::remediation_exercise("c", "<form></form>", 1, "")[0].content;
        assert!(ex_sys.contains("postMessage"));
        assert!(ex_sys.contains("sandbox"));
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

    /// Live quality-iteration harness for `prompt::outline` — not a
    /// correctness test (nothing to assert against), a print-and-eyeball
    /// loop for tuning the prompt against the REAL configured Fast-tier
    /// provider (the model outline generation actually runs on). Ignored by
    /// default: spends real tokens. Run with `cargo test -p learnive \
    /// engine::tests::outline_quality_probe -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "hits the real configured provider, spends tokens, for manual prompt tuning only"]
    async fn outline_quality_probe() {
        let ai = live_ai_from_env();

        let cases: &[(&str, &str)] = &[
            (
                "como funciona busca binária",
                "Entender como a busca binária encontra um valor em um vetor ordenado e implementá-la corretamente",
            ),
            (
                "adicionar e remover itens de uma lista em python",
                "Aprender a adicionar e remover elementos de uma lista em Python usando seus métodos principais",
            ),
            (
                // Deliberately NOT the same wording as `prompt::outline`'s
                // own calculus few-shot example — a near-duplicate here
                // would just show memorization, not generalization.
                "integrais",
                "Entender o que é uma integral e calcular integrais de funções polinomiais simples",
            ),
            (
                "termodinâmica para engenharia",
                "Compreender as leis da termodinâmica e aplicá-las a ciclos e sistemas de engenharia",
            ),
            (
                "derivada de x ao quadrado",
                "Calcular a derivada de uma função de potência usando a regra do poder",
            ),
            (
                // A different broad, textbook-chapter-scale domain, to check
                // the fix generalizes beyond physics/engineering topics.
                "genética básica",
                "Entender os princípios da herança genética: genes, alelos, dominância, e as leis de Mendel",
            ),
        ];

        for (topic, objective) in cases {
            let outline = generate_outline(&ai, topic, objective).await;
            eprintln!("\n=== topic: {topic}\n    objective: {objective}");
            match outline {
                Ok(o) => {
                    for item in &o.items {
                        eprintln!("  - {}", item.title);
                    }
                }
                Err(e) => eprintln!("  ERROR: {e:?}"),
            }
        }
    }

    fn print_prereq_tree(nodes: &[PrereqNode], depth: usize) {
        for n in nodes {
            eprintln!("{}- {}", "  ".repeat(depth + 1), n.title);
            print_prereq_tree(&n.children, depth + 1);
        }
    }

    /// Live quality-iteration harness for `prompt::propose_prerequisites`
    /// (§S15) — same print-and-eyeball shape as `outline_quality_probe`,
    /// against the REAL configured Robust-tier provider this call actually
    /// runs on (deliberately Robust, not Fast — see the doc comment on
    /// `propose_prerequisites` for why). Run with `cargo test -p learnive \
    /// engine::tests::prerequisites_quality_probe -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "hits the real configured provider, spends tokens, for manual prompt tuning only"]
    async fn prerequisites_quality_probe() {
        let ai = live_ai_from_env();

        let cases: &[(&str, &str)] = &[
            (
                // §S15's own spec example — should propose something like
                // álgebra/limites/derivadas.
                "integração",
                "Aprender a calcular integrais de funções polinomiais e trigonométricas simples",
            ),
            (
                // Self-contained — should come back an empty tree.
                "como funciona busca binária",
                "Entender como a busca binária encontra um valor em um vetor ordenado e implementá-la corretamente",
            ),
            (
                "aprendizado de máquina supervisionado",
                "Entender os fundamentos de aprendizado supervisionado: regressão, classificação, e como treinar e avaliar um modelo",
            ),
        ];

        for (topic, objective) in cases {
            let tree = propose_prerequisites(&ai, topic, objective).await;
            eprintln!("\n=== topic: {topic}\n    objective: {objective}");
            match tree {
                Ok(nodes) if nodes.is_empty() => eprintln!("  (empty tree)"),
                Ok(nodes) => print_prereq_tree(&nodes, 0),
                Err(e) => eprintln!("  ERROR: {e:?}"),
            }
        }
    }

    #[tokio::test]
    async fn generate_outline_via_mock() {
        let ai = mock_ai(r#"["Introduction", "Sets", "Functions"]"#);
        let outline = generate_outline(&ai, "mathematics", "Learn discrete math basics")
            .await
            .unwrap();
        assert_eq!(outline.topic, "mathematics");
        assert_eq!(outline.items.len(), 3);
        assert_eq!(outline.items[1].title, "Sets");

        // §S5: a linear chain, no diamonds — each item's sole prerequisite is
        // the previous item's id, ids are unique, the first item is free.
        assert!(outline.items[0].prerequisites.is_empty());
        assert_eq!(
            outline.items[1].prerequisites,
            vec![outline.items[0].id.clone()]
        );
        assert_eq!(
            outline.items[2].prerequisites,
            vec![outline.items[1].id.clone()]
        );
        let ids: std::collections::HashSet<_> = outline.items.iter().map(|i| &i.id).collect();
        assert_eq!(ids.len(), 3, "every item gets a distinct id");
    }

    #[tokio::test]
    async fn propose_prerequisites_via_mock() {
        let ai = mock_ai(
            r#"[{"title":"Algebra basics","children":[]},
               {"title":"Derivatives","children":[
                   {"title":"Product rule","children":[]},
                   {"title":"Chain rule","children":[]}
               ]}]"#,
        );
        let forest = propose_prerequisites(&ai, "calculus", "Learn integration")
            .await
            .unwrap();
        assert_eq!(forest.len(), 2);
        assert_eq!(forest[0].title, "Algebra basics");
        assert!(forest[0].children.is_empty());
        assert_eq!(forest[1].title, "Derivatives");
        assert_eq!(forest[1].children.len(), 2);
        assert_eq!(forest[1].children[0].title, "Product rule");
    }

    /// §S15: an explicit empty array is a normal, common answer (most
    /// objectives need no prerequisites) — must not be treated as a parse
    /// failure.
    #[tokio::test]
    async fn propose_prerequisites_empty_tree_is_not_an_error() {
        let ai = mock_ai("[]");
        let forest = propose_prerequisites(&ai, "greetings", "Say hello in French")
            .await
            .unwrap();
        assert!(forest.is_empty());
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
        let a = grade(&ai, &rubric, "<form></form>", "my answer")
            .await
            .unwrap();
        assert!(a.all_demonstrated());
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
