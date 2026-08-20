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
//! **Merged decide+generate for L1/L2 (§14 latency, 2026-08-19).**
//! `decide_move_ai` used to always be its own full round trip before content
//! generation started — every move paid for two separate calls regardless of
//! what was decided. [`decide_and_generate`] collapses that to one call for
//! the common case: it starts a single **streamed** request whose response
//! must begin with a `<!--move: type-->` marker (the mirror of the existing
//! trailing `<!--tactics: ...-->` sentinel, just leading) —
//! [`read_move_marker`] reads it off the front of the stream. If the
//! declared type is [`MoveRender::Streamed`], the model keeps writing that
//! move's full content on the SAME call and [`DecidedMove::Streamed`] hands
//! the remainder of the stream straight back to the caller — no second round
//! trip. If it's [`MoveRender::Structured`] (including `research`), the
//! model was told to write nothing else; [`DecidedMove::Structured`] carries
//! only the type, and the caller still makes a real [`generate_move`] call
//! for its content — that call was already non-streamed, kept that way
//! deliberately (see below), so this branch costs the same two round trips
//! as before the merge, never more.
//!
//! This is why the merge is asymmetric rather than a single call for every
//! move type: a **standalone** JSON-only response has previously been
//! swallowed whole by a reasoning-heavy model's invisible `reasoning` delta
//! and never reached usable `content` (`engine.rs`'s `collect` doc comment,
//! confirmed live 2026-08-17) — that's why `decide_move_ai` and structured
//! `generate_move` use `Ai::complete` (non-streamed), not `Ai::stream`, to
//! this day. Folding the decision into the FRONT of an already-reliable
//! streamed prose call (proven live to deliver content once reasoning ends,
//! confirmed again 2026-08-19) doesn't repeat that failure; folding it into
//! a structured JSON call that streams on its own would.
//!
//! Tier: [`decide_and_generate`] always calls at [`Tier::Fast`], same as the
//! old standalone `decide_move_ai` — a deliberate trade, not an oversight.
//! [`MoveType::tier`] routes `explain`/`confront` to `Tier::Robust`; a move
//! chosen via this merged call that turns out to be one of those two is
//! generated by the fast model instead of the robust one. The alternative
//! (always call at `Tier::Robust`, to never risk that) was rejected: it
//! would pay robust-tier latency/cost on every decision, including the
//! common case where the outcome is Fast-tier content or no content at all
//! (a `Structured` pick, whose real content-call already uses the correct
//! tier) — working directly against the latency goal that motivated this
//! change. Revisit if this quality trade proves to matter in practice; there
//! is no telemetry yet to judge it by (same gap PLAN.md already flags for
//! `calibrate_rung`).
//!
//! `decide_and_generate` falls back to the OLD two-call path
//! (`decide_move_ai` then, if applicable, a fresh `generate_move_stream`) on
//! any marker-read failure — never worse than before this change, only
//! sometimes better. L0 is untouched: it never called the AI for `decide`
//! in the first place, so there was no round trip here to collapse.
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
use futures_util::StreamExt;

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
    /// Acquires grounding for a concept the current corpus has nothing on
    /// (§S13, `api::cold_start::acquire`) — chosen exactly like any other
    /// move (offered in the menu only when `MoveContext::grounding` is
    /// empty, mirroring how `profile` is withheld with no open hypothesis),
    /// but produces no learner-facing content of its own. The orchestration
    /// loop (`api::generation::generate_node`) intercepts it before
    /// `render()` is ever consulted, runs acquisition, refreshes the
    /// context's grounding, and loops back to decide the real next move —
    /// `render()`/`generate_move*` must never actually be called with this
    /// type (debug-asserted the same way the streamed/structured split is).
    Research,
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
    /// Never actually called for `Research` (see its own doc comment) — the
    /// orchestration loop intercepts it first; `Structured` here is just an
    /// arbitrary total-match default, not a real routing decision.
    pub fn render(self) -> MoveRender {
        match self {
            MoveType::Test
            | MoveType::Profile
            | MoveType::Plan
            | MoveType::Other
            | MoveType::Research => MoveRender::Structured,
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
            MoveType::Research => "research",
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
/// `objective` is the document's current objective text (§S4) — empty only
/// for a document with no confirmed objective yet. `profile` (§S7) is still empty
/// until that slice exists. Every function degrades gracefully on an empty
/// field, the same way grounding already does in `api.rs::grounding_for`.
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
    pub profile: String,
    pub grounding: String,
    /// Verbatim tail of this node's content so far (§14 budget: ~1.5k chars),
    /// fed to `decide_move` only — the caller keeps this updated as moves are
    /// generated. Empty for the node's first move.
    pub node_tail: String,
    /// Set once a `research` move has run for this node-generation call
    /// (§S13) — withholds `research` from the menu on any further
    /// `decide_move` call in the same request, the cap on repeated
    /// acquisition attempts. Not persisted; scoped to one `generate_node`
    /// call, same lifetime as `prior_moves`.
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
/// `pub(crate)`: called directly by `api::generation`'s loop (not through
/// [`decide_move`]/[`decide_and_generate`]) since L0 has no round trip to
/// merge and no reason to go through either.
pub(crate) fn l0_next_move(prior: &[MoveRecord]) -> Result<MoveType, EngineError> {
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

/// Outcome of [`decide_and_generate`] — see the module docs' "merged
/// decide+generate" note.
pub enum DecidedMove {
    /// A `MoveRender::Streamed` type was chosen; `stream` continues exactly
    /// where the leading marker left off (any bytes from the same chunk
    /// that closed it are replayed first) — pump it exactly like
    /// [`generate_move_stream`]'s own output, then call
    /// [`finish_streamed_move`] once it ends.
    Streamed {
        move_type: MoveType,
        stream: TokenStream,
    },
    /// A `MoveRender::Structured` type (or `research`) was chosen via a
    /// marker-only response with no content behind it — the caller still
    /// needs a real [`generate_move`] call for its content.
    Structured { move_type: MoveType },
}

const MOVE_MARKER_PREFIX: &str = "<!--move: ";
const MOVE_MARKER_SUFFIX: &str = "-->";
/// Bound on how many characters of a streamed response `read_move_marker`
/// buffers looking for a complete marker before giving up. Only counts real
/// content chunks (the provider's `reasoning` delta never reaches this
/// stream — `parse_sse_line` drops it before `Ai::stream` ever yields a
/// chunk), so a long reasoning phase before the marker appears never trips
/// this; only a marker that's missing, malformed, or absurdly delayed does.
const MOVE_MARKER_BUFFER_CAP: usize = 200;

/// Reads the leading `<!--move: type-->` marker off a
/// [`decide_and_generate`] response. Returns the decided type and a stream
/// that continues exactly where the marker left off: any trailing bytes
/// from the same chunk that closed it are replayed first, then the
/// underlying stream is relayed verbatim, unchanged.
async fn read_move_marker(mut tokens: TokenStream) -> Result<(MoveType, TokenStream), EngineError> {
    let mut buf = String::new();
    loop {
        match tokens.next().await {
            Some(Ok(chunk)) => {
                buf.push_str(&chunk);
                if let Some(open) = buf.find(MOVE_MARKER_PREFIX) {
                    let type_start = open + MOVE_MARKER_PREFIX.len();
                    if let Some(close_rel) = buf[type_start..].find(MOVE_MARKER_SUFFIX) {
                        let type_end = type_start + close_rel;
                        let move_type = parse::move_type_name(buf[type_start..type_end].trim())?;
                        let leftover = buf[type_end + MOVE_MARKER_SUFFIX.len()..]
                            .trim_start_matches('\n')
                            .to_string();
                        let rest: TokenStream = Box::pin(async_stream::stream! {
                            if !leftover.is_empty() {
                                yield Ok(leftover);
                            }
                            while let Some(item) = tokens.next().await {
                                yield item;
                            }
                        });
                        return Ok((move_type, rest));
                    }
                }
                if buf.len() > MOVE_MARKER_BUFFER_CAP {
                    return Err(EngineError::Parse(
                        "no <!--move: type--> marker in leading response".to_string(),
                    ));
                }
            }
            Some(Err(e)) => return Err(e.into()),
            None => {
                return Err(EngineError::Parse(
                    "stream ended before a move marker".to_string(),
                ));
            }
        }
    }
}

/// Merged decide+generate for L1/L2 — see the module docs' "merged
/// decide+generate" note for the full rationale (why the tier is fixed at
/// `Tier::Fast`, why the fallback exists, why this never touches L0).
pub async fn decide_and_generate(
    ai: &Ai,
    policy: AgentPolicy,
    ctx: &MoveContext,
) -> Result<DecidedMove, EngineError> {
    debug_assert_ne!(
        policy,
        AgentPolicy::L0,
        "L0 never calls the AI — use l0_next_move directly"
    );
    let messages = prompt::decide_and_generate(policy, ctx);
    let outcome = match ai.stream(Tier::Fast, messages).await {
        Ok(tokens) => read_move_marker(tokens).await,
        Err(e) => Err(e.into()),
    };
    match outcome {
        Ok((move_type, rest)) => Ok(match move_type.render() {
            MoveRender::Streamed => DecidedMove::Streamed {
                move_type,
                stream: rest,
            },
            MoveRender::Structured => DecidedMove::Structured { move_type },
        }),
        Err(_) => decide_only_fallback(ai, policy, ctx).await,
    }
}

/// Fallback for [`decide_and_generate`] on any marker-read failure — the OLD
/// two-call path, unchanged. Never worse than before the merge existed.
async fn decide_only_fallback(
    ai: &Ai,
    policy: AgentPolicy,
    ctx: &MoveContext,
) -> Result<DecidedMove, EngineError> {
    let move_type = decide_move_ai(ai, policy, ctx).await?;
    Ok(match move_type.render() {
        MoveRender::Streamed => DecidedMove::Streamed {
            move_type,
            stream: generate_move_stream(ai, move_type, ctx).await?,
        },
        MoveRender::Structured => DecidedMove::Structured { move_type },
    })
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

pub mod prompt;

pub mod parse;

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

    /// §14 merged decide+generate: the marker and any content from the same
    /// chunk that closed it must both be recovered correctly.
    #[tokio::test]
    async fn read_move_marker_splits_type_from_leftover_content() {
        let stream: TokenStream = Box::pin(async_stream::stream! {
            yield Ok("<!--move: explain-->".to_string());
            yield Ok("Hello ".to_string());
            yield Ok("world.".to_string());
        });
        let (move_type, rest) = read_move_marker(stream).await.unwrap();
        assert_eq!(move_type, MoveType::Explain);
        let collected: String = rest.map(|r| r.unwrap()).collect::<Vec<_>>().await.concat();
        assert_eq!(collected, "Hello world.");
    }

    #[tokio::test]
    async fn read_move_marker_handles_marker_and_content_in_the_same_chunk() {
        let stream: TokenStream = Box::pin(async_stream::stream! {
            yield Ok("<!--move: test-->rest-of-chunk".to_string());
        });
        let (move_type, rest) = read_move_marker(stream).await.unwrap();
        assert_eq!(move_type, MoveType::Test);
        let collected: String = rest.map(|r| r.unwrap()).collect::<Vec<_>>().await.concat();
        assert_eq!(collected, "rest-of-chunk");
    }

    #[tokio::test]
    async fn read_move_marker_errors_when_no_marker_appears() {
        let filler = "x".repeat(300);
        let stream: TokenStream = Box::pin(async_stream::stream! {
            yield Ok(filler);
        });
        assert!(read_move_marker(stream).await.is_err());
    }

    /// End to end: a mock reply carrying the marker plus streamed content
    /// yields `DecidedMove::Streamed` with the right type and the content
    /// recovered intact, in one call.
    #[tokio::test]
    async fn decide_and_generate_streams_content_after_a_streamed_marker() {
        let ai = mock_ai("<!--move: explain-->Explained content here.");
        let ctx = MoveContext::default();
        let outcome = decide_and_generate(&ai, AgentPolicy::L1, &ctx)
            .await
            .unwrap();
        match outcome {
            DecidedMove::Streamed { move_type, stream } => {
                assert_eq!(move_type, MoveType::Explain);
                let text: String = stream
                    .map(|r| r.unwrap())
                    .collect::<Vec<_>>()
                    .await
                    .concat();
                assert!(text.contains("Explained"));
            }
            DecidedMove::Structured { .. } => panic!("expected streamed"),
        }
    }

    /// A structured-render type (e.g. `test`) needs no content in the same
    /// call — the marker alone is a complete, valid response.
    #[tokio::test]
    async fn decide_and_generate_returns_structured_with_no_content_call() {
        let ai = mock_ai("<!--move: test-->");
        let ctx = MoveContext::default();
        let outcome = decide_and_generate(&ai, AgentPolicy::L1, &ctx)
            .await
            .unwrap();
        match outcome {
            DecidedMove::Structured { move_type } => assert_eq!(move_type, MoveType::Test),
            DecidedMove::Streamed { .. } => panic!("expected structured"),
        }
    }

    /// A response with no marker at all must not surface as an error to the
    /// caller — it falls back to the OLD two-call path (`decide_move_ai`
    /// then, if applicable, a fresh `generate_move_stream`), reusing that
    /// already-tested code unchanged (see the module docs' fallback note).
    #[tokio::test]
    async fn decide_and_generate_falls_back_to_the_two_call_path_on_a_missing_marker() {
        let ai = scripted_ai(|req| {
            let sys = req
                .messages
                .first()
                .map(|m| m.content.as_str())
                .unwrap_or("");
            if sys.contains("SAME response") {
                "No marker here, sorry.".to_string()
            } else if sys.contains("Respond ONLY with JSON choosing the next move") {
                r#"{"move_type":"explain","rationale":"fallback"}"#.to_string()
            } else {
                "Explained via fallback.".to_string()
            }
        });
        let ctx = MoveContext::default();
        let outcome = decide_and_generate(&ai, AgentPolicy::L1, &ctx)
            .await
            .unwrap();
        match outcome {
            DecidedMove::Streamed { move_type, stream } => {
                assert_eq!(move_type, MoveType::Explain);
                let text: String = stream
                    .map(|r| r.unwrap())
                    .collect::<Vec<_>>()
                    .await
                    .concat();
                assert!(text.contains("fallback"));
            }
            DecidedMove::Structured { .. } => panic!("expected streamed via fallback"),
        }
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

        // One-shot assembly from raw moves — `api.rs::finalize` now goes
        // through `engine::finalize_node` against already-tagged,
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
        let done = decide_move(&testar_ai, AgentPolicy::L0, &ctx)
            .await
            .unwrap_err();
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
        let (ai, policy) = crate::api::build_ai(&config, &secret);

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
            match generate_move(&ai, policy, MoveType::Test, &ctx).await {
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

    /// Latency investigation: for a real `Explain` move under L1/L2, times
    /// `decide_move` (a full, non-streamed round trip to pick the move type)
    /// separately from time-to-first-token of the subsequent
    /// `generate_move_stream` call, against the REAL configured provider.
    /// Answers "where does the >1min wall time actually go" instead of
    /// guessing. Not part of the normal suite. Run with:
    /// `cargo test -p learnive movement::tests::live_time_to_first_token -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "hits the real configured AI provider; run manually, see doc comment"]
    async fn live_time_to_first_token() {
        let env_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env");
        crate::load_dotenv(env_path);
        let data_dir =
            std::env::temp_dir().join(format!("learnive-live-timing-{}", std::process::id()));
        let config = crate::config::AppConfig::load(&data_dir);
        let secret = crate::secret::SecretStore::open(&data_dir);
        let (ai, policy) = crate::api::build_ai(&config, &secret);
        println!("policy rung = {policy:?}");

        for round in 1..=3 {
            let ctx = MoveContext {
                topic: "Photosynthesis".into(),
                item_title: "the light-dependent reactions".into(),
                objective: "Demonstrate understanding of the light-dependent reactions.".into(),
                ..Default::default()
            };

            let t0 = std::time::Instant::now();
            let move_type = match decide_move(&ai, policy, &ctx).await {
                Ok(mt) => mt,
                Err(e) => {
                    println!(
                        "round {round}: decide_move ERROR after {:?}: {e}",
                        t0.elapsed()
                    );
                    continue;
                }
            };
            let t1 = std::time::Instant::now();
            println!(
                "round {round}: decide_move -> {move_type:?} in {:?}",
                t1 - t0
            );

            if move_type.render() != MoveRender::Streamed {
                println!("round {round}: decided move is structured, not streamed — skipping TTFT");
                continue;
            }

            let mut tokens = match generate_move_stream(&ai, move_type, &ctx).await {
                Ok(s) => s,
                Err(e) => {
                    println!(
                        "round {round}: generate_move_stream ERROR after {:?}: {e}",
                        t1.elapsed()
                    );
                    continue;
                }
            };
            let t2 = std::time::Instant::now();
            println!(
                "round {round}: stream() call itself returned in {:?}",
                t2 - t1
            );
            match tokens.next().await {
                Some(Ok(first)) => {
                    let t3 = std::time::Instant::now();
                    println!(
                        "round {round}: first token after {:?} (total since decide_move start: {:?}) — {:?}",
                        t3 - t2,
                        t3 - t0,
                        first.chars().take(40).collect::<String>()
                    );
                }
                Some(Err(e)) => println!("round {round}: stream ERROR: {e}"),
                None => println!("round {round}: stream ended with no tokens"),
            }
        }
    }

    /// Dumps the REAL request bodies `decide_move` and an `Ask` move's
    /// streamed generation would send — no network — so they can be curled
    /// directly against the provider to isolate raw provider TTFB from
    /// app-side overhead, with realistic (not toy) prompt sizes.
    #[test]
    #[ignore = "writes to /tmp for a manual curl comparison, see doc comment"]
    fn dump_real_request_bodies() {
        let env_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env");
        crate::load_dotenv(env_path);
        let fast = std::env::var("LEARNIVE_MODEL_FAST").unwrap_or_default();
        let robust = std::env::var("LEARNIVE_MODEL_ROBUST").unwrap_or_default();

        let ctx = MoveContext {
            topic: "Photosynthesis".into(),
            item_title: "the light-dependent reactions".into(),
            objective: "Demonstrate understanding of the light-dependent reactions.".into(),
            ..Default::default()
        };

        #[derive(serde::Serialize)]
        struct Body<'a> {
            model: &'a str,
            messages: &'a [crate::ai::ChatMessage],
            stream: bool,
        }

        let decide = prompt::decide_move(AgentPolicy::L1, &ctx);
        println!(
            "decide_move chars: {}",
            decide.iter().map(|m| m.content.len()).sum::<usize>()
        );
        std::fs::write(
            "/tmp/claude-1000/-home-hashino-Projects-learnive/47a18ee9-2d2e-4f3d-83f6-0fb5cc70e776/scratchpad/req_decide_move.json",
            serde_json::to_string(&Body {
                model: &fast,
                messages: &decide,
                stream: true,
            })
            .unwrap(),
        )
        .unwrap();

        let ask = prompt::generate_move_streamed(MoveType::Ask, &ctx);
        println!(
            "ask chars: {}",
            ask.iter().map(|m| m.content.len()).sum::<usize>()
        );
        std::fs::write(
            "/tmp/claude-1000/-home-hashino-Projects-learnive/47a18ee9-2d2e-4f3d-83f6-0fb5cc70e776/scratchpad/req_ask.json",
            serde_json::to_string(&Body {
                model: &fast,
                messages: &ask,
                stream: true,
            })
            .unwrap(),
        )
        .unwrap();

        let explain = prompt::generate_move_streamed(MoveType::Explain, &ctx);
        std::fs::write(
            "/tmp/claude-1000/-home-hashino-Projects-learnive/47a18ee9-2d2e-4f3d-83f6-0fb5cc70e776/scratchpad/req_explain.json",
            serde_json::to_string(&Body {
                model: &robust,
                messages: &explain,
                stream: true,
            })
            .unwrap(),
        )
        .unwrap();
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
            let messages = prompt::generate_move(AgentPolicy::L1, MoveType::Test, &ctx);
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
