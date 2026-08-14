//! Move ABI (§6) and the policy ladder — S2 of the agentic-loop build (see
//! PLAN.md's "core evolution: agentic loop" section).
//!
//! A **move** is the tutor's atomic unit of output: HTML + two invariant
//! flags — `interactive` (renders in the sandbox iframe, §4.4) and `graded`
//! (locked rubric + gate, §8) — plus a named, extensible [`MoveType`] and the
//! tactic self-labels the profile (§7) later joins on. `decide_move` picks
//! the next move type; `generate_move*` produces its content. Both take an
//! [`AgentPolicy`] rung:
//!
//! - **L0** (scripted): `decide_move` is a pure Rust function reproducing
//!   today's prose→exercise sequence exactly — no AI call, no ambiguity.
//! - **L1** (guided) / **L2** (open): `decide_move` is an AI call from a
//!   closed menu (L1) or an open one that allows `other` (L2).
//!
//! **Streamed vs structured is a real invariant of the move ABI, not an
//! implementation detail** (revises the first cut of this module, which used
//! one non-streamed JSON call for every move type — that contradicted §14's
//! ~1s TTFT target). [`MoveType::render`] partitions the nine types:
//!
//! - **Streamed** (`explain`, `ask`, `confront`, `integrate`, `revisit`):
//!   prose-contract HTML, streamed token-by-token straight into the app
//!   origin exactly like today's prose does — a JSON envelope would defeat
//!   that. Their flags are therefore *fixed* by the type (`interactive:
//!   false, graded: false`), never model-chosen, and tactics ride along as a
//!   trailing `<!--tactics: a, b-->` HTML comment the model appends after its
//!   content — invisible when rendered (comments never display, streamed or
//!   not) and stripped server-side, from the *stored* HTML only, once the
//!   stream ends.
//! - **Structured** (`test`, `profile`, `plan`, `other`): exercise-contract
//!   HTML, one non-streamed call returning the JSON envelope (flags +
//!   tactics +, if graded, the rubric). Nothing here can be shown
//!   half-built — a half-rendered `<form>`/sandboxed widget is useless, the
//!   rubric must be locked *whole* before submission (§8), and `plan`'s
//!   proposed outline is structured data with nowhere to live in a prose
//!   stream (§S4: it moved here from the streamed set for exactly this
//!   reason — the model, not Rust, computes the diff, via the envelope's
//!   optional `outline` field) — so paying the extra latency is the same
//!   trade today's exercise+rubric call already makes.
//!
//! `generate_move` (structured) shares one JSON contract, parse/validate, and
//! one bounded repair-on-violation across L0/L1/L2 — templates constrain a
//! move's shape, never the prose's voice (PLAN.md). The streamed path has no
//! repair (same as today's prose: once tokens are in flight to the client,
//! there is nothing to retry).
//!
//! Wired into `api.rs::generate_node` (S3): its decide→generate loop drives
//! this module move by move. `GeneratedMove::move_type`/`interactive` are
//! part of the stable contract but unread by S3 (the caller already has the
//! decided type, and no render path yet distinguishes `interactive` — see
//! `generate_node`'s doc comment); hence the `allow` stays until a later
//! slice reads them.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::ai::{Ai, ChatMessage, ProviderError, Tier, TokenStream};
use crate::engine::{self, EngineError, Rubric, RubricObjective};

/// Named, extensible move type (§6 ABI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveType {
    Explain,
    Ask,
    Test,
    Profile,
    Confront,
    Integrate,
    Revisit,
    Plan,
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
    /// Tier routing (§14): robust for explanatory prose/confrontation, fast
    /// for the rest (exercises, questions, short moves).
    pub fn tier(self) -> Tier {
        match self {
            MoveType::Explain | MoveType::Confront => Tier::Robust,
            _ => Tier::Fast,
        }
    }

    /// Streamed vs structured (§14) — see the module docs for the rationale.
    pub fn render(self) -> MoveRender {
        match self {
            MoveType::Test | MoveType::Profile | MoveType::Plan | MoveType::Other => {
                MoveRender::Structured
            }
            _ => MoveRender::Streamed,
        }
    }
}

impl std::fmt::Display for MoveType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            MoveType::Explain => "explain",
            MoveType::Ask => "ask",
            MoveType::Test => "test",
            MoveType::Profile => "profile",
            MoveType::Confront => "confront",
            MoveType::Integrate => "integrate",
            MoveType::Revisit => "revisit",
            MoveType::Plan => "plan",
            MoveType::Other => "other",
        };
        write!(f, "{s}")
    }
}

/// The policy-ladder rung (§14/§6.2 applied to the model): capability is
/// measured, not assumed. Invariants (move ABI, sandbox, locked rubric,
/// append-only) never relax across rungs — only how a move is *decided* and
/// how strictly its shape is templated does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPolicy {
    /// `decide_move` is a Rust function — today's loop, unchanged in shape.
    L0,
    /// `decide_move` picks from a closed menu of named types.
    L1,
    /// `decide_move` picks freely; `other` (a bespoke move) is allowed.
    L2,
}

/// A move already emitted in the node, summarized for L0's rule and for the
/// L1/L2 prompt's context tail — not the full HTML (§14 budget).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveRecord {
    pub move_type: MoveType,
    pub graded: bool,
}

/// Context handed to `decide_move`/`generate_move*` (§14 context budget).
/// `objective` is wired from `objective::summarize` (§S4) — empty only for a
/// document with no confirmed objective yet. `profile` (§S7) is still empty
/// until that slice exists. Every function degrades gracefully on an empty
/// field, the same way grounding already does in `api.rs::grounding_for`.
#[derive(Debug, Clone, Default)]
pub struct MoveContext {
    pub topic: String,
    pub item_title: String,
    /// Titles of outline items already covered, for narrative continuity —
    /// the direct analogue of `engine::prompt::prose`'s `context` param.
    pub outline_context: String,
    pub prior_moves: Vec<MoveRecord>,
    pub objective: String,
    pub profile: String,
    pub grounding: String,
    /// Verbatim tail of this node's content so far (§14 budget: ~1.5k chars),
    /// fed to `decide_move` only — the caller keeps this updated as moves are
    /// generated. Empty for the node's first move.
    pub node_tail: String,
}

/// A generated move (§6 ABI): HTML + the two invariant flags + tactics.
#[derive(Debug, Clone)]
pub struct GeneratedMove {
    pub move_type: MoveType,
    pub interactive: bool,
    pub graded: bool,
    pub html: String,
    /// Tactic self-labels emitted in the same call (§7) — profile
    /// attribution is a join over `events::EventKind::MoveGenerated`, 0
    /// reflection tokens.
    pub tactics: Vec<String>,
    /// Present iff `graded` — locked together with the move (§8).
    pub rubric: Option<Rubric>,
    /// The revised outline titles a `plan` move proposes (§S4/§5) — the
    /// model computes the diff, not Rust. Empty for every other move type,
    /// and empty for a `plan` move that only remarks in prose without a
    /// concrete change (§S4: "extensões menores silenciosas" — no proposal,
    /// no approval needed). The caller (`api.rs::generate_node`) decides
    /// what "differs from the current outline" means and gates approval.
    pub proposed_outline: Vec<String>,
    /// Whether the first response failed validation and a repair round was
    /// needed. Only meaningful for `MoveRender::Structured` (the streamed
    /// path has no repair). Not yet logged as an event — S3's caller decides
    /// what to do with it (§9 ladder telemetry consumes this once wired).
    pub repaired: bool,
}

/// Decides the next move type. L0 never calls the AI; L1/L2 do, with one
/// bounded repair attempt on a schema violation.
pub async fn decide_move(
    ai: &Ai,
    policy: AgentPolicy,
    ctx: &MoveContext,
) -> Result<MoveType, EngineError> {
    match policy {
        AgentPolicy::L0 => l0_next_move(&ctx.prior_moves),
        AgentPolicy::L1 | AgentPolicy::L2 => decide_move_ai(ai, policy, ctx).await,
    }
}

/// L0's rule: reproduces today's loop exactly — explain, then check. Once
/// both have happened, there is nothing left for `decide_move` to decide;
/// completion is `Assessment::all_demonstrated`'s call, not this function's.
fn l0_next_move(prior: &[MoveRecord]) -> Result<MoveType, EngineError> {
    match prior {
        [] => Ok(MoveType::Explain),
        [
            MoveRecord {
                move_type: MoveType::Explain,
                graded: false,
            },
        ] => Ok(MoveType::Test),
        _ => Err(EngineError::NoNextMove),
    }
}

async fn decide_move_ai(
    ai: &Ai,
    policy: AgentPolicy,
    ctx: &MoveContext,
) -> Result<MoveType, EngineError> {
    let messages = prompt::decide_move(policy, ctx);
    let text = engine::collect(ai, Tier::Fast, messages.clone()).await?;
    if let Ok(mt) = parse::move_type(&text) {
        return Ok(mt);
    }
    let repair = repair_messages(messages, &text, "expected JSON {\"move_type\":\"...\"}");
    let text = engine::collect(ai, Tier::Fast, repair).await?;
    parse::move_type(&text)
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
        proposed_outline: Vec::new(),
        repaired: false,
    }
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
/// bounded repair attempt on a schema violation (§9 ladder telemetry
/// eventually counts these; for now the caller just gets `repaired: true`).
pub async fn generate_move(
    ai: &Ai,
    policy: AgentPolicy,
    move_type: MoveType,
    ctx: &MoveContext,
) -> Result<GeneratedMove, EngineError> {
    debug_assert_eq!(
        move_type.render(),
        MoveRender::Structured,
        "generate_move is only for MoveRender::Structured types — use generate_move_stream"
    );
    let tier = move_type.tier();
    let messages = prompt::generate_move(policy, move_type, ctx);
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

pub mod prompt {
    use super::{AgentPolicy, MoveContext, MoveRecord, MoveType};
    use crate::ai::ChatMessage;
    use crate::engine::prompt::{
        EXERCISE_HTML_CONTRACT, ISLAND_CONTRACT, PROSE_HTML_CONTRACT, sources_block,
    };

    fn non_empty(s: &str) -> &str {
        if s.trim().is_empty() { "(none yet)" } else { s }
    }

    fn describe_prior(prior: &[MoveRecord]) -> String {
        if prior.is_empty() {
            return "(none — this is the first move)".to_string();
        }
        prior
            .iter()
            .map(|m| {
                if m.graded {
                    format!("{} (graded)", m.move_type)
                } else {
                    m.move_type.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The move menu, with `profile` (§7) dropped when there is nothing for it
    /// to investigate.
    ///
    /// A `profile` move probes an OPEN HYPOTHESIS about the learner; on a fresh
    /// document there are none, and the per-move instruction used to handle
    /// that by asking the model to "skip this move type". A model cannot skip a
    /// move it was just told to write: seen live (2026-08-14, first node of a
    /// new document) it dutifully wrote the skip itself into the document —
    /// "Nenhuma hipótese aberta foi listada… nenhuma investigação será gerada."
    /// as the learner's opening prose. So the option is withheld instead of
    /// restraint being requested.
    fn menu(policy: AgentPolicy, ctx: &MoveContext) -> String {
        let types = if ctx.profile.contains(crate::profile::HYPOTHESES_HEADER) {
            "explain, ask, test, profile, confront, integrate, revisit, plan"
        } else {
            "explain, ask, test, confront, integrate, revisit, plan"
        };
        match policy {
            AgentPolicy::L1 => format!(
                "Choose the NEXT move from EXACTLY this closed menu: {types}. \
                 Pick one type; do not combine or invent one."
            ),
            AgentPolicy::L2 => format!(
                "Choose the NEXT move. Prefer a named type ({types}) but you may \
                 use \"other\" for a bespoke move that doesn't fit any of them."
            ),
            AgentPolicy::L0 => unreachable!("L0 decides via l0_next_move, never the AI"),
        }
    }

    /// Prompt for `decide_move` (L1/L2 only — L0 never calls this).
    pub fn decide_move(policy: AgentPolicy, ctx: &MoveContext) -> Vec<ChatMessage> {
        vec![
            ChatMessage::system(format!(
                "You are a personal tutor deciding what to do next in a living \
                 document (§6) — the app is the learner's tutor, not a fixed \
                 exercise machine. {}\n\
                 Respond ONLY with JSON choosing the next move: \
                 {{\"move_type\":\"...\",\"rationale\":\"one short sentence\"}}.",
                menu(policy, ctx)
            )),
            ChatMessage::user(format!(
                "Overall topic: {}\nConcept of this node: {}\n\
                 Curriculum objective: {}\nLearner profile: {}\n\
                 Moves already in this node: {}\n\
                 Node content so far (tail): {}",
                ctx.topic,
                ctx.item_title,
                non_empty(&ctx.objective),
                non_empty(&ctx.profile),
                describe_prior(&ctx.prior_moves),
                non_empty(tail_chars(&ctx.node_tail, 1500)),
            )),
        ]
    }

    /// Last `max_chars` characters of `s` (char-boundary safe) — the §14
    /// verbatim-tail budget for `decide_move`'s context.
    fn tail_chars(s: &str, max_chars: usize) -> &str {
        match s.char_indices().rev().nth(max_chars.saturating_sub(1)) {
            Some((i, _)) => &s[i..],
            None => s,
        }
    }

    fn purpose(move_type: MoveType) -> &'static str {
        match move_type {
            MoveType::Explain => {
                "Write short, atomic explanatory prose for this concept (§6). Do \
                 not include an exercise or ask a question — those are separate \
                 moves."
            }
            MoveType::Confront => {
                "Build the STRONGEST counter-argument to the learner's stated \
                 position (§7): be adversarial, not flattering. Distinguish \
                 legitimate disagreement from a misconception — if it looks like \
                 the latter, say so and explain why, gently but plainly."
            }
            MoveType::Test => {
                "This move MUST be graded: produce a comprehension check AND its \
                 rubric, locked together (§8). Every 'application' objective needs \
                 at least one transfer=true item for a scenario not covered in the \
                 text."
            }
            MoveType::Profile => {
                "Investigate ONE of the open hypotheses about the learner listed in \
                 the profile context (§7) — a short probing question or a targeted \
                 mini-check whose answer would confirm or refute it. If none is \
                 listed (L2 may still pick this move off-menu), probe how the \
                 learner approaches THIS concept instead — one short question. \
                 NEVER write about the absence of a hypothesis: whatever you \
                 produce is what the learner reads."
            }
            MoveType::Plan => {
                "Revise the outline (§5, non-destructive) ONLY if you have a concrete \
                 structural change to propose (reordering, adding, splitting, or \
                 removing concepts) — write your rationale as short prose in \"html\" \
                 and put the COMPLETE revised ordered list of outline item titles \
                 (existing titles you keep, unchanged, plus the new/changed ones) in \
                 the \"outline\" field. If you have nothing structural to propose \
                 right now, just remark in \"html\" and leave \"outline\" empty — the \
                 learner is never asked to approve a non-change."
            }
            _ => "Produce this move's content, atomic and focused on its stated purpose.",
        }
    }

    /// Prompt for the **streamed** path (`MoveRender::Streamed` types): pure
    /// prose contract, no JSON envelope — flags are fixed by the caller from
    /// the type, not emitted here. Tactics ride a trailing sentinel comment
    /// (stripped server-side, never shown — see the module docs).
    pub fn generate_move_streamed(move_type: MoveType, ctx: &MoveContext) -> Vec<ChatMessage> {
        vec![
            ChatMessage::system(format!(
                "You are a personal tutor generating a \"{move_type}\" move (§6 \
                 ABI) for a living document. {}\n\n{PROSE_HTML_CONTRACT}\n\n\
                 {ISLAND_CONTRACT}\n\n\
                 After your HTML, on its own line, append an HTML comment \
                 listing the tactic self-labels you used (e.g. \"analogy\", \
                 \"worked-example\", \"interactive-visual\", \"formal-first\"): \
                 <!--tactics: label-one, label-two-->. This comment is invisible \
                 when rendered and is stripped before storage — it is bookkeeping, \
                 not content.",
                purpose(move_type)
            )),
            ChatMessage::user(format!(
                "Overall topic: {}\nConcept of this node: {}\n\
                 Context of what has been taught so far: {}\n\
                 Curriculum objective: {}\nLearner profile: {}{}",
                ctx.topic,
                ctx.item_title,
                non_empty(&ctx.outline_context),
                non_empty(&ctx.objective),
                non_empty(&ctx.profile),
                sources_block(&ctx.grounding),
            )),
        ]
    }

    /// Prompt for the **structured** path (`MoveRender::Structured` types):
    /// JSON envelope with flags + tactics + (if graded) objectives. Contract
    /// choice mirrors §3.1/§4.4 exactly as `engine::prompt` does: `test`
    /// (always graded, sandbox-capable) gets `EXERCISE_HTML_CONTRACT`; the
    /// rest get `PROSE_HTML_CONTRACT` — getting this backwards means a graded
    /// move's JS vanishes on render, or sanitized prose gets exercise-only
    /// guidance.
    pub fn generate_move(
        policy: AgentPolicy,
        move_type: MoveType,
        ctx: &MoveContext,
    ) -> Vec<ChatMessage> {
        let contract = match move_type {
            MoveType::Test => EXERCISE_HTML_CONTRACT,
            _ => PROSE_HTML_CONTRACT,
        };
        let rung_note = match policy {
            AgentPolicy::L0 => "This move type was chosen by a fixed rule.",
            AgentPolicy::L1 => "This move type was chosen from a closed menu.",
            AgentPolicy::L2 => "This move type was chosen freely.",
        };
        vec![
            ChatMessage::system(format!(
                "You are a personal tutor generating a \"{move_type}\" move (§6 \
                 ABI) for a living document. {rung_note} {}\n\n{contract}\n\n\
                 Also emit the tactic self-labels you used (e.g. \"analogy\", \
                 \"worked-example\", \"interactive-visual\", \"formal-first\") — \
                 short kebab-case tags, in the SAME call (§7).\n\n\
                 Respond ONLY with the Move JSON contract: \
                 {{\"html\":\"...\",\"interactive\":true|false,\"graded\":true|\
                 false,\"tactics\":[\"...\"],\"objectives\":[{{\"id\":\"o1\",\
                 \"kind\":\"knowledge|application|synthesis\",\"description\":\
                 \"...\",\"criteria\":\"...\",\"transfer\":true|false}}],\
                 \"outline\":[\"...\"]}}. Omit \"objectives\" (or leave it empty) \
                 when graded=false. Omit \"outline\" (or leave it empty) for every \
                 move type except \"plan\" with a concrete structural change.",
                purpose(move_type)
            )),
            ChatMessage::user(format!(
                "Overall topic: {}\nConcept of this node: {}\n\
                 Curriculum objective: {}\nLearner profile: {}{}",
                ctx.topic,
                ctx.item_title,
                non_empty(&ctx.objective),
                non_empty(&ctx.profile),
                sources_block(&ctx.grounding),
            )),
        ]
    }
}

// ---------------------------------------------------------------------------
// Tolerant parsing of the model output.
// ---------------------------------------------------------------------------

pub mod parse {
    use super::{EngineError, GeneratedMove, MoveType, Rubric, RubricObjective};
    use crate::engine::parse::extract_json;
    use learnive_core::ObjectiveType;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct RawDecision {
        move_type: MoveType,
        #[serde(default)]
        #[allow(dead_code)]
        rationale: String,
    }

    pub fn move_type(text: &str) -> Result<MoveType, EngineError> {
        let json = extract_json(text).ok_or_else(|| EngineError::Parse("no JSON".to_string()))?;
        let raw: RawDecision =
            serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))?;
        Ok(raw.move_type)
    }

    /// Strips a trailing `<!--tactics: a, b-->` sentinel (see the module
    /// docs on the streamed path) from streamed move output. A missing or
    /// malformed sentinel just means no tactics recorded — the streamed
    /// content already rendered successfully either way, so this never
    /// errors.
    pub fn strip_tactics_sentinel(text: &str) -> (String, Vec<String>) {
        const MARK: &str = "<!--tactics:";
        if let Some(pos) = text.rfind(MARK)
            && let Some(end) = text[pos + MARK.len()..].find("-->")
        {
            let tactics = text[pos + MARK.len()..pos + MARK.len() + end]
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            return (text[..pos].trim_end().to_string(), tactics);
        }
        (text.trim().to_string(), Vec::new())
    }

    #[derive(Deserialize)]
    struct RawMove {
        html: String,
        #[serde(default)]
        interactive: bool,
        #[serde(default)]
        graded: bool,
        #[serde(default)]
        tactics: Vec<String>,
        #[serde(default)]
        objectives: Vec<RawObjective>,
        /// `plan`'s proposed revised outline (§S4) — ignored for every other
        /// move type.
        #[serde(default)]
        outline: Vec<String>,
    }

    #[derive(Deserialize)]
    struct RawObjective {
        id: String,
        #[serde(default = "knowledge")]
        kind: ObjectiveType,
        description: String,
        #[serde(default)]
        criteria: String,
        #[serde(default)]
        transfer: bool,
    }

    fn knowledge() -> ObjectiveType {
        ObjectiveType::Knowledge
    }

    /// Parses + validates a **structured** move against the JSON contract.
    /// `Test` is intrinsically graded (§8) — not the model's discretion, so
    /// `graded` is forced true regardless of what the model set.
    pub fn generated_move(move_type: MoveType, text: &str) -> Result<GeneratedMove, EngineError> {
        let json = extract_json(text).ok_or_else(|| EngineError::Parse("no JSON".to_string()))?;
        let raw: RawMove =
            serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))?;

        if raw.html.trim().is_empty() {
            return Err(EngineError::Parse("empty html".to_string()));
        }

        let graded = raw.graded || matches!(move_type, MoveType::Test);
        if graded && raw.objectives.is_empty() {
            return Err(EngineError::Parse(
                "graded move with no objectives".to_string(),
            ));
        }

        let rubric = graded.then(|| Rubric {
            objectives: raw
                .objectives
                .into_iter()
                .map(|o| RubricObjective {
                    id: o.id,
                    kind: o.kind,
                    description: o.description,
                    criteria: o.criteria,
                    transfer: o.transfer,
                })
                .collect(),
        });

        Ok(GeneratedMove {
            move_type,
            interactive: raw.interactive,
            graded,
            html: raw.html,
            tactics: raw.tactics,
            rubric,
            proposed_outline: raw.outline,
            repaired: false,
        })
    }
}

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
        assert_eq!(MoveType::Confront.tier(), Tier::Robust);
        for mt in [
            MoveType::Ask,
            MoveType::Test,
            MoveType::Profile,
            MoveType::Integrate,
            MoveType::Revisit,
            MoveType::Plan,
            MoveType::Other,
        ] {
            assert_eq!(mt.tier(), Tier::Fast);
        }
    }

    #[test]
    fn render_partitions_streamed_vs_structured() {
        for mt in [
            MoveType::Explain,
            MoveType::Ask,
            MoveType::Confront,
            MoveType::Integrate,
            MoveType::Revisit,
        ] {
            assert_eq!(mt.render(), MoveRender::Streamed);
        }
        for mt in [
            MoveType::Test,
            MoveType::Profile,
            MoveType::Plan,
            MoveType::Other,
        ] {
            assert_eq!(mt.render(), MoveRender::Structured);
        }
    }

    #[tokio::test]
    async fn l0_decides_explain_first_then_test() {
        let ai = mock_ai("unused");
        let ctx = MoveContext::default();

        let first = decide_move(&ai, AgentPolicy::L0, &ctx).await.unwrap();
        assert_eq!(first, MoveType::Explain);

        let mut ctx = ctx;
        ctx.prior_moves.push(MoveRecord {
            move_type: MoveType::Explain,
            graded: false,
        });
        let second = decide_move(&ai, AgentPolicy::L0, &ctx).await.unwrap();
        assert_eq!(second, MoveType::Test);
    }

    #[tokio::test]
    async fn l0_errors_when_node_is_already_complete() {
        let ai = mock_ai("unused");
        let mut ctx = MoveContext::default();
        ctx.prior_moves.push(MoveRecord {
            move_type: MoveType::Explain,
            graded: false,
        });
        ctx.prior_moves.push(MoveRecord {
            move_type: MoveType::Test,
            graded: true,
        });
        let err = decide_move(&ai, AgentPolicy::L0, &ctx).await.unwrap_err();
        assert!(matches!(err, EngineError::NoNextMove));
    }

    #[tokio::test]
    async fn l1_decide_move_parses_ai_json() {
        let ai = mock_ai(r#"{"move_type":"test","rationale":"time to check"}"#);
        let mt = decide_move(&ai, AgentPolicy::L1, &MoveContext::default())
            .await
            .unwrap();
        assert_eq!(mt, MoveType::Test);
    }

    #[tokio::test]
    async fn decide_move_repairs_once_on_malformed_json() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        let ai = scripted_ai(move |_req| {
            let n = calls_inner.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                "not json at all".to_string()
            } else {
                r#"{"move_type":"ask","rationale":"fixed"}"#.to_string()
            }
        });
        let mt = decide_move(&ai, AgentPolicy::L1, &MoveContext::default())
            .await
            .unwrap();
        assert_eq!(mt, MoveType::Ask);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one repair round");
    }

    #[test]
    fn decide_move_prompt_carries_the_profile_text() {
        // §S7 "injeção no decide_move": `MoveContext::profile` (evidence
        // table + distilled traits/hypotheses, wired by `api::profile_for`)
        // must actually reach the decision prompt, not just exist as a field.
        let ctx = MoveContext {
            profile: "worked-example: 3 demonstrated, 0 partial".to_string(),
            ..Default::default()
        };
        let user = &prompt::decide_move(AgentPolicy::L1, &ctx)[1].content;
        assert!(user.contains("worked-example: 3 demonstrated, 0 partial"));
    }

    #[test]
    fn profile_move_is_offered_only_when_there_is_a_hypothesis_to_test() {
        // Regression, found in a live run: on a fresh document (no hypotheses
        // yet) the model picked `profile` and then wrote the skip instruction
        // itself into the learner's document. The menu now withholds the
        // option instead of asking the model to decline it.
        let fresh = MoveContext {
            profile: "Evidence (tactic -> outcome, from the event log):\nanalogy: 1 demonstrated"
                .to_string(),
            ..Default::default()
        };
        let with_hypothesis = MoveContext {
            profile: format!(
                "{} (a \"profile\" move should test one of these):\n- prefers worked examples",
                crate::profile::HYPOTHESES_HEADER
            ),
            ..Default::default()
        };
        for policy in [AgentPolicy::L1, AgentPolicy::L2] {
            let without = &prompt::decide_move(policy, &fresh)[0].content;
            let with = &prompt::decide_move(policy, &with_hypothesis)[0].content;
            assert!(!without.contains("profile"), "{policy:?} offered profile");
            assert!(with.contains("profile"), "{policy:?} withheld profile");
        }
    }

    #[test]
    fn explain_prompt_carries_prose_contract_not_exercise() {
        let ctx = MoveContext::default();
        let sys = &prompt::generate_move_streamed(MoveType::Explain, &ctx)[0].content;
        assert!(sys.contains("NEVER use"));
        assert!(!sys.contains("postMessage"));
        assert!(sys.contains("tactics:"));
        // §S11: the streamed path is the only one with a real IslandGate
        // behind it, so it's the only one told about the island contract.
        assert!(sys.contains("figure data-interactive"));
    }

    #[test]
    fn test_prompt_carries_exercise_contract_and_forces_graded() {
        let ctx = MoveContext::default();
        let sys = &prompt::generate_move(AgentPolicy::L0, MoveType::Test, &ctx)[0].content;
        assert!(sys.contains("postMessage"));
        assert!(sys.contains("sandbox"));
        assert!(sys.contains("MUST be graded"));
    }

    #[test]
    fn structured_prose_prompt_omits_the_island_contract() {
        // §S11 follow-up: `profile`/`plan`/`other` share `PROSE_HTML_CONTRACT`
        // with the streamed path but have no `IslandGate` behind them, and
        // asking a JSON-envelope call to emit raw island HTML/JS inside a
        // string field risks breaking the envelope itself. The island
        // paragraph must stay out of their prompt entirely.
        let ctx = MoveContext::default();
        for mt in [MoveType::Profile, MoveType::Plan, MoveType::Other] {
            let sys = &prompt::generate_move(AgentPolicy::L0, mt, &ctx)[0].content;
            assert!(
                sys.contains("NEVER use"),
                "{mt} should still get the base HTML contract"
            );
            assert!(
                !sys.contains("figure data-interactive"),
                "{mt} must not be told about the island contract"
            );
        }
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
        let mv = generate_move(
            &ai,
            AgentPolicy::L0,
            MoveType::Test,
            &MoveContext::default(),
        )
        .await
        .unwrap();
        assert!(mv.graded, "test is intrinsically graded (§8)");
        let rubric = mv.rubric.unwrap();
        assert_eq!(rubric.objectives.len(), 1);
        assert!(rubric.objectives[0].transfer);
    }

    #[tokio::test]
    async fn plan_move_carries_proposed_outline_through_the_envelope() {
        let ai = mock_ai(
            r#"{"html":"<p>Splitting limits into its own concept.</p>","graded":false,"outline":["Intro","Limits","Continuity","Derivatives"]}"#,
        );
        let mv = generate_move(
            &ai,
            AgentPolicy::L1,
            MoveType::Plan,
            &MoveContext::default(),
        )
        .await
        .unwrap();
        assert!(!mv.graded);
        assert_eq!(
            mv.proposed_outline,
            vec!["Intro", "Limits", "Continuity", "Derivatives"]
        );
    }

    #[tokio::test]
    async fn non_plan_moves_never_carry_a_proposed_outline() {
        let ai = mock_ai(r#"{"html":"<p>A remark, not a proposal.</p>","graded":false}"#);
        let mv = generate_move(
            &ai,
            AgentPolicy::L1,
            MoveType::Profile,
            &MoveContext::default(),
        )
        .await
        .unwrap();
        assert!(mv.proposed_outline.is_empty());
    }

    #[tokio::test]
    async fn generate_move_rejects_graded_with_no_objectives() {
        let ai = mock_ai(r#"{"html":"<form></form>","graded":true,"objectives":[]}"#);
        // First attempt fails validation; repair attempt gets the same broken
        // reply (MockProvider::new is constant), so the whole call errors —
        // proving the invariant is actually enforced, not just parsed.
        let err = generate_move(
            &ai,
            AgentPolicy::L0,
            MoveType::Test,
            &MoveContext::default(),
        )
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
        let mv = generate_move(
            &ai,
            AgentPolicy::L0,
            MoveType::Test,
            &MoveContext::default(),
        )
        .await
        .unwrap();
        assert!(mv.repaired);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// The real check for "L0 ≙ today's loop" (PLAN.md): assemble a node from
    /// L0's Explain+Test output through the SAME `engine::assemble_node`
    /// the live endpoint uses, and assert the structural shape
    /// `engine::tests::assemble_node_wraps_prose_and_exercise` pins today —
    /// not just that the new decision sequence agrees with itself.
    #[tokio::test]
    async fn l0_move_pipeline_matches_todays_node_shape() {
        let explicar_ai = mock_ai("<h2>Limits</h2><p>Explanation.</p> <!--tactics: analogy-->");
        let testar_ai = mock_ai(
            r#"{"html":"<form><input name=\"a\"></form>","interactive":false,"graded":true,"tactics":["worked-example"],"objectives":[{"id":"o1","kind":"application","description":"apply","criteria":"transfers","transfer":true}]}"#,
        );

        let mut ctx = MoveContext {
            topic: "calculus".into(),
            item_title: "Limits".into(),
            ..Default::default()
        };

        let mt1 = decide_move(&explicar_ai, AgentPolicy::L0, &ctx)
            .await
            .unwrap();
        assert_eq!(mt1, MoveType::Explain);
        let accumulated = collect_stream(&explicar_ai, mt1, &ctx).await;
        let mv1 = finish_streamed_move(mt1, &accumulated);
        assert!(!mv1.graded);

        ctx.prior_moves.push(MoveRecord {
            move_type: mv1.move_type,
            graded: mv1.graded,
        });
        let mt2 = decide_move(&testar_ai, AgentPolicy::L0, &ctx)
            .await
            .unwrap();
        assert_eq!(mt2, MoveType::Test);
        let mv2 = generate_move(&testar_ai, AgentPolicy::L0, mt2, &ctx)
            .await
            .unwrap();
        assert!(mv2.graded);
        let rubric = mv2.rubric.clone().unwrap();
        assert!(!rubric.objectives.is_empty());

        // Same assembly path as `api.rs::finalize` uses today.
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
        let done = decide_move(&testar_ai, AgentPolicy::L0, &ctx)
            .await
            .unwrap_err();
        assert!(matches!(done, EngineError::NoNextMove));
    }
}
