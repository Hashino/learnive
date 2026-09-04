//! Move ABI (§6/§6.3) — S33 deterministic revision (PLAN.md; user decision,
//! 2026-09-03, after doc rmklzfy56r on a free model repeated explains, wrote
//! an unanswerable `ask`, and picked `revisit` with nothing to revisit).
//!
//! A **move** is the tutor's atomic unit of output: HTML + two invariant
//! flags — `interactive` (renders in the sandbox iframe, §4.4) and `graded`
//! (locked rubric + gate, §8). **Move selection is deterministic Rust**
//! ([`next_move`]): the model generates content, it never chooses what kind
//! of move comes next — the L1/L2 policy ladder, its menu, and the merged
//! decide+generate call are all gone. The model's move-type *choice* was the
//! quality failure; the tiering that used to ride on it (§12.1's fast/robust
//! split) survives unchanged through [`MoveType::tier`].
//!
//! Template per node (the old L0 rule, generalized):
//! - a learning node: `explain` → `test`
//! - a review node (`MoveContext::review_mode`): `revisit` → `test`
//! - a chapter-close node (S33-4, `MoveContext::chapter_close`):
//!   `explain` → `integrate` → `test`
//!
//! Rust-forced moves remain, never decided: `respond` answers a learner
//! question (§S6/§S17, `api::reading::ask_question`), `research` acquires
//! grounding (§S13) — the orchestration loop intercepts `research` when
//! grounding is empty, exactly one attempt per node, then loops back to the
//! template.
//!
//! **Streamed vs structured is a real invariant of the move ABI, not an
//! implementation detail** (§14's ~1s TTFT target). [`MoveType::render`]
//! partitions the types:
//!
//! - **Streamed** (`explain`, `integrate`, `revisit`, `respond`):
//!   prose-contract HTML, streamed token-by-token straight into the app
//!   origin. Flags are *fixed* by the type (`interactive: false, graded:
//!   false`), never model-chosen.
//! - **Structured** (`test`): exercise-contract HTML, one non-streamed call
//!   returning the JSON envelope (flags + rubric, locked together, §8).
//!   Nothing here can be shown half-built — a half-rendered `<form>` is
//!   useless and the rubric must be locked *whole* before submission — so
//!   paying the extra latency is the same trade the exercise+rubric call has
//!   always made.
//!
//! `generate_move` (structured) keeps one JSON contract and one bounded
//! repair-on-violation. The streamed path has no repair (once tokens are in
//! flight to the client, there is nothing to retry).
//!
//! `Other` exists only as a `#[serde(other)]` deserialization catch-all: the
//! event log is append-only source of truth (§4.3), and logs written before
//! S33 carry `ask`/`profile`/`confront`/`plan` moves. It is never generated,
//! never decided, never rendered.
//!
//! Wired into `api::generation::generate_node`: its template→generate loop
//! drives this module move by move.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::ai::{Ai, ChatMessage, ProviderError, Tier, TokenStream};
use crate::engine::{self, EngineError, Rubric, RubricObjective};

/// Named move type (§6 ABI, S33 revision).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveType {
    Explain,
    Test,
    Integrate,
    Revisit,
    /// Answers a question the learner asked mid-reading (§S6/§9/§S17).
    /// Forced by Rust only, exactly like `Research` — the deterministic
    /// template never picks it. §8.2's unification is of the generation
    /// PATH (grounding/citations/`MoveGenerated`), not of who decides to
    /// answer a question. Streamed (`render()`), Robust tier (genuine
    /// explanatory prose, §12.1). Used for both the inline-answer and the
    /// spawn-a-sub-node cases (`MoveContext::spawned_section_title`
    /// distinguishes them for `purpose()`); `api::reading::ask_question`
    /// decides which case via `engine::decide_ask_response`.
    Respond,
    /// Acquires grounding for a concept the current corpus has nothing on
    /// (§S13, `api::cold_start::acquire`) — forced by the orchestration
    /// loop (`api::generation::generate_node`) when grounding is empty and
    /// unattempted, never by the template; produces no learner-facing
    /// content of its own. The loop intercepts it before `render()` is
    /// ever consulted, runs acquisition, refreshes the context's grounding,
    /// and loops back to the template — `render()`/`generate_move*` must
    /// never actually be called with this type (debug-asserted the same way
    /// the streamed/structured split is).
    Research,
    /// Deserialization catch-all for the append-only event log (§4.3):
    /// logs written before S33 contain `ask`/`profile`/`confront`/`plan`
    /// moves that no longer exist as types. `#[serde(other)]` folds any
    /// unknown name here instead of failing the whole log read; `Other` is
    /// never generated, never decided, and `resumed_ungraded_moves` filters
    /// it out so an old partial node can't wedge the template.
    #[serde(other)]
    Other,
}

/// Which generation path a move type uses — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveRender {
    /// Prose contract, streamed; flags fixed at `interactive: false, graded: false`.
    Streamed,
    /// Exercise contract, one non-streamed JSON call; flags model-chosen
    /// (`test` still forces `graded: true` — never the model's discretion).
    Structured,
}

impl MoveType {
    /// Tier routing (§14): robust for explanatory prose, fast for the rest
    /// (exercises, short moves).
    pub fn tier(self) -> Tier {
        match self {
            MoveType::Explain | MoveType::Respond => Tier::Robust,
            _ => Tier::Fast,
        }
    }

    /// Streamed vs structured (§14) — see the module docs for the rationale.
    /// Never actually called for `Research` (see its own doc comment) — the
    /// orchestration loop intercepts it first; `Structured` here is just an
    /// arbitrary total-match default, not a real routing decision.
    pub fn render(self) -> MoveRender {
        match self {
            MoveType::Test | MoveType::Other | MoveType::Research => MoveRender::Structured,
            _ => MoveRender::Streamed,
        }
    }
}

impl std::fmt::Display for MoveType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            MoveType::Explain => "explain",
            MoveType::Test => "test",
            MoveType::Integrate => "integrate",
            MoveType::Revisit => "revisit",
            MoveType::Respond => "respond",
            MoveType::Research => "research",
            MoveType::Other => "other",
        };
        write!(f, "{s}")
    }
}

/// A move already emitted in the node — the template's decision input,
/// not the full HTML (§14 budget).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveRecord {
    pub move_type: MoveType,
    pub graded: bool,
}

/// Context handed to `next_move`/`generate_move*` (§14 context budget).
/// `objective` is the document's current objective text (§S4) — empty only
/// for a document with no confirmed objective yet. Every function degrades
/// gracefully on an empty field, the same way grounding already does in
/// `api::reading::grounding_for`.
#[derive(Debug, Clone, Default)]
pub struct MoveContext {
    pub topic: String,
    pub item_title: String,
    /// Titles of outline items already covered PLUS an excerpt of what they
    /// actually said (`api::reading::prior_content_context`) — narrative
    /// continuity AND the guard against a later node re-teaching what an
    /// earlier one already did.
    pub outline_context: String,
    pub prior_moves: Vec<MoveRecord>,
    pub objective: String,
    pub grounding: String,
    /// Verbatim tail of this node's content so far (§14 budget: ~1.5k chars),
    /// fed to the content prompts — the caller keeps this updated as moves are
    /// generated. Empty for the node's first move.
    pub node_tail: String,
    /// Set once a `research` move has run for this NODE (§S13) — withholds
    /// `research` from the menu on any further `decide_move` call, the cap
    /// on repeated acquisition attempts. Seeded from the event log by
    /// `prepare` (`events::aggregate::research_attempted`) on every
    /// per-move `/generate` request (§S18), not just set true in-process —
    /// each request gets a fresh `ctx`, so an in-process-only flag would
    /// only cap research within a single request, not across the several a
    /// node's generation now spans. Still also set `true` mid-loop the
    /// moment a request's OWN research attempt runs, exactly as before, so
    /// a second pick can't happen later in that same request either.
    pub research_attempted: bool,
    /// §S15: titles of this node's own children in the outline tree
    /// (`OutlineItem::parent_id` pointing back at it) — a prerequisite
    /// decomposed into sub-skills, or a question that spawned an
    /// elaboration. When non-empty, the `test` move is told to integrate
    /// what the children taught rather than probing each in isolation
    /// again — the structural answer to shallow mastery a light pass
    /// through prerequisites would otherwise risk.
    pub children_titles: Vec<String>,
    /// §S15 learn/review/skip toggle: true when this node's
    /// `OutlineItem::mode` is `Review` — the learner already believes they
    /// know this prerequisite. Every move prompt is told to keep it short
    /// (a definition-level refresher, a couple of exercises) instead of
    /// full first-time generation; the gate is still the same
    /// `Demonstrated` grade any node needs.
    pub review_mode: bool,
    /// §S15: the title of this node's PARENT in the prerequisite tree, when
    /// it has one — every prompt already gets `topic` (whole document) and
    /// `item_title` (this node's own concept), with nothing distinguishing
    /// them from a THIRD scope in between: the parent's own, broader concept.
    /// A prerequisite sub-node's prompt is told to stay out of it (§S15,
    /// `purpose`'s `scope_addendum`) — the parent gets its own node later.
    pub parent_title: Option<String>,
    /// Titles of every OTHER outline item with no node generated for it yet
    /// (`api::reading::prepare`'s `states` fold) — the counterpart to
    /// `outline_context`'s already-taught list, naming what's NOT taught yet
    /// so a move can be told to stay out of it. Added 2026-08-20 after live
    /// QA on an "Epistemologia" document: `topic_scope_note`'s "teach the
    /// node's own concept in full" and `scope_addendum`'s parent-only guard
    /// left a gap for SIBLING/later outline items with no parent-child
    /// relationship — a "belief" prerequisite node (objective: "distinguish
    /// knowledge from mere belief") taught the Gettier problem and the
    /// justified-true-belief analysis, which is the LATER "distinguishing
    /// knowledge from mere belief" node's own material, because nothing told
    /// it that node existed and was off-limits. `scope_addendum` only fires
    /// for `parent_title` (§S15 decomposed sub-nodes); this field covers the
    /// general case, any outline shape.
    pub later_titles: Vec<String>,
    /// UI-selected locale (`Locale::from_header`), request-scoped like every
    /// other `Locale` use in the app — not persisted per-document. Drives
    /// `locale::language_directive` on every move-generation prompt; see
    /// that function's doc comment for the live bug this closes. Defaults to
    /// `Locale::En`, so every `..Default::default()` test fixture keeps
    /// compiling unchanged.
    pub locale: crate::locale::Locale,
    /// §S17: the learner's question text, set only for a Rust-forced
    /// `Respond` move (`api::reading::ask_question`) — `None` for every
    /// other move type/caller. Mirrors what `engine::answer_question`'s
    /// `question` param used to carry directly.
    pub question: Option<String>,
    /// §S17: where in the document the question was asked from (selection
    /// or reading line), when there was one — mirrors `answer_question`'s
    /// `reading_context` param. `None` when the anchor named no block, same
    /// as before.
    pub reading_context: Option<String>,
    /// §S17: set only when a `Respond` move is spawning a new sub-node
    /// rather than answering inline (`engine::AskDecision::Spawn`) — the new
    /// section's own title, mirroring `generate_subnode_prose`'s `sub_title`
    /// param. `item_title`/`topic` stay the PARENT node's own scope in this
    /// case (unlike a §S15 prerequisite sub-node); only this field switches
    /// `purpose()`'s framing from "answer inline" to "write a self-contained
    /// elaboration titled X, spliced right after where it was asked".
    pub spawned_section_title: Option<String>,
    /// §8.2 remediation, forced in Rust the same way `question` is (never
    /// the template's choice): the exercise the learner just got wrong,
    /// paired with their answer — mirrors `engine::remediate`'s
    /// `exercise_html`/`answer` params, pre-formatted into one block by the
    /// caller. Set together with `unmet_objectives`/`remediation_attempt` or
    /// not at all.
    pub failed_attempt: Option<String>,
    /// §8.2: which rubric objectives the failed attempt didn't demonstrate,
    /// pre-formatted the same way `engine::remediate`'s `unmet` summary was.
    pub unmet_objectives: Option<String>,
    /// §8.2: how many remediation attempts on this same objective so far —
    /// scaffolding converges toward the worked example as this grows, then
    /// difficulty ramps back up. Mirrors `remediate`/`generate_remediation_
    /// exercise`'s `attempt` param.
    pub remediation_attempt: Option<u32>,
    /// S33-4: this node is the LAST node of a chapter that was actually
    /// decomposed into more than one atomic `Node` child (computed in
    /// `prepare` from the outline shape — never guessed) — the template
    /// inserts `integrate` between `explain` and `test` when true (§8's
    /// integration, scoped to the chapter the learner just finished).
    /// False for every other node shape, and false for a review node
    /// (`review_mode` wins: a review reactivates, it doesn't integrate).
    pub chapter_close: bool,
    /// §S23: the zero-cost scaffolding parameter
    /// (`events::aggregate::scaffolding_level`), reconstructed by `prepare`
    /// on every `/generate` call the same way `research_attempted` is —
    /// calibrates SUPPORT in the fade addendum (a worked example before
    /// the problem, or the problem direct), never difficulty.
    pub scaffolding: crate::events::aggregate::ScaffoldingLevel,
    /// §S23: titles of already-demonstrated prerequisites or graph-close
    /// siblings — fed to `Test`'s interleave addendum so the exercise can
    /// be told to mix them in, distinct from `children_titles`
    /// (integration: distant concepts COMBINE; interleaving: near
    /// concepts must be told APART). Empty when there is nothing nearby
    /// yet demonstrated to mix in.
    pub interleave_titles: Vec<String>,
    /// §S21 post-generation grounding-verification gate (`movement::
    /// grounding`): set ONLY on the single corrective-regeneration attempt
    /// after the gate's own structured check flagged claims in the FIRST
    /// attempt as unsupported by `grounding` above — names those claims so
    /// the retry can revise or drop them instead of blindly regenerating
    /// from scratch. `None` on every first-pass call and for every move
    /// type outside the gate's scope. Drives `prompt::
    /// grounding_correction_addendum`, applied in `purpose()` the same way
    /// `remediation_addendum`/`fade_addendum` are.
    pub grounding_correction: Option<Vec<String>>,
}

/// A generated move (§6 ABI): HTML + the two invariant flags + tactics.
#[derive(Debug, Clone)]
pub struct GeneratedMove {
    pub move_type: MoveType,
    pub interactive: bool,
    pub graded: bool,
    pub html: String,
    /// Tactic self-labels emitted in the same call, parsed defensively from
    /// a trailing sentinel — the §7 evidence table that consumed them is
    /// gone (S33), so nothing reads these any more; kept because the event
    /// log's `MoveGenerated` schema carries tactics and stripping must stay
    /// tolerant of a model emitting one anyway.
    pub tactics: Vec<String>,
    /// Present iff `graded` — locked together with the move (§8).
    pub rubric: Option<Rubric>,
    /// The model's own worked solution to the exact task in `html` (S16) —
    /// server-only, never sent to the client. Empty for a non-graded move
    /// (the schema doesn't ask for one) and for model output that predates
    /// this field; see `engine::ExerciseAndRubric::reference_solution` for
    /// why it exists and `engine::grade`'s degrade-when-empty behavior.
    pub reference_solution: String,
    /// Whether the first response failed validation and a repair round was
    /// needed. Only meaningful for `MoveRender::Structured` (the streamed
    /// path has no repair). Not yet logged as an event — S3's caller decides
    /// what to do with it (§9 ladder telemetry consumes this once wired).
    pub repaired: bool,
}

/// The deterministic move template (S33) — pure Rust, zero tokens, no round
/// trip. The generalization of the old L0 rule (`explain`, then `test`)
/// with the two shapes S33 adds: a review node reactivates before it checks,
/// and a chapter-close node integrates before it checks. Every node still
/// ends in a graded check (§6's invariant), every node still opens with
/// teaching (the old `enforce_teaching_before_test`'s invariant is now
/// structural, not a guard on a model choice).
///
/// `Err(NoNextMove)` means the node's template is exhausted — the caller's
/// cost guard (`api::generation::generate_node`) forces `test` at the last
/// slot before this can surface, and `resumed_ungraded_moves` filters old-log
/// move types the template doesn't recognize so a partial pre-S33 node can't
/// wedge here.
pub fn next_move(ctx: &MoveContext) -> Result<MoveType, EngineError> {
    match ctx.prior_moves.as_slice() {
        // A review node reactivates (one compact reactivation pass) before
        // checking; a fresh node teaches first.
        [] => Ok(if ctx.review_mode {
            MoveType::Revisit
        } else {
            MoveType::Explain
        }),
        // Either opener is followed by the check — unless this node closes a
        // chapter that was actually decomposed (S33-4), in which case the
        // integration move comes first.
        [
            MoveRecord {
                move_type: MoveType::Explain | MoveType::Revisit,
                ..
            },
        ] => Ok(if ctx.chapter_close {
            MoveType::Integrate
        } else {
            MoveType::Test
        }),
        // An integration move is itself followed by the check (§8: the
        // gate still closes the node).
        [
            _,
            MoveRecord {
                move_type: MoveType::Integrate,
                ..
            },
        ] => Ok(MoveType::Test),
        _ => Err(EngineError::NoNextMove),
    }
}

/// Starts a **streamed** move (`MoveRender::Streamed` types only — debug-
/// asserted). Returns the raw token stream; the caller pumps it into SSE
/// exactly like today's prose, accumulating the full text, then calls
/// [`finish_streamed_move`] once the stream ends.
pub async fn generate_move_stream(
    ai: &Ai,
    move_type: MoveType,
    ctx: &MoveContext,
) -> Result<TokenStream, ProviderError> {
    debug_assert_eq!(
        move_type.render(),
        MoveRender::Streamed,
        "generate_move_stream is only for MoveRender::Streamed types"
    );
    let messages = prompt::generate_move_streamed(move_type, ctx);
    ai.stream(move_type.tier(), messages).await
}

/// Strips the trailing tactics sentinel from a streamed move's accumulated
/// text and assembles the move. Flags are fixed by the type — a streamed
/// move has no JSON envelope to set them in.
pub fn finish_streamed_move(move_type: MoveType, accumulated: &str) -> GeneratedMove {
    debug_assert_eq!(
        move_type.render(),
        MoveRender::Streamed,
        "finish_streamed_move is only for MoveRender::Streamed types"
    );
    let (html, tactics) = parse::strip_tactics_sentinel(accumulated);
    GeneratedMove {
        move_type,
        interactive: false,
        graded: false,
        html,
        tactics,
        rubric: None,
        reference_solution: String::new(),
        repaired: false,
    }
}

/// Generates a **streamed-contract** move without actually streaming (§S17):
/// same prose contract [`generate_move_stream`]/[`finish_streamed_move`]
/// use, but collected as one call — for callers whose own response isn't
/// itself an SSE frame (`/ask`'s JSON reply, remediation's explanation: both
/// pre-existing non-streaming endpoints unified onto the move ABI in this
/// slice) and so have no stream to pump token-by-token. `engine::collect`
/// (== `ai.complete`) is the same non-streaming path every other non-SSE
/// prompt call in this codebase already uses — see its doc comment for why
/// `stream: false` is also the more reliable of the two against a
/// reasoning-heavy model, not just simpler here.
pub async fn generate_move_complete(
    ai: &Ai,
    move_type: MoveType,
    ctx: &MoveContext,
) -> Result<GeneratedMove, EngineError> {
    debug_assert_eq!(
        move_type.render(),
        MoveRender::Streamed,
        "generate_move_complete is only for MoveRender::Streamed types"
    );
    let messages = prompt::generate_move_streamed(move_type, ctx);
    let text = engine::collect(ai, move_type.tier(), messages).await?;
    Ok(finish_streamed_move(move_type, &text))
}

const ISLAND_OPEN: &str = "<figure data-interactive>";
const ISLAND_CLOSE: &str = "</figure>";

/// Gates a streamed move's raw token output so an interactive island's HTML
/// never reaches the client as an SSE `token` frame — only its empty
/// placeholder does; the real content stays in the frozen accumulator for
/// storage and is fetched later, out of band, into a sandboxed iframe
/// (§4.4, `api::block_frame`) — the same split `exercise_frame`/`get_node`
/// already use for the graded exercise, just at token-stream granularity
/// instead of read-time.
///
/// The model is contracted (`prompt::ISLAND_CONTRACT`) to open an island with
/// the exact literal `<figure data-interactive>` and close it with
/// `</figure>`, nothing else on that opening tag. Fixed markers turn
/// mid-stream detection into a substring search instead of a real streaming
/// HTML tokenizer; the cost is forbidding other attributes there, which is
/// fine — the server injects the block id itself once the close tag arrives
/// (so `ensure_block_ids`, §4.3, keeps it verbatim at `assemble_node` time
/// instead of minting a second one).
pub struct IslandGate {
    accumulated: String,
    pending: String,
    island: Option<String>,
}

impl IslandGate {
    pub fn new() -> Self {
        Self {
            accumulated: String::new(),
            pending: String::new(),
            island: None,
        }
    }

    /// Feeds one raw chunk from the provider. Returns the `token` frames (in
    /// order) safe to forward to the client — usually zero or one, but a
    /// chunk that both closes an island and carries trailing prose yields
    /// two.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.accumulated.push_str(chunk);

        if let Some(buf) = self.island.as_mut() {
            buf.push_str(chunk);
            let Some(pos) = buf.find(ISLAND_CLOSE) else {
                return Vec::new();
            };

            let block_id = format!("isl-{}", crate::engine::new_id());
            let raw = buf[..pos + ISLAND_CLOSE.len()].to_string();
            let leftover = buf[pos + ISLAND_CLOSE.len()..].to_string();
            let tagged = raw.replacen(
                ISLAND_OPEN,
                &format!(r#"<figure data-interactive data-block-id="{block_id}">"#),
                1,
            );
            let placeholder =
                format!(r#"<figure data-interactive data-block-id="{block_id}"></figure>"#);

            let buf_len = buf.len();
            self.accumulated.truncate(self.accumulated.len() - buf_len);
            self.accumulated.push_str(&tagged);
            self.accumulated.push_str(&leftover);

            self.island = None;
            self.pending = leftover;
            return vec![placeholder];
        }

        self.pending.push_str(chunk);
        if let Some(pos) = self.pending.find(ISLAND_OPEN) {
            let before = self.pending[..pos].to_string();
            self.island = Some(self.pending[pos..].to_string());
            self.pending.clear();
            return if before.is_empty() {
                Vec::new()
            } else {
                vec![before]
            };
        }

        // No marker yet: hold back only enough trailing bytes to still
        // catch one split across the next chunk boundary, flush the rest.
        let hold = ISLAND_OPEN.len() - 1;
        if self.pending.len() > hold {
            let split_at = floor_char_boundary(&self.pending, self.pending.len() - hold);
            let safe = self.pending[..split_at].to_string();
            self.pending = self.pending[split_at..].to_string();
            if !safe.is_empty() {
                return vec![safe];
            }
        }
        Vec::new()
    }

    /// Stream ended: returns the frozen accumulator (full raw text, for
    /// storage) and any trailing text still safely flushable as a `token`
    /// frame. An island that never closed (provider stopped mid-stream) is
    /// dropped from the trailing text, not flushed — its raw HTML must not
    /// leak into a live SSE frame (the whole point of this gate) even on the
    /// unhappy path. It is NOT lost: `accumulated` already has it (every
    /// chunk was appended there unconditionally), so `redact_interactive_blocks`
    /// handles it correctly the next time this node is read from storage.
    pub fn finish(mut self) -> (String, Option<String>) {
        self.island = None;
        let trailing = if self.pending.is_empty() {
            None
        } else {
            Some(self.pending)
        };
        (self.accumulated, trailing)
    }
}

impl Default for IslandGate {
    fn default() -> Self {
        Self::new()
    }
}

fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Generates a **structured** move's content (`MoveRender::Structured` types
/// only — debug-asserted), tier-routed by `MoveType::tier` (§14), with one
/// bounded repair attempt on a schema violation (logged as `SchemaViolation`
/// by the caller; §9).
pub async fn generate_move(
    ai: &Ai,
    move_type: MoveType,
    ctx: &MoveContext,
) -> Result<GeneratedMove, EngineError> {
    debug_assert_eq!(
        move_type.render(),
        MoveRender::Structured,
        "generate_move is only for MoveRender::Structured types — use generate_move_stream"
    );
    let tier = move_type.tier();
    let messages = prompt::generate_move(move_type, ctx);
    let text = engine::collect(ai, tier, messages.clone()).await?;
    if let Ok(mv) = parse::generated_move(move_type, &text) {
        return Ok(mv);
    }
    let repair = repair_messages(messages, &text, "the Move JSON contract was violated");
    let text = engine::collect(ai, tier, repair).await?;
    let mut mv = parse::generated_move(move_type, &text)?;
    mv.repaired = true;
    Ok(mv)
}

/// Appends the bad response + a correction request (one bounded repair
/// round, §14 strict validation with repair).
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

// ---------------------------------------------------------------------------
// Prompts (§6, §14). In English — the app content language.
// ---------------------------------------------------------------------------

pub mod prompt;

pub mod parse;

/// §S21 post-generation grounding-verification gate — see the module's own
/// doc comment for the full design (why it's a separate structured call
/// from the move's own generation, the escalation shape, why a failed
/// verifier degrades to a visible warning instead of erroring the request).
pub mod grounding;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{ChatRequest, MockProvider, Models, Provider};
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn mock_ai(reply: &str) -> Ai {
        Ai::new(
            Provider::Mock(MockProvider::new(reply)),
            Models::single("mock"),
        )
    }

    fn scripted_ai<F>(f: F) -> Ai
    where
        F: Fn(&ChatRequest) -> String + Send + Sync + 'static,
    {
        Ai::new(
            Provider::Mock(MockProvider::scripted(f)),
            Models::single("mock"),
        )
    }

    fn full_text(req: &ChatRequest) -> String {
        req.messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn collect_stream(ai: &Ai, move_type: MoveType, ctx: &MoveContext) -> String {
        let stream = generate_move_stream(ai, move_type, ctx).await.unwrap();
        stream
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat()
    }

    /// Feeds an `IslandGate` one token at a time and returns (all emitted
    /// frames concatenated, the frozen accumulator).
    fn run_gate(chunks: &[&str]) -> (String, String) {
        let mut gate = IslandGate::new();
        let mut emitted = String::new();
        for c in chunks {
            for frame in gate.push(c) {
                emitted.push_str(&frame);
            }
        }
        let (accumulated, trailing) = gate.finish();
        if let Some(t) = trailing {
            emitted.push_str(&t);
        }
        (emitted, accumulated)
    }

    #[test]
    fn island_gate_passes_plain_prose_through_untouched() {
        let (emitted, accumulated) = run_gate(&["Hello ", "world", ", no islands here."]);
        assert_eq!(emitted, "Hello world, no islands here.");
        assert_eq!(accumulated, emitted);
    }

    #[test]
    fn island_gate_never_emits_the_islands_raw_script() {
        let (emitted, accumulated) = run_gate(&[
            "Before. ",
            "<figure data-interactive><script>alert(document.cookie)</script></figure>",
            " After.",
        ]);
        assert!(!emitted.contains("<script"));
        assert!(!emitted.contains("alert"));
        assert!(emitted.contains("Before."));
        assert!(emitted.contains("After."));
        assert!(emitted.contains("data-block-id=\"isl-"));

        // The frozen copy (what gets stored) keeps the raw content, with the
        // same id the client's placeholder was given.
        assert!(accumulated.contains("<script>alert(document.cookie)</script>"));
        let id_start = accumulated.find("data-block-id=\"isl-").unwrap();
        let id = &accumulated[id_start..id_start + 30];
        assert!(emitted.contains(id));
    }

    #[test]
    fn island_gate_reassembles_a_marker_split_across_many_tiny_chunks() {
        let full = "<figure data-interactive><b>x</b></figure>";
        let chunks: Vec<&str> = full
            .char_indices()
            .map(|(i, c)| &full[i..i + c.len_utf8()])
            .collect();
        let (emitted, accumulated) = run_gate(&chunks);
        assert!(!emitted.contains("<b>"));
        assert!(accumulated.contains("<b>x</b>"));
        assert!(emitted.contains("data-block-id=\"isl-"));
    }

    #[test]
    fn island_gate_never_flushes_an_unterminated_island_but_keeps_it_for_storage() {
        // Defensive case: the provider stops mid-island (truncated response).
        // The raw fragment must not leak into a live `token` frame, but it
        // must not be lost from storage either.
        let (emitted, accumulated) = run_gate(&["prefix ", "<figure data-interactive><script>x"]);
        assert_eq!(emitted, "prefix ");
        assert!(!emitted.contains("<script"));
        assert!(accumulated.contains("<script>x"));
    }

    #[test]
    fn island_gate_handles_two_islands_in_one_move() {
        // Pins the `accumulated.truncate(accumulated.len() - buf_len)`
        // arithmetic in `push`: a second island later in the same move must
        // not corrupt or duplicate anything the first island's close already
        // committed to `accumulated`.
        let (emitted, accumulated) = run_gate(&[
            "Intro. ",
            "<figure data-interactive>",
            "<script>one</script>",
            "</figure>",
            " Middle text. ",
            "<figure data-interactive>",
            "<script>two</script>",
            "</figure>",
            " Outro.",
        ]);

        assert!(!emitted.contains("<script"));
        assert!(emitted.contains("Intro."));
        assert!(emitted.contains("Middle text."));
        assert!(emitted.contains("Outro."));
        assert_eq!(emitted.matches("data-block-id=\"isl-").count(), 2);

        assert!(accumulated.contains("<script>one</script>"));
        assert!(accumulated.contains("<script>two</script>"));
        fn extract_id(s: &str, from: usize) -> (String, usize) {
            let marker = "data-block-id=\"";
            let start = from + s[from..].find(marker).unwrap() + marker.len();
            let end = start + s[start..].find('"').unwrap();
            (s[start..end].to_string(), end)
        }
        let (first_id, after_first) = extract_id(&accumulated, 0);
        let (second_id, _) = extract_id(&accumulated, after_first);
        assert_ne!(first_id, second_id);

        // Ordering must be preserved: nothing from the second island bled
        // into the first, nor vice versa.
        let intro = accumulated.find("Intro.").unwrap();
        let one = accumulated.find("<script>one").unwrap();
        let middle = accumulated.find("Middle text.").unwrap();
        let two = accumulated.find("<script>two").unwrap();
        let outro = accumulated.find("Outro.").unwrap();
        assert!(intro < one && one < middle && middle < two && two < outro);
    }

    #[test]
    fn tier_routing_matches_plan() {
        assert_eq!(MoveType::Explain.tier(), Tier::Robust);
        assert_eq!(MoveType::Respond.tier(), Tier::Robust);
        for mt in [
            MoveType::Test,
            MoveType::Integrate,
            MoveType::Revisit,
            MoveType::Other,
        ] {
            assert_eq!(mt.tier(), Tier::Fast);
        }
    }

    #[test]
    fn render_partitions_streamed_vs_structured() {
        for mt in [
            MoveType::Explain,
            MoveType::Integrate,
            MoveType::Revisit,
            MoveType::Respond,
        ] {
            assert_eq!(mt.render(), MoveRender::Streamed);
        }
        for mt in [MoveType::Test, MoveType::Research, MoveType::Other] {
            assert_eq!(mt.render(), MoveRender::Structured);
        }
    }

    /// S33: deleted move types deserialize from old event logs as `Other`
    /// instead of failing the read — the append-only log is source of truth
    /// (§4.3), and `#[serde(other)]` is what keeps pre-S33 documents
    /// readable.
    #[test]
    fn pre_s33_move_names_deserialize_as_other() {
        for name in ["ask", "profile", "confront", "plan"] {
            let mt: MoveType =
                serde_json::from_str(&format!("\"{name}\"")).expect("must deserialize");
            assert_eq!(mt, MoveType::Other, "{name} must fold into Other");
        }
    }

    #[test]
    fn template_opens_explain_then_test() {
        let mut ctx = MoveContext::default();
        assert_eq!(next_move(&ctx).unwrap(), MoveType::Explain);

        ctx.prior_moves.push(MoveRecord {
            move_type: MoveType::Explain,
            graded: false,
        });
        assert_eq!(next_move(&ctx).unwrap(), MoveType::Test);

        ctx.prior_moves.push(MoveRecord {
            move_type: MoveType::Test,
            graded: true,
        });
        assert!(matches!(
            next_move(&ctx).unwrap_err(),
            EngineError::NoNextMove
        ));
    }

    #[test]
    fn review_node_reactivates_before_it_checks() {
        let mut ctx = MoveContext {
            review_mode: true,
            ..Default::default()
        };
        assert_eq!(next_move(&ctx).unwrap(), MoveType::Revisit);

        ctx.prior_moves.push(MoveRecord {
            move_type: MoveType::Revisit,
            graded: false,
        });
        assert_eq!(next_move(&ctx).unwrap(), MoveType::Test);
    }

    #[test]
    fn chapter_close_inserts_integrate_between_explain_and_test() {
        let mut ctx = MoveContext {
            chapter_close: true,
            ..Default::default()
        };
        assert_eq!(next_move(&ctx).unwrap(), MoveType::Explain);

        ctx.prior_moves.push(MoveRecord {
            move_type: MoveType::Explain,
            graded: false,
        });
        assert_eq!(next_move(&ctx).unwrap(), MoveType::Integrate);

        ctx.prior_moves.push(MoveRecord {
            move_type: MoveType::Integrate,
            graded: false,
        });
        assert_eq!(next_move(&ctx).unwrap(), MoveType::Test);
    }

    #[test]
    fn explain_prompt_carries_prose_contract_not_exercise() {
        let ctx = MoveContext::default();
        let sys = &prompt::generate_move_streamed(MoveType::Explain, &ctx)[0].content;
        assert!(sys.contains("NEVER use"));
        assert!(!sys.contains("postMessage"));
        // S33: the tactics sentinel contract is gone with the §7 evidence
        // table — the streamed prompt must not ask for self-labels anymore.
        assert!(!sys.contains("tactics:"));
        // §S11: the streamed path is the only one with a real IslandGate
        // behind it, so it's the only one told about the island contract.
        assert!(sys.contains("figure data-interactive"));
    }

    #[test]
    fn test_prompt_carries_exercise_contract_and_forces_graded() {
        let ctx = MoveContext::default();
        let sys = &prompt::generate_move(MoveType::Test, &ctx)[0].content;
        assert!(sys.contains("postMessage"));
        assert!(sys.contains("sandbox"));
        assert!(sys.contains("MUST be graded"));
    }

    /// S16: a `test` move's `reference_solution` round-trips off the wire
    /// into `GeneratedMove`, and is empty (not an error) when the model
    /// omits it — the same degrade-gracefully contract as `rubric`/
    /// `objectives` already have.
    #[test]
    fn generated_move_parses_reference_solution() {
        let with_solution = parse::generated_move(
            MoveType::Test,
            r#"{"html":"<form></form>","graded":true,"reference_solution":"x=42",
               "objectives":[{"id":"o1","kind":"application","description":"d","criteria":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(with_solution.reference_solution, "x=42");

        let without_solution = parse::generated_move(
            MoveType::Test,
            r#"{"html":"<form></form>","graded":true,
               "objectives":[{"id":"o1","kind":"application","description":"d","criteria":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(without_solution.reference_solution, "");
    }

    #[test]
    fn structured_prose_prompt_omits_the_island_contract() {
        // §S11 follow-up: a structured PROSE move (`other`, the deserialization
        // catch-all) shares `PROSE_HTML_CONTRACT` with the streamed path but
        // has no `IslandGate` behind it, and asking a JSON-envelope call to
        // emit raw island HTML/JS inside a string field risks breaking the
        // envelope itself. The island paragraph must stay out of its prompt.
        let ctx = MoveContext::default();
        let sys = &prompt::generate_move(MoveType::Other, &ctx)[0].content;
        assert!(sys.contains("NEVER use"));
        assert!(!sys.contains("figure data-interactive"));
    }

    #[test]
    fn strips_tactics_sentinel_from_streamed_output() {
        let (html, tactics) = parse::strip_tactics_sentinel(
            "<h2>Limits</h2><p>Explanation.</p>\n<!--tactics: analogy, worked-example-->",
        );
        assert_eq!(html, "<h2>Limits</h2><p>Explanation.</p>");
        assert_eq!(
            tactics,
            vec!["analogy".to_string(), "worked-example".to_string()]
        );
    }

    #[test]
    fn missing_sentinel_yields_no_tactics_not_an_error() {
        let (html, tactics) = parse::strip_tactics_sentinel("<p>plain content, no sentinel</p>");
        assert_eq!(html, "<p>plain content, no sentinel</p>");
        assert!(tactics.is_empty());
    }

    #[tokio::test]
    async fn streamed_move_yields_ungraded_noninteractive_move_with_tactics() {
        let ai = mock_ai("<h2>Limits</h2><p>Explanation.</p> <!--tactics: analogy-->");
        let ctx = MoveContext::default();
        let accumulated = collect_stream(&ai, MoveType::Explain, &ctx).await;
        let mv = finish_streamed_move(MoveType::Explain, &accumulated);

        assert!(!mv.graded);
        assert!(!mv.interactive);
        assert!(mv.rubric.is_none());
        assert_eq!(mv.tactics, vec!["analogy".to_string()]);
        assert!(
            !mv.html.contains("tactics:"),
            "sentinel must not leak into stored html"
        );
    }

    #[tokio::test]
    async fn generate_move_test_forces_graded_even_if_model_said_false() {
        let ai = mock_ai(
            r#"{"html":"<form><input name=\"a\"></form>","graded":false,"objectives":[{"id":"o1","kind":"application","description":"apply","criteria":"transfers","transfer":true}]}"#,
        );
        let mv = generate_move(&ai, MoveType::Test, &MoveContext::default())
            .await
            .unwrap();
        assert!(mv.graded, "test is intrinsically graded (§8)");
        let rubric = mv.rubric.unwrap();
        assert_eq!(rubric.objectives.len(), 1);
        assert!(rubric.objectives[0].transfer);
    }

    #[tokio::test]
    async fn generate_move_rejects_graded_with_no_objectives() {
        let ai = mock_ai(r#"{"html":"<form></form>","graded":true,"objectives":[]}"#);
        // First attempt fails validation; repair attempt gets the same broken
        // reply (MockProvider::new is constant), so the whole call errors —
        // proving the invariant is actually enforced, not just parsed.
        let err = generate_move(&ai, MoveType::Test, &MoveContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::Parse(_)));
    }

    #[tokio::test]
    async fn generate_move_repairs_once_on_malformed_json() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        let ai = scripted_ai(move |_req| {
            let n = calls_inner.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                "not json".to_string()
            } else {
                r#"{"html":"<form><input name=\"a\"></form>","graded":true,"objectives":[{"id":"o1","kind":"knowledge","description":"d","criteria":"c","transfer":false}]}"#.to_string()
            }
        });
        let mv = generate_move(&ai, MoveType::Test, &MoveContext::default())
            .await
            .unwrap();
        assert!(mv.repaired);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// The real check for the template (PLAN.md): assemble a node from the
    /// deterministic Explain+Test sequence through the SAME
    /// `engine::assemble_node` the live endpoint uses, and assert the
    /// structural shape `engine::tests::assemble_node_wraps_prose_and_
    /// exercise` pins — not just that the template agrees with itself.
    #[tokio::test]
    async fn template_move_pipeline_matches_todays_node_shape() {
        let explicar_ai = mock_ai("<h2>Limits</h2><p>Explanation.</p> <!--tactics: analogy-->");
        let testar_ai = mock_ai(
            r#"{"html":"<form><input name=\"a\"></form>","interactive":false,"graded":true,"tactics":["worked-example"],"objectives":[{"id":"o1","kind":"application","description":"apply","criteria":"transfers","transfer":true}]}"#,
        );

        let mut ctx = MoveContext {
            topic: "calculus".into(),
            item_title: "Limits".into(),
            ..Default::default()
        };

        let mt1 = next_move(&ctx).unwrap();
        assert_eq!(mt1, MoveType::Explain);
        let accumulated = collect_stream(&explicar_ai, mt1, &ctx).await;
        let mv1 = finish_streamed_move(mt1, &accumulated);
        assert!(!mv1.graded);

        ctx.prior_moves.push(MoveRecord {
            move_type: mv1.move_type,
            graded: mv1.graded,
        });
        let mt2 = next_move(&ctx).unwrap();
        assert_eq!(mt2, MoveType::Test);
        let mv2 = generate_move(&testar_ai, mt2, &ctx).await.unwrap();
        assert!(mv2.graded);
        let rubric = mv2.rubric.clone().unwrap();
        assert!(!rubric.objectives.is_empty());

        // One-shot assembly from raw moves — `api::generation::finalize` now
        // goes through `engine::finalize_node` against already-tagged,
        // progressively-persisted content (§S6 follow-up), but this is
        // still the reference shape both produce: prose blocks + a tagged
        // exercise form in one node.
        let node =
            engine::assemble_node("d1", "n1", &mv1.html, &mv2.html, "n1-ex", "n1-ru").unwrap();
        assert!(!node.content.blocks.is_empty());
        let exercise = node.content.exercise.unwrap();
        assert_eq!(exercise.exercise_id, "n1-ex");
        assert_eq!(exercise.rubric_id.as_deref(), Some("n1-ru"));

        ctx.prior_moves.push(MoveRecord {
            move_type: mv2.move_type,
            graded: mv2.graded,
        });
        let done = next_move(&ctx).unwrap_err();
        assert!(matches!(done, EngineError::NoNextMove));
    }

    /// Manual validation for the `EXERCISE_HTML_CONTRACT` self-consistency
    /// clause (engine/prompt.rs): generates a `Test` move for 10 distinct
    /// topics against the REAL configured provider (`.env`) and prints
    /// exercise_html + rubric for each so a human (or another LLM pass) can
    /// judge whether the rubric's criteria actually match what the exercise
    /// asks. Not part of the normal suite — no oracle to assert against, and
    /// it spends real API budget. Run with:
    /// `cargo test -p learnive --lib movement::tests::live_exercise_rubric_consistency_check -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "hits the real configured AI provider; run manually, see doc comment"]
    async fn live_exercise_rubric_consistency_check() {
        // `cargo test` sets cwd to this crate's manifest dir, not the
        // workspace root where `.env` actually lives (unlike `cargo run`,
        // invoked from the root) — resolve it explicitly.
        let env_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env");
        crate::load_dotenv(env_path);
        let data_dir =
            std::env::temp_dir().join(format!("learnive-live-check-{}", std::process::id()));
        let config = crate::config::AppConfig::load(&data_dir);
        let secret = crate::secret::SecretStore::open(&data_dir);
        let ai = crate::api::build_ai(&config, &secret);

        let topics: [(&str, &str); 10] = [
            ("Photosynthesis", "the light-dependent reactions"),
            ("French Revolution", "the causes of the 1789 uprising"),
            ("Python recursion", "writing a recursive factorial function"),
            ("Thermodynamics", "the first law of thermodynamics"),
            ("Linear algebra", "computing a matrix determinant"),
            ("Cellular respiration", "the stages of glycolysis"),
            (
                "Probability",
                "expected value of a discrete random variable",
            ),
            ("Roman history", "the causes of the fall of the Republic"),
            ("SQL", "writing an INNER JOIN across two tables"),
            (
                "Macroeconomics",
                "the effect of a tariff on consumer surplus",
            ),
        ];

        for (topic, item_title) in topics {
            let ctx = MoveContext {
                topic: topic.to_string(),
                item_title: item_title.to_string(),
                objective: format!("Demonstrate understanding of {item_title}."),
                ..Default::default()
            };
            println!("\n=== TOPIC: {topic} ===");
            match generate_move(&ai, MoveType::Test, &ctx).await {
                Ok(mv) => {
                    println!("--- exercise_html ---\n{}", mv.html);
                    println!(
                        "--- rubric ---\n{}",
                        serde_json::to_string_pretty(&mv.rubric).unwrap()
                    );
                }
                Err(e) => println!("--- ERROR: {e} ---"),
            }
        }
    }

    /// Manual quality probe for grounded `Explain` generation against the
    /// REAL configured provider (`.env`) — built to test whether a
    /// smaller/faster model (e.g. `openai/gpt-oss-120b`) can produce good
    /// grounded prose now that generation is meant to be heavily grounded
    /// on real source text (PLAN.md's S21/S27), without waiting for the
    /// pivot's ingestion pipeline (PDF extraction, the acervo gate) to
    /// exist. Feeds a real excerpt as `ctx.grounding` — the exact field
    /// `movement::prompt`'s `CITE_CONTRACT` keys off to require inline
    /// `<cite>` tags — and prints the resulting HTML for a human to judge:
    /// quality of the rewrite, whether citations look sane, whether
    /// anything reads as unsupported by the excerpt. No oracle to assert
    /// against — this is eyeball QA, not a regression test. This test
    /// never fabricates or embeds source text itself; point it at a file
    /// with a real excerpt you have the right to use.
    ///
    /// Set `LEARNIVE_MODEL_ROBUST` in `.env` to the model under test before
    /// running (`Explain` is a robust-tier move). Run:
    /// `LEARNIVE_TEST_GROUNDING_FILE=/path/to/excerpt.txt cargo test -p learnive --lib movement::tests::live_grounded_explain_quality_check -- --ignored --nocapture`
    /// Optional: `LEARNIVE_TEST_TOPIC`/`LEARNIVE_TEST_TITLE` override the
    /// node framing (sensible defaults otherwise).
    #[tokio::test]
    #[ignore = "hits the real configured AI provider; run manually, see doc comment"]
    async fn live_grounded_explain_quality_check() {
        let env_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env");
        crate::load_dotenv(env_path);

        let grounding_path = std::env::var("LEARNIVE_TEST_GROUNDING_FILE").expect(
            "set LEARNIVE_TEST_GROUNDING_FILE to a text file containing a real book/article \
             excerpt — this test never fabricates source text itself",
        );
        let grounding = std::fs::read_to_string(&grounding_path)
            .unwrap_or_else(|e| panic!("failed to read {grounding_path}: {e}"));
        assert!(
            !grounding.trim().is_empty(),
            "grounding file {grounding_path} is empty"
        );

        let topic = std::env::var("LEARNIVE_TEST_TOPIC").unwrap_or_else(|_| "Test topic".into());
        let item_title =
            std::env::var("LEARNIVE_TEST_TITLE").unwrap_or_else(|_| "Test node".into());

        let data_dir =
            std::env::temp_dir().join(format!("learnive-live-grounding-{}", std::process::id()));
        let config = crate::config::AppConfig::load(&data_dir);
        let secret = crate::secret::SecretStore::open(&data_dir);
        let ai = crate::api::build_ai(&config, &secret);

        let ctx = MoveContext {
            topic: topic.clone(),
            item_title: item_title.clone(),
            objective: format!("Demonstrate understanding of {item_title}."),
            grounding: grounding.clone(),
            ..Default::default()
        };

        println!(
            "=== grounding excerpt ({} chars) from {grounding_path} ===\n{grounding}\n",
            grounding.len()
        );
        let t0 = std::time::Instant::now();
        match generate_move_complete(&ai, MoveType::Explain, &ctx).await {
            Ok(mv) => {
                println!("=== generated in {:?} ===", t0.elapsed());
                println!("--- html ---\n{}", mv.html);
                let cite_count = mv.html.matches("<cite").count();
                println!("--- {cite_count} <cite> tag(s) found ---");
            }
            Err(e) => println!("--- ERROR after {:?}: {e} ---", t0.elapsed()),
        }
    }

    /// Dumps REAL `Test` move request bodies (exercise+rubric contract) for
    /// 5 distinct topics, one JSON file each — no network, no `reasoning`
    /// field (added externally per condition for the reasoning-effort A/B
    /// comparison). See doc comment on `dump_real_request_bodies`.
    #[test]
    #[ignore = "writes to /tmp for a manual reasoning-effort comparison"]
    fn dump_test_move_requests() {
        crate::load_dotenv(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env"));
        let fast = std::env::var("LEARNIVE_MODEL_FAST").unwrap_or_default();

        #[derive(serde::Serialize)]
        struct Body<'a> {
            model: &'a str,
            messages: &'a [crate::ai::ChatMessage],
            stream: bool,
        }

        let topics: [(&str, &str); 5] = [
            ("Photosynthesis", "the light-dependent reactions"),
            ("Linear algebra", "computing a matrix determinant"),
            ("Cellular respiration", "the stages of glycolysis"),
            ("Thermodynamics", "the first law of thermodynamics"),
            (
                "Probability",
                "expected value of a discrete random variable",
            ),
        ];

        for (i, (topic, item_title)) in topics.iter().enumerate() {
            let ctx = MoveContext {
                topic: topic.to_string(),
                item_title: item_title.to_string(),
                objective: format!("Demonstrate understanding of {item_title}."),
                ..Default::default()
            };
            let messages = prompt::generate_move(MoveType::Test, &ctx);
            std::fs::write(
                format!(
                    "/tmp/claude-1000/-home-hashino-Projects-learnive/47a18ee9-2d2e-4f3d-83f6-0fb5cc70e776/scratchpad/req_test_{i}.json"
                ),
                serde_json::to_string(&Body {
                    model: &fast,
                    messages: &messages,
                    stream: true,
                })
                .unwrap(),
            )
            .unwrap();
        }
        println!("wrote 5 req_test_N.json files");
    }

    /// Same as `dump_test_move_requests` but for the `Explain` move (prose
    /// contract, robust tier) — for the reasoning-effort A/B on streamed
    /// content, which is the actually-blocking wait (unlike `Test`, which
    /// generates while the learner is still reading the settled prose).
    #[test]
    #[ignore = "writes to /tmp for a manual reasoning-effort comparison"]
    fn dump_explain_move_requests() {
        crate::load_dotenv(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env"));
        let robust = std::env::var("LEARNIVE_MODEL_ROBUST").unwrap_or_default();

        #[derive(serde::Serialize)]
        struct Body<'a> {
            model: &'a str,
            messages: &'a [crate::ai::ChatMessage],
            stream: bool,
        }

        let topics: [(&str, &str); 5] = [
            ("Photosynthesis", "the light-dependent reactions"),
            ("Linear algebra", "computing a matrix determinant"),
            ("Cellular respiration", "the stages of glycolysis"),
            ("Thermodynamics", "the first law of thermodynamics"),
            (
                "Probability",
                "expected value of a discrete random variable",
            ),
        ];

        for (i, (topic, item_title)) in topics.iter().enumerate() {
            let ctx = MoveContext {
                topic: topic.to_string(),
                item_title: item_title.to_string(),
                objective: format!("Demonstrate understanding of {item_title}."),
                ..Default::default()
            };
            let messages = prompt::generate_move_streamed(MoveType::Explain, &ctx);
            std::fs::write(
                format!(
                    "/tmp/claude-1000/-home-hashino-Projects-learnive/47a18ee9-2d2e-4f3d-83f6-0fb5cc70e776/scratchpad/req_explain_{i}.json"
                ),
                serde_json::to_string(&Body {
                    model: &robust,
                    messages: &messages,
                    stream: true,
                })
                .unwrap(),
            )
            .unwrap();
        }
        println!("wrote 5 req_explain_N.json files");
    }
}
