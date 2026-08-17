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

/// Continuity instruction + the node's own verbatim tail (§14 budget),
/// shared by `generate_move_streamed`/`generate_move` — `decide_move`
/// already got `node_tail` (line ~90); the content-writing calls didn't,
/// which is exactly why a node's second move used to open with its own
/// fresh `<h2>` repeating the concept title, reintroducing it from
/// scratch as if the first move had never run (seen live, 2026-08-15: a
/// node's `explain` and `integrate` moves each titled themselves
/// "Noção de iteração em programação").
fn continuity_note() -> &'static str {
    "If \"Node content so far\" below is non-empty, this move is NOT the \
     first in this node — it continues content the learner already read. \
     Do NOT reopen with a heading that repeats the node's own concept \
     title, and do NOT reintroduce the concept as if from scratch: write \
     the next distinct section, assuming the reader just finished what's \
     shown."
}

/// §S15 generalization: `topic` (the whole document's subject) and
/// `item_title` (this node's own, narrower concept) are both handed to
/// every move with nothing distinguishing their roles — the model can
/// (and did, live, 2026-08-17) treat the overall topic as content to teach
/// in THIS node rather than as background. `scope_addendum` below already
/// names a concrete parent when this node is a prerequisite sub-node; this
/// note is the general form, present on every move regardless of tree
/// shape, so the same drift can't happen on a node with no parent at all.
fn topic_scope_note() -> &'static str {
    "\"Overall topic\" below is background context ONLY — it tells you \
     which aspects of THIS node's own concept (\"Concept of this node\") \
     to emphasize or frame. It is NOT itself something to teach here: \
     teach the node's own concept, not the document's overall subject."
}

fn node_so_far_line(ctx: &MoveContext) -> String {
    format!(
        "\nNode content so far: {}",
        non_empty(tail_chars(&ctx.node_tail, 1500))
    )
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
    let mut types = if ctx.profile.contains(crate::profile::HYPOTHESES_HEADER) {
        "explain, ask, test, profile, confront, integrate, revisit, plan".to_string()
    } else {
        "explain, ask, test, confront, integrate, revisit, plan".to_string()
    };
    // Offered only when there is genuinely nothing to ground on — same
    // withhold-don't-ask-restraint pattern as `profile` above (a model told
    // "research is available" when grounding already exists has no reason
    // not to pick it "just in case", spending a whole move on nothing).
    if !ctx.research_attempted && ctx.grounding.trim().is_empty() {
        types.push_str(", research");
    }
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
             document — the app is the learner's tutor, not a fixed \
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

/// §S15 addendum for `Test`: when this node has its own decomposed
/// sub-concepts (a prerequisite tree, or a question that spawned an
/// elaboration), at least one objective must require combining them — the
/// structural answer to shallow mastery a light pass through prerequisites
/// would otherwise risk (a node only reaches `Demonstrated` after an
/// exercise that genuinely needs its children together, not each isolated).
fn integration_addendum(children_titles: &[String]) -> String {
    if children_titles.is_empty() {
        return String::new();
    }
    format!(
        " This node has its own sub-concepts, already taught separately: {}. \
         At least one objective here MUST require combining or applying them \
         TOGETHER, not testing any one of them in isolation again.",
        children_titles.join(", ")
    )
}

/// §S15 learn/review/skip: the learner marked this node as a review, not
/// first-time learning — every move purpose gets told to shrink its scope
/// accordingly, never to lower the evidence bar (`Test` still must grade).
fn review_addendum(review_mode: bool, move_type: MoveType) -> &'static str {
    if !review_mode {
        return "";
    }
    match move_type {
        MoveType::Test => {
            " The learner marked this as a REVIEW of something they already \
             believe they know: keep the check to just one or two short \
             exercises, not a full battery."
        }
        _ => {
            " The learner marked this as a REVIEW of something they already \
             believe they know, not first-time learning: keep this to a \
             compact definition-level refresher, a few sentences, not a full \
             lesson."
        }
    }
}

/// §S15: when this node is a prerequisite sub-node, every move prompt
/// already sees `topic` (the whole document's subject) alongside
/// `item_title` (this node's own, narrower concept) — with nothing telling
/// the model to keep those apart, the document topic (usually the thing the
/// PARENT node teaches) bled into the sub-node's own content instead of the
/// sub-node's narrower concept staying self-contained. Observed live
/// (2026-08-17): a "Defining and calling functions in Rust" prerequisite
/// node, in a "recursive functions in Rust" document, ended up teaching
/// recursion's base case/recursive step and setting a recursive exercise —
/// the parent node's own material, taught before the parent node existed.
fn scope_addendum(parent_title: Option<&str>, item_title: &str) -> String {
    match parent_title {
        None => String::new(),
        Some(parent) => format!(
            " This node is a PREREQUISITE STEP toward \"{parent}\", which is \
             its own separate node, taught later. Stay strictly inside \
             \"{item_title}\"'s own scope here: do not teach, preview, or \
             lean on techniques/content specific to \"{parent}\" — leave \
             that entirely for its own node."
        ),
    }
}

fn purpose(move_type: MoveType, ctx: &MoveContext) -> String {
    let base = match move_type {
        MoveType::Explain => {
            "Write short, atomic explanatory prose for this concept. Do \
             not include an exercise or ask a question — those are separate \
             moves."
        }
        MoveType::Confront => {
            "Build the STRONGEST counter-argument to the learner's stated \
             position: be adversarial, not flattering. Distinguish \
             legitimate disagreement from a misconception — if it looks like \
             the latter, say so and explain why, gently but plainly."
        }
        MoveType::Test => {
            "This move MUST be graded: produce a comprehension check AND its \
             rubric, locked together. Every 'application' objective needs \
             at least one transfer=true item for a scenario not covered in the \
             text. If \"Context of what has been taught so far\" shows a prior \
             node's exercise, this check must probe something genuinely new — \
             not the same operation on cosmetically different numbers/names."
        }
        MoveType::Profile => {
            "Investigate ONE of the open hypotheses about the learner listed in \
             the profile context — a short probing question or a targeted \
             mini-check whose answer would confirm or refute it. If none is \
             listed (L2 may still pick this move off-menu), probe how the \
             learner approaches THIS concept instead — one short question. \
             NEVER write about the absence of a hypothesis: whatever you \
             produce is what the learner reads."
        }
        MoveType::Plan => {
            "Revise the outline non-destructively ONLY if you have a concrete \
             structural change to propose (reordering, adding, splitting, or \
             removing concepts) — write your rationale as short prose in \"html\" \
             and put the COMPLETE revised ordered list of outline item titles \
             (existing titles you keep, unchanged, plus the new/changed ones) in \
             the \"outline\" field. If you have nothing structural to propose \
             right now, just remark in \"html\" and leave \"outline\" empty — the \
             learner is never asked to approve a non-change."
        }
        _ => "Produce this move's content, atomic and focused on its stated purpose.",
    };
    let integration = if move_type == MoveType::Test {
        integration_addendum(&ctx.children_titles)
    } else {
        String::new()
    };
    format!(
        "{base}{integration}{}{}",
        review_addendum(ctx.review_mode, move_type),
        scope_addendum(ctx.parent_title.as_deref(), &ctx.item_title)
    )
}

/// Prompt for the **streamed** path (`MoveRender::Streamed` types): pure
/// prose contract, no JSON envelope — flags are fixed by the caller from
/// the type, not emitted here. Tactics ride a trailing sentinel comment
/// (stripped server-side, never shown — see the module docs).
pub fn generate_move_streamed(move_type: MoveType, ctx: &MoveContext) -> Vec<ChatMessage> {
    let cite = cite_addendum(&ctx.grounding);
    // A `plan` move's whole job is reasoning about the outline/topic as a
    // structural whole — the "don't teach the overall topic" framing below
    // would be irrelevant noise there, not a helpful constraint.
    let scope_note = if move_type == MoveType::Plan {
        ""
    } else {
        topic_scope_note()
    };
    vec![
        ChatMessage::system(format!(
            "You are a personal tutor generating a \"{move_type}\" move \
             for a living document. {}\n\n{}\n\n{scope_note}\n\n{PROSE_HTML_CONTRACT}\n\n\
             {ISLAND_CONTRACT}\n\n{cite}\n\n\
             After your HTML, on its own line, append an HTML comment \
             listing the tactic self-labels you used (e.g. \"analogy\", \
             \"worked-example\", \"interactive-visual\", \"formal-first\"): \
             <!--tactics: label-one, label-two-->. This comment is invisible \
             when rendered and is stripped before storage — it is bookkeeping, \
             not content.",
            purpose(move_type, ctx),
            continuity_note()
        )),
        ChatMessage::user(format!(
            "Overall topic: {}\nConcept of this node: {}\n\
             Context of what has been taught so far: {}\n\
             Curriculum objective: {}\nLearner profile: {}{}{}",
            ctx.topic,
            ctx.item_title,
            non_empty(&ctx.outline_context),
            non_empty(&ctx.objective),
            non_empty(&ctx.profile),
            sources_block(&ctx.grounding),
            node_so_far_line(ctx),
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
            "You are a personal tutor generating a \"{move_type}\" move \
             for a living document. {rung_note} {}\n\n{}\n\n{}\n\n{contract}\n\n{cite}\n\n\
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
            purpose(move_type, ctx),
            continuity_note(),
            topic_scope_note()
        )),
        ChatMessage::user(format!(
            "Overall topic: {}\nConcept of this node: {}\n\
             Context of what has been taught so far: {}\n\
             Curriculum objective: {}\nLearner profile: {}{}{}",
            ctx.topic,
            ctx.item_title,
            non_empty(&ctx.outline_context),
            non_empty(&ctx.objective),
            non_empty(&ctx.profile),
            sources_block(&ctx.grounding),
            node_so_far_line(ctx),
        )),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu_text(ctx: &MoveContext) -> String {
        decide_move(AgentPolicy::L1, ctx)
            .into_iter()
            .map(|m| m.content)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// §S13 withhold-don't-ask-restraint: `research` only belongs on the
    /// menu when there is genuinely nothing to ground on yet, and must
    /// disappear the moment either grounding exists or one attempt already
    /// ran this node — otherwise a model "offered" the option with nothing
    /// to gain picks it anyway (the same failure mode `profile` had before
    /// it was withheld the same way).
    #[test]
    fn research_offered_only_when_ungrounded_and_unattempted() {
        let bare = MoveContext::default();
        assert!(menu_text(&bare).contains(", research"));

        let grounded = MoveContext {
            grounding: "[id: x | loc: sec:1 | Title — Sec]\nSome passage.".into(),
            ..Default::default()
        };
        assert!(!menu_text(&grounded).contains(", research"));

        let attempted = MoveContext {
            research_attempted: true,
            ..Default::default()
        };
        assert!(!menu_text(&attempted).contains(", research"));
    }

    /// §S15: a node with materialized children must be told, in its `test`
    /// move, to integrate them rather than probe each in isolation — the
    /// structural answer to shallow prerequisite mastery. A node with none
    /// must not see this instruction (it would be nonsensical noise).
    #[test]
    fn test_move_asks_to_integrate_children_only_when_present() {
        let with_children = MoveContext {
            children_titles: vec!["Product rule".into(), "Chain rule".into()],
            ..Default::default()
        };
        let sys = &generate_move(AgentPolicy::L1, MoveType::Test, &with_children)[0].content;
        assert!(sys.contains("Product rule"));
        assert!(sys.contains("MUST require combining"));

        let bare = MoveContext::default();
        let sys = &generate_move(AgentPolicy::L1, MoveType::Test, &bare)[0].content;
        assert!(!sys.contains("MUST require combining"));
    }

    /// §S15 learn/review/skip: a review-mode node's moves are told to stay
    /// short, in both the streamed and structured paths — but the check
    /// still must grade (never a lowered evidence bar).
    #[test]
    fn review_mode_asks_for_a_short_pass_not_a_lower_bar() {
        let review = MoveContext {
            review_mode: true,
            ..Default::default()
        };
        let explain_sys = &generate_move_streamed(MoveType::Explain, &review)[0].content;
        assert!(explain_sys.contains("REVIEW"));
        assert!(explain_sys.contains("compact"));

        let test_sys = &generate_move(AgentPolicy::L1, MoveType::Test, &review)[0].content;
        assert!(test_sys.contains("REVIEW"));
        assert!(test_sys.contains("MUST be graded"));

        let bare = MoveContext::default();
        let bare_explain = &generate_move_streamed(MoveType::Explain, &bare)[0].content;
        assert!(!bare_explain.contains("REVIEW"));
    }

    /// §S15: a prerequisite sub-node's prompt must name its parent and tell
    /// the model to stay out of the parent's own scope — the fix for content
    /// observed drifting into the parent's material (2026-08-17 live report).
    /// A node with no parent must not see this instruction at all.
    #[test]
    fn sub_node_is_told_to_stay_out_of_its_parents_scope() {
        let sub_node = MoveContext {
            item_title: "Defining and calling functions in Rust".into(),
            parent_title: Some("recursive functions in Rust".into()),
            ..Default::default()
        };
        let explain_sys = &generate_move_streamed(MoveType::Explain, &sub_node)[0].content;
        assert!(explain_sys.contains("PREREQUISITE STEP"));
        assert!(explain_sys.contains("recursive functions in Rust"));
        assert!(explain_sys.contains("Defining and calling functions in Rust"));

        let test_sys = &generate_move(AgentPolicy::L1, MoveType::Test, &sub_node)[0].content;
        assert!(test_sys.contains("PREREQUISITE STEP"));

        let bare = MoveContext::default();
        let bare_explain = &generate_move_streamed(MoveType::Explain, &bare)[0].content;
        assert!(!bare_explain.contains("PREREQUISITE STEP"));
    }

    /// General form of the same fix, on EVERY node regardless of tree shape
    /// (not just prerequisite sub-nodes): `topic` must read as background/
    /// emphasis guidance, never as content to teach in this node. `plan` is
    /// exempt — its whole job is reasoning about the outline/topic as a
    /// structural whole, so the note would be noise there.
    #[test]
    fn topic_is_framed_as_emphasis_guidance_not_content_to_teach() {
        let ctx = MoveContext::default();
        let explain_sys = &generate_move_streamed(MoveType::Explain, &ctx)[0].content;
        assert!(explain_sys.contains("background context ONLY"));

        let test_sys = &generate_move(AgentPolicy::L1, MoveType::Test, &ctx)[0].content;
        assert!(test_sys.contains("background context ONLY"));

        let plan_sys = &generate_move_streamed(MoveType::Plan, &ctx)[0].content;
        assert!(!plan_sys.contains("background context ONLY"));
    }
}
