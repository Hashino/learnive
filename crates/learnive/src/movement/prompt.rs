use super::{AgentPolicy, MoveContext, MoveRecord, MoveRender, MoveType};
use crate::ai::ChatMessage;
use crate::engine::prompt::{
    CITE_CONTRACT, EXERCISE_HTML_CONTRACT, ISLAND_CONTRACT, PROSE_HTML_CONTRACT, sources_block,
};
use crate::locale::language_directive;

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
///
/// Fixed 2026-08-18: the original wording ("which aspects of THIS node's
/// own concept... to emphasize or frame") itself licensed narrowing a
/// prerequisite down to only the slice the overall topic needs — observed
/// live on top-level prerequisite roots ("Arrays", "Linked lists" in a
/// hash-map-collisions document), which have no `parent_title` and so
/// never get `scope_addendum`'s stronger warning below. The node's own
/// concept must be taught in full, general, standalone form regardless of
/// tree position; "overall topic" may motivate or pick examples, never
/// decide what gets left out.
fn topic_scope_note() -> &'static str {
    "\"Overall topic\" and \"Curriculum objective\" below are background/\
     motivational context ONLY — never a lens that narrows which aspects of \
     THIS node's own concept (\"Concept of this node\") get taught, and \
     never something to state or paraphrase to the learner directly (never \
     write a sentence like \"this matters for the curriculum's objective of \
     ...\" or \"by the end of this curriculum you'll...\" — the learner is \
     reading one atomic node, not a syllabus). Teach \"Concept of this \
     node\" in full: as a self-contained, general concept a reader could \
     learn and reuse on its own, regardless of the overall topic or where \
     the curriculum is ultimately headed — not just the slice of it the \
     overall topic happens to need. You may use \"Overall topic\" or \
     \"Curriculum objective\" silently, to motivate why the concept matters \
     or to pick an illustrative example, but never to decide what to omit \
     and never as text the learner sees named or referenced. \"Teach in \
     full\" still excludes anything listed under \"Not yet taught\" below, \
     when present — those are separate, later nodes' own material, not a \
     part of THIS node's concept just because they're related to it."
}

fn node_so_far_line(ctx: &MoveContext) -> String {
    format!(
        "\nNode content so far: {}",
        non_empty(tail_chars(&ctx.node_tail, 1500))
    )
}

/// Renders `MoveContext::later_titles` as a user-message line, paired with
/// "Context of what has been taught so far" — see that field's doc comment
/// for the live bug (a prerequisite node teaching a later, sibling node's
/// own material) this closes.
fn not_yet_taught_line(ctx: &MoveContext) -> String {
    if ctx.later_titles.is_empty() {
        String::new()
    } else {
        format!(
            "\nNot yet taught — belongs to a SEPARATE, LATER node, not this \
             one (do not teach, preview, or state its content here): {}",
            ctx.later_titles.join("; ")
        )
    }
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
fn candidate_types(ctx: &MoveContext) -> Vec<MoveType> {
    let mut types = vec![MoveType::Explain, MoveType::Ask, MoveType::Test];
    if ctx.profile.contains(crate::profile::HYPOTHESES_HEADER) {
        types.push(MoveType::Profile);
    }
    types.extend([
        MoveType::Confront,
        MoveType::Integrate,
        MoveType::Revisit,
        MoveType::Plan,
    ]);
    // Offered only when there is genuinely nothing to ground on — same
    // withhold-don't-ask-restraint pattern as `profile` above (a model told
    // "research is available" when grounding already exists has no reason
    // not to pick it "just in case", spending a whole move on nothing).
    if !ctx.research_attempted && ctx.grounding.trim().is_empty() {
        types.push(MoveType::Research);
    }
    types
}

fn menu(policy: AgentPolicy, ctx: &MoveContext) -> String {
    let types = candidate_types(ctx)
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(", ");
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

/// Prompt for the merged decide+generate call (§14 latency,
/// `decide_and_generate`, L1/L2 only) — see the module docs' "merged
/// decide+generate" note. Asks for a leading `<!--move: type-->` marker
/// (mirrors the existing trailing `<!--tactics: ...-->` sentinel, just
/// leading); if the chosen type renders streamed, the model keeps writing
/// that move's full content in the SAME response, under the same contract as
/// `generate_move_streamed`. If it renders structured, the model is told to
/// write NOTHING else — the caller makes a real `generate_move` call for
/// that content (see the module docs for why this call never asks a
/// structured type to stream its own JSON).
pub fn decide_and_generate(policy: AgentPolicy, ctx: &MoveContext) -> Vec<ChatMessage> {
    let candidates = candidate_types(ctx);
    let streamed_purposes = candidates
        .iter()
        .filter(|t| t.render() == MoveRender::Streamed)
        .map(|t| format!("- \"{t}\": {}", purpose(*t, ctx)))
        .collect::<Vec<_>>()
        .join("\n");
    let structured_names = candidates
        .iter()
        .filter(|t| t.render() == MoveRender::Structured)
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let cite = cite_addendum(&ctx.grounding);
    let lang = language_directive(ctx.locale);

    vec![
        ChatMessage::system(format!(
            "You are a personal tutor deciding what to do next in a living \
             document, and — for most outcomes — generating it in the SAME \
             response. {}\n\n\
             STEP 1: on the very first line, write EXACTLY one HTML comment \
             naming your choice: <!--move: type-->. No text before it.\n\n\
             STEP 2: what comes after depends on which type you chose:\n\
             - If you chose one of: {structured_names} — write NOTHING else. \
             Stop immediately after the marker; that move's content is \
             generated in a separate call.\n\
             - If you chose one of the other types, continue in the SAME \
             response with that move's full content, following the guidance \
             for whichever you picked:\n{streamed_purposes}\n\n\
             {}\n\n{}\n\n{lang}\n\n{PROSE_HTML_CONTRACT}\n\n{ISLAND_CONTRACT}\n\n{cite}\n\n\
             If you continued with content, after your HTML, on its own line, \
             append an HTML comment listing the tactic self-labels you used \
             (e.g. \"analogy\", \"worked-example\", \"interactive-visual\", \
             \"formal-first\"): <!--tactics: label-one, label-two-->. This \
             comment is invisible when rendered and is stripped before \
             storage — it is bookkeeping, not content.",
            menu(policy, ctx),
            continuity_note(),
            topic_scope_note(),
        )),
        ChatMessage::user(format!(
            "Overall topic: {}\nConcept of this node: {}\n\
             Context of what has been taught so far: {}{}\n\
             Curriculum objective: {}\nLearner profile: {}\n\
             Moves already in this node: {}{}{}",
            ctx.topic,
            ctx.item_title,
            non_empty(&ctx.outline_context),
            not_yet_taught_line(ctx),
            non_empty(&ctx.objective),
            non_empty(&ctx.profile),
            describe_prior(&ctx.prior_moves),
            sources_block(&ctx.grounding),
            node_so_far_line(ctx),
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
///
/// Fixed 2026-08-20: "compact definition-level refresher" alone let the
/// model read "review" as license to skip the node's own definition
/// entirely and open directly on how the overall topic extends it —
/// observed live on a "Binary search over a sorted array" review node in a
/// "first/last occurrence" document, which never once explained the
/// compare-to-middle/halve-the-interval mechanism and opened straight on
/// the first/last-occurrence variant. A sibling review node in the same
/// document ("Arrays and indexing") did state its own definition first,
/// showing the gap is content-dependent, not universal: the closer a
/// review node's own concept sits to the document's target topic, the
/// more likely "compact" gets read as "skip to what's relevant now." Same
/// family of bug as `topic_scope_note`'s fix (`18b2934`) — "compact"
/// must mean brief, not omitted.
fn review_addendum(review_mode: bool, move_type: MoveType) -> &'static str {
    if !review_mode {
        return "";
    }
    match move_type {
        MoveType::Test => {
            " The learner marked this as a REVIEW of something they already \
             believe they know: keep the check to just one or two short \
             exercises, not a full battery — but it must still probe THIS \
             node's own concept (\"Concept of this node\"), not skip \
             straight to how the overall topic extends or applies it."
        }
        _ => {
            " The learner marked this as a REVIEW of something they already \
             believe they know, not first-time learning: keep this to a \
             compact definition-level refresher, a few sentences, not a full \
             lesson — but it must still state THIS node's own definition. \
             \"Compact\" means brief, not skipped: do not open directly on \
             how the overall topic extends or applies this concept without \
             ever stating the concept itself."
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
    let base: String = match move_type {
        MoveType::Explain => "Write short, atomic explanatory prose for this concept. Do \
             not include an exercise or ask a question — those are separate \
             moves."
            .to_string(),
        MoveType::Confront => "Build the STRONGEST counter-argument to the learner's stated \
             position: be adversarial, not flattering. Distinguish \
             legitimate disagreement from a misconception — if it looks like \
             the latter, say so and explain why, gently but plainly."
            .to_string(),
        MoveType::Respond => respond_purpose(ctx),
        MoveType::Test => "This move MUST be graded: produce a comprehension check AND its \
             rubric, locked together. Every 'application' objective needs \
             at least one transfer=true item for a scenario not covered in the \
             text. If \"Context of what has been taught so far\" shows a prior \
             node's exercise, this check must probe something genuinely new — \
             not the same operation on cosmetically different numbers/names."
            .to_string(),
        MoveType::Profile => "Investigate ONE of the open hypotheses about the learner listed in \
             the profile context: ONE short conversational question about HOW \
             the learner thinks about or approaches this concept. If none is \
             listed (L2 may still pick this move off-menu), ask one short \
             question about how the learner would approach THIS concept \
             instead. This is NOT an exercise: never pose a task with a \
             correct answer to check ('write a function that…', 'calculate…', \
             'implement…'), never require code or a worked solution, never \
             emit a form — that is what the \"test\" move is for. graded MUST \
             be false. NEVER write about the absence of a hypothesis: \
             whatever you produce is what the learner reads."
            .to_string(),
        MoveType::Integrate => "Connect this node's concept to concept(s) the learner has \
             ALREADY been taught — named in \"Context of what has been \
             taught so far\" or earlier in \"Moves already in this node\". \
             Never integrate forward into \"Curriculum objective\" or a \
             concept not yet taught: do not preview, name, or state the \
             curriculum's final definition/destination, even to foreshadow \
             where the material is headed. If nothing has been taught yet to \
             integrate with, pick a different move instead of forcing one."
            .to_string(),
        MoveType::Plan => "Revise the outline non-destructively ONLY if you have a concrete \
             structural change to propose (reordering, adding, splitting, or \
             removing concepts) — write your rationale as short prose in \"html\" \
             and put the COMPLETE revised ordered list of outline item titles \
             (existing titles you keep, unchanged, plus the new/changed ones) in \
             the \"outline\" field. If you have nothing structural to propose \
             right now, just remark in \"html\" and leave \"outline\" empty — the \
             learner is never asked to approve a non-change."
            .to_string(),
        _ => "Produce this move's content, atomic and focused on its stated purpose.".to_string(),
    };
    let integration = if move_type == MoveType::Test {
        integration_addendum(&ctx.children_titles)
    } else {
        String::new()
    };
    format!(
        "{base}{integration}{}{}{}",
        review_addendum(ctx.review_mode, move_type),
        scope_addendum(ctx.parent_title.as_deref(), &ctx.item_title),
        remediation_addendum(ctx, move_type),
    )
}

/// §S17: `Respond`'s purpose text, ported verbatim from `engine::prompt`'s
/// old `answer_question`/`subnode_prose` system messages — branches on
/// `MoveContext::spawned_section_title` to tell the two `AskDecision`
/// outcomes apart, same distinction `api::reading::ask_question` already
/// made before this slice, just now expressed as prompt text instead of two
/// separate functions.
fn respond_purpose(ctx: &MoveContext) -> String {
    let question = ctx.question.as_deref().unwrap_or("(no question given)");
    match ctx.spawned_section_title.as_deref() {
        Some(sub_title) => format!(
            "The learner asked a question that warrants a real new section of \
             the living document (§7/§9), not a short inline reply — it will be \
             spliced permanently into the document, right after the paragraph \
             where they asked. Write it as a self-contained elaboration titled \
             \"{sub_title}\": someone reading only this section, without the \
             surrounding conversation, should still follow it. Answer the \
             question directly; if it states a position or disagreement, engage \
             dialectically rather than flattering or simply validating it (§7). \
             The learner's question: {question}"
        ),
        None => format!(
            "Answer the learner's question directly and completely — do not \
             repeat the whole node's content, resolve the specific doubt. If \
             it states a position or disagreement, engage dialectically rather \
             than flattering or simply validating it (§7); a plain clarifying \
             question just gets a clear, honest answer. The learner's \
             question: {question}"
        ),
    }
}

/// §8.2 remediation addendum for `Explain`/`Test`, the only two move types
/// `api::grading::answer` ever forces — `None` (empty string) for every
/// other type/caller, same withhold pattern as `review_addendum`/
/// `scope_addendum`. Ported from `engine::prompt::remediation`/
/// `remediation_exercise`'s old system messages: `Explain` gets the
/// worked-solution framing, `Test` gets the new-but-similar-instance
/// framing, both scaled by `attempt` (scaffolding converges toward the
/// worked example, then difficulty ramps back up, §8.2).
fn remediation_addendum(ctx: &MoveContext, move_type: MoveType) -> String {
    let (Some(failed), Some(unmet), Some(attempt)) = (
        ctx.failed_attempt.as_deref(),
        ctx.unmet_objectives.as_deref(),
        ctx.remediation_attempt,
    ) else {
        return String::new();
    };
    // Internal bookkeeping words ("remediation", "attempt N") below are for
    // the MODEL's own calibration only — never write them, or any other
    // aside naming this moment as a "session"/"attempt", into the HTML the
    // learner sees. Fixed live 2026-08-15 (pre-§S17, `engine::prompt::
    // remediation`): the model opened an explanation with a heading quoting
    // this prompt's own "(§8.2)" back at the learner — the instruction
    // moved here verbatim so the same failure can't reappear now that this
    // framing is shared prompt text instead of its own function.
    let no_echo = " Write ONLY the tutor's/exercise's own words, continuing \
                    naturally — never a heading, label, or aside naming this \
                    moment as \"remediation\", a \"session\", or an \
                    \"attempt\"; the learner sees a tutor/exercise, never the \
                    machinery behind it.";
    match move_type {
        MoveType::Explain => format!(
            " This is a REMEDIATION explanation (attempt {attempt}) — \
             internal framing only, see note below — not a first-pass \
             explain: walk through the worked solution to the SPECIFIC \
             problem the learner just missed, step by step, pointing at \
             exactly where their reasoning likely went wrong given what they \
             submitted. \"Node content so far\" below is what the learner \
             already read — do NOT re-explain anything it already covers; \
             stay tightly focused on correcting THIS mistake. Only add new \
             conceptual explanation for the specific point behind the \
             student's error if it wasn't covered there, or was covered too \
             thinly to account for this particular mistake. Higher attempt \
             numbers should be more heavily scaffolded — spell out more of \
             the steps directly rather than prompting the learner to find \
             them. Do NOT pose a new problem here and do NOT state the \
             answer to the exercise that follows — a separate practice \
             problem is generated as its own move.{no_echo}\n\
             The problem they missed:\n{failed}\n\
             Objectives not yet demonstrated:\n{unmet}"
        ),
        MoveType::Test => format!(
            " This is a REMEDIATION check (attempt {attempt}) — internal \
             framing only, see note below — not a first-pass test: it must \
             probe the SAME objective(s) as the problem the learner just \
             failed, but be a DIFFERENT instance (new numbers/scenario) — \
             never a copy. The higher the attempt number, the closer this \
             instance should sit to the worked example just explained (more \
             scaffolding); difficulty ramps back up only once the learner \
             demonstrates the concept.{no_echo}\n\
             The problem they missed:\n{failed}\n\
             Objectives not yet demonstrated:\n{unmet}"
        ),
        _ => String::new(),
    }
}

/// Prompt for the **streamed** path (`MoveRender::Streamed` types): pure
/// prose contract, no JSON envelope — flags are fixed by the caller from
/// the type, not emitted here. Tactics ride a trailing sentinel comment
/// (stripped server-side, never shown — see the module docs).
pub fn generate_move_streamed(move_type: MoveType, ctx: &MoveContext) -> Vec<ChatMessage> {
    let cite = cite_addendum(&ctx.grounding);
    let lang = language_directive(ctx.locale);
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
             for a living document. {}\n\n{}\n\n{scope_note}\n\n{lang}\n\n\
             {PROSE_HTML_CONTRACT}\n\n\
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
             Context of what has been taught so far: {}{}\n\
             Curriculum objective: {}\nLearner profile: {}{}{}{}",
            ctx.topic,
            ctx.item_title,
            non_empty(&ctx.outline_context),
            not_yet_taught_line(ctx),
            non_empty(&ctx.objective),
            non_empty(&ctx.profile),
            sources_block(&ctx.grounding),
            node_so_far_line(ctx),
            reading_selection_line(ctx),
        )),
    ]
}

/// §S17: renders `MoveContext::reading_context` (a `Respond` move's
/// selection/reading-line anchor) as a user-message line — mirrors
/// `engine::prompt`'s old `reading_context_block`, just folded into the
/// shared streamed-path builder instead of a separate `answer_question`
/// prompt function. Empty for every move type/caller with no reading
/// context set.
fn reading_selection_line(ctx: &MoveContext) -> String {
    match ctx.reading_context.as_deref() {
        Some(t) if !t.trim().is_empty() => format!("\nWhere the learner is reading: {t}"),
        _ => String::new(),
    }
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
    let lang = language_directive(ctx.locale);
    vec![
        ChatMessage::system(format!(
            "You are a personal tutor generating a \"{move_type}\" move \
             for a living document. {rung_note} {}\n\n{}\n\n{}\n\n{lang}\n\n\
             {contract}\n\n{cite}\n\n\
             Also emit the tactic self-labels you used (e.g. \"analogy\", \
             \"worked-example\", \"interactive-visual\", \"formal-first\") — \
             short kebab-case tags, in the SAME call (§7).\n\n\
             Respond ONLY with the Move JSON contract: \
             {{\"html\":\"...\",\"interactive\":true|false,\"graded\":true|\
             false,\"tactics\":[\"...\"],\"reference_solution\":\"...\",\
             \"objectives\":[{{\"id\":\"o1\",\
             \"kind\":\"knowledge|application|synthesis\",\"description\":\
             \"...\",\"criteria\":\"...\",\"transfer\":true|false}}],\
             \"outline\":[\"...\"]}}. Omit \"objectives\" (or leave it empty) \
             when graded=false. Omit \"outline\" (or leave it empty) for every \
             move type except \"plan\" with a concrete structural change. \
             Omit \"reference_solution\" when graded=false; when graded=true \
             (a \"test\" move) it is REQUIRED — the worked-out correct answer \
             to the exact task in html, server-only, never shown to the \
             student (see the exercise contract below for why).",
            purpose(move_type, ctx),
            continuity_note(),
            topic_scope_note()
        )),
        ChatMessage::user(format!(
            "Overall topic: {}\nConcept of this node: {}\n\
             Context of what has been taught so far: {}{}\n\
             Curriculum objective: {}\nLearner profile: {}{}{}",
            ctx.topic,
            ctx.item_title,
            non_empty(&ctx.outline_context),
            not_yet_taught_line(ctx),
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
        assert!(explain_sys.contains("own definition"));

        let test_sys = &generate_move(AgentPolicy::L1, MoveType::Test, &review)[0].content;
        assert!(test_sys.contains("REVIEW"));
        assert!(test_sys.contains("MUST be graded"));
        assert!(test_sys.contains("own concept"));

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
        assert!(explain_sys.contains("background/motivational context ONLY"));

        let test_sys = &generate_move(AgentPolicy::L1, MoveType::Test, &ctx)[0].content;
        assert!(test_sys.contains("background/motivational context ONLY"));

        let plan_sys = &generate_move_streamed(MoveType::Plan, &ctx)[0].content;
        assert!(!plan_sys.contains("background/motivational context ONLY"));
    }

    /// Live report (2026-08-17, `hidc0ayawb`): a `profile` move wrote a full
    /// ungradable coding task ("Please define a recursive function...") as
    /// plain prose instead of a short conversational question — the old
    /// "targeted mini-check" wording read as license to pose a task. The
    /// prompt must rule that out explicitly.
    #[test]
    fn profile_move_is_told_it_is_not_an_exercise() {
        let ctx = MoveContext::default();
        let profile_sys = &generate_move(AgentPolicy::L1, MoveType::Profile, &ctx)[0].content;
        assert!(profile_sys.contains("NOT an exercise"));
        assert!(profile_sys.contains("graded MUST be false"));
    }

    /// Live report (2026-08-20): a document requested in pt-BR, with the
    /// outline/objective correctly in pt-BR, still drifted into English
    /// mid-document because no move-generation prompt carried ANY language
    /// instruction. Every content-producing prompt must now carry the
    /// `Locale`-derived directive, on both the merged decide+generate path
    /// and the two standalone paths, for both locales.
    #[test]
    fn every_content_prompt_carries_the_locale_directive() {
        use crate::locale::Locale;

        let en = MoveContext::default();
        let pt = MoveContext {
            locale: Locale::PtBr,
            ..Default::default()
        };

        let decide_gen_en = &decide_and_generate(AgentPolicy::L1, &en)[0].content;
        let decide_gen_pt = &decide_and_generate(AgentPolicy::L1, &pt)[0].content;
        assert!(decide_gen_en.contains("in English"));
        assert!(decide_gen_pt.contains("Brazilian Portuguese"));

        let streamed_en = &generate_move_streamed(MoveType::Explain, &en)[0].content;
        let streamed_pt = &generate_move_streamed(MoveType::Explain, &pt)[0].content;
        assert!(streamed_en.contains("in English"));
        assert!(streamed_pt.contains("Brazilian Portuguese"));

        let structured_en = &generate_move(AgentPolicy::L1, MoveType::Test, &en)[0].content;
        let structured_pt = &generate_move(AgentPolicy::L1, MoveType::Test, &pt)[0].content;
        assert!(structured_en.contains("in English"));
        assert!(structured_pt.contains("Brazilian Portuguese"));
    }

    /// §S17: `Respond` is Rust-forced (`/ask`) — it must never appear in a
    /// menu a model can pick from, on either rung that has a real menu.
    #[test]
    fn respond_never_offered_in_the_menu() {
        let ctx = MoveContext::default();
        assert!(!menu_text(&ctx).contains("respond"));
        let l2 = decide_move(AgentPolicy::L2, &ctx)[0].content.clone();
        assert!(!l2.contains("respond"));
    }

    /// §S17: the two `AskDecision` outcomes must produce distinguishable
    /// prompt text — inline answers stay a direct reply, a spawn gets the
    /// "self-contained elaboration titled X" framing carrying the new
    /// section's own title, not the parent node's.
    #[test]
    fn respond_purpose_distinguishes_inline_from_spawn() {
        let inline = MoveContext {
            question: Some("why does this converge?".into()),
            ..Default::default()
        };
        let sys = &generate_move_streamed(MoveType::Respond, &inline)[0].content;
        assert!(sys.contains("why does this converge?"));
        assert!(!sys.contains("self-contained elaboration"));

        let spawn = MoveContext {
            question: Some("why does this converge?".into()),
            spawned_section_title: Some("Convergence criteria".into()),
            ..Default::default()
        };
        let sys = &generate_move_streamed(MoveType::Respond, &spawn)[0].content;
        assert!(sys.contains("self-contained elaboration"));
        assert!(sys.contains("Convergence criteria"));
    }

    /// §S17: a `Respond` move's user message carries the reading-context
    /// anchor when the caller set one, and stays silent when it didn't —
    /// same optional-context contract every other `MoveContext` field uses.
    #[test]
    fn respond_user_message_carries_reading_context_when_present() {
        let with_ctx = MoveContext {
            question: Some("q".into()),
            reading_context: Some("the paragraph about limits".into()),
            ..Default::default()
        };
        let user = &generate_move_streamed(MoveType::Respond, &with_ctx)[1].content;
        assert!(user.contains("the paragraph about limits"));

        let without = MoveContext {
            question: Some("q".into()),
            ..Default::default()
        };
        let user = &generate_move_streamed(MoveType::Respond, &without)[1].content;
        assert!(!user.contains("Where the learner is reading"));
    }

    /// §8.2: `remediation_addendum` only fires when the caller set the full
    /// remediation trio (`failed_attempt`/`unmet_objectives`/
    /// `remediation_attempt`) — a normal, non-remediation `Explain`/`Test`
    /// move must see none of this framing. Also guards the 2026-08-15 live
    /// fix ported into this addendum: the internal words "remediation"/
    /// "attempt" must never reach the learner unescorted by the no-echo
    /// instruction.
    #[test]
    fn remediation_addendum_only_fires_with_full_context_and_never_bare() {
        let bare = MoveContext::default();
        let sys = &generate_move_streamed(MoveType::Explain, &bare)[0].content;
        assert!(!sys.to_lowercase().contains("remediation"));

        let remediating = MoveContext {
            failed_attempt: Some("Exercise: 2+2=?\nStudent's answer: 5".into()),
            unmet_objectives: Some("- o1: arithmetic wrong".into()),
            remediation_attempt: Some(2),
            ..Default::default()
        };
        let explain_sys = &generate_move_streamed(MoveType::Explain, &remediating)[0].content;
        assert!(explain_sys.contains("2+2=?"));
        assert!(explain_sys.contains("never a heading, label, or aside"));

        let test_sys = &generate_move(AgentPolicy::L1, MoveType::Test, &remediating)[0].content;
        assert!(test_sys.contains("arithmetic wrong"));
        assert!(test_sys.contains("DIFFERENT instance"));
    }
}
