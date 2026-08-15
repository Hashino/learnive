use super::{AgentPolicy, MoveContext, MoveRecord, MoveType};
use crate::ai::ChatMessage;
use crate::engine::prompt::{
    CITE_CONTRACT, EXERCISE_HTML_CONTRACT, ISLAND_CONTRACT, PROSE_HTML_CONTRACT, sources_block,
};

/// [`CITE_CONTRACT`], appended only when there is grounding to cite —
/// otherwise the model would see an instruction about a SOURCES block that
/// never shows up in the user message below it.
fn cite_addendum(grounding: &str) -> &'static str {
    if grounding.trim().is_empty() {
        ""
    } else {
        CITE_CONTRACT
    }
}

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
    let cite = cite_addendum(&ctx.grounding);
    vec![
        ChatMessage::system(format!(
            "You are a personal tutor generating a \"{move_type}\" move (§6 \
             ABI) for a living document. {}\n\n{PROSE_HTML_CONTRACT}\n\n\
             {ISLAND_CONTRACT}\n\n{cite}\n\n\
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
    // The exercise runs unsanitized in its own sandbox with no click handler
    // (§4.4) — citing there would be inert markup at best. Every other
    // structured move lands in the sanitized app origin, same as the
    // streamed path, so it gets the same addendum.
    let cite = if move_type == MoveType::Test {
        ""
    } else {
        cite_addendum(&ctx.grounding)
    };
    let rung_note = match policy {
        AgentPolicy::L0 => "This move type was chosen by a fixed rule.",
        AgentPolicy::L1 => "This move type was chosen from a closed menu.",
        AgentPolicy::L2 => "This move type was chosen freely.",
    };
    vec![
        ChatMessage::system(format!(
            "You are a personal tutor generating a \"{move_type}\" move (§6 \
             ABI) for a living document. {rung_note} {}\n\n{contract}\n\n{cite}\n\n\
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
