use super::Rubric;
use crate::ai::ChatMessage;

/// HTML contract for **sanitized** surfaces (prose, remediation): this content
/// is inserted into the app origin and passes through the client `sanitizeHtml`
/// (`assets/core.js`), which silently REMOVES anything outside the contract.
/// Telling the model prevents it from generating something that vanishes on
/// render (§3.1/§4.4). **Keep in sync with the sanitizer** — the list of
/// blocked tags/attributes here mirrors the one there.
///
/// The interactive island (§4.4/§S11) is a SEPARATE addendum
/// ([`ISLAND_CONTRACT`]), not part of this constant, and must only be
/// appended where the promise is actually kept end to end: the
/// **streamed** path (`movement::prompt::generate_move_streamed`), where
/// `movement::IslandGate` gates the island's raw HTML out of the live SSE
/// `token` frames, injects a block id once it sees the closing tag, and
/// `api::block_frame` serves it through its own sandboxed iframe
/// (`assets/node.js`'s `hydrateIslands`), exactly like
/// `exercise_frame` already does for the exercise. Every other caller of
/// THIS constant (the **structured** JSON-envelope path for
/// profile/plan/other moves, the remediation explanation, a sub-node's
/// Q&A answer) has no gating and no hydration route — worse, for the
/// structured path specifically, asking the model to emit raw HTML/JS
/// inside a JSON string field risks breaking the JSON envelope itself
/// (escaping, truncation). So it stays out of the shared contract and is
/// opt-in per call site.
///
/// On the math rule: it deliberately does NOT forbid LaTeX. The model
/// writes math in LaTeX because that is what its training data is, so a
/// prohibition would be an *unenforced* request — unlike the HTML rules
/// above, which the sanitizer actually enforces — and it would cap notation
/// at what Unicode can express, a permanent quality ceiling for any real
/// mathematics. The server converts LaTeX to MathML at freeze time instead
/// (`learnive_core::render_math`). What the rule is for is fixing the
/// *form*: the converter can only find math that is delimited, and a bare
/// `\frac{1}{2}` mid-sentence is not safely recoverable. Same bargain
/// [`ISLAND_CONTRACT`] makes — the model commits to literal markers, the
/// server does the rest.
///
/// Written as one flat literal on purpose: this constant is interpolated
/// *into* other `format!` strings, so a `{PLACEHOLDER}` for a second
/// constant would never be expanded and would ship to the model verbatim
/// (`contract_tests` guards exactly that).
pub const PROSE_HTML_CONTRACT: &str = "\
HTML rules (the content is sanitized — anything that violates them disappears on render):\n\
- Use ONLY static semantic HTML: <h2>-<h4>, <p>, <ul>/<ol>/<li>, <table>/<tr>/\
<td>, <code>/<pre>, <blockquote>, <strong>/<em>, <div>, <span>, <a href> (only \
http(s), mailto: or #) and <img> (only src https: or data:image/).\n\
- Layout: shape the page with <div>/<span>, but ONLY with these class values \
(any other class is stripped on render): \"callout\" an aside worth pausing on; \
\"callout key\" the central idea of the section; \"callout warning\" a common \
mistake or misconception; \"columns\" a side-by-side comparison, holding two or \
three <div class=\"panel\">; \"panel\" one bounded unit; and on <span>: \"term\" a \
term you are defining, \"hl\" a phrase to make stand out. Nest and combine them \
freely — use them when the SHAPE of the content carries meaning (a contrast, a \
warning, a definition), not for decoration.\n\
- Keep the section a SEQUENCE of several top-level elements. Never wrap the \
whole thing in one outer <div>: the reader navigates, highlights and asks \
questions per top-level block, so a single wrapper collapses all of that into \
one undifferentiated blob.\n\
- NEVER use: <script>, <style>, <form>, <iframe>, <object>, <embed>, <link>, \
<meta>, <base>; nor on* attributes (onclick, onerror...), nor inline style \
attributes, nor javascript: URLs. All of that is discarded.\n\
- Do not generate id/data-* attributes — the server assigns them.\n\
- Write in HTML, not Markdown: no ###, **bold**, or ``` fences — use <h3>, \
<strong>, <pre><code>. Markdown reaches the reader as literal characters.\n\
- Math: write it in LaTeX and ALWAYS delimit it — $$…$$ or \\[…\\] for a \
formula on its own line, \\(…\\) or $…$ inline. The server typesets it. \
Undelimited LaTeX (a bare \\frac{1}{2} in a sentence) cannot be typeset and \
reaches the reader as raw source, so never write one. Prefer plain text or \
Unicode for anything that is not really math (H₂O, 25 °C, 3 × 4).";

/// Addendum to [`PROSE_HTML_CONTRACT`] for the ONE call site with a real
/// mechanism behind it (`movement::prompt::generate_move_streamed`) —
/// see that constant's doc comment for why every other caller must NOT
/// append this. Fixed, literal open/close markers (no other attributes
/// allowed on the opening tag) are what make mid-stream detection a
/// substring search instead of a real streaming HTML tokenizer — the
/// server injects the block id itself, so the model never needs to
/// invent one.
pub const ISLAND_CONTRACT: &str = "\
Need a dynamic visualization, simulation, or diagram mid-section — something \
a static image can't show? Wrap ONLY that part in an interactive island: the \
EXACT literal opening tag <figure data-interactive> (nothing else on it — no \
other attributes, no id), then any HTML/CSS/JS/SVG you want (it runs isolated \
in a sandbox iframe, same freedom as the exercise block), then the EXACT \
literal closing tag </figure>. It must be a top-level element, never nested \
inside a <p> or other block. Everything outside it still obeys the HTML \
rules above. Use this sparingly, only when it teaches better than prose — \
most content needs no island at all.";

/// Addendum to [`PROSE_HTML_CONTRACT`] for any call site that received
/// grounding passages (§10/§4.3) — appended only when `sources_block` is
/// non-empty, so a call with no grounding never sees it. The sole exception
/// to "do not generate id/data-* attributes" above: `<cite>` is how a
/// grounded claim points back at the passage it came from, and the client
/// sanitizer (`assets/core.js`) allowlists exactly this tag/attribute pair.
pub const CITE_CONTRACT: &str = "\
Grounding: you are given SOURCES below (real, openly-licensed passages). \
Ground your explanation in them and CITE where you rely on one: wrap the \
specific claim in <cite data-source-id=\"ID\" data-locator=\"LOC\">…</cite>, \
using ONLY an ID and LOC that appear verbatim in a SOURCES entry. Never \
invent a source id or locator; if nothing fits, write the sentence without a \
<cite>. This is the sole exception to the \"no data-* attributes\" rule. \
SOURCES are supporting context, not the assignment — if a passage is \
tangential to what this move is actually teaching or testing, ignore it \
rather than bending the content to fit it.";

/// Contract for the exercise block: it runs isolated in an `<iframe sandbox>`
/// (§4.4) — NO same-origin, cannot see the token or the page DOM — so it is
/// NOT sanitized and may use JS/CSS/SVG freely. In exchange it must return the
/// answer via the postMessage protocol (§8: structured artifact locked
/// together with the rubric).
pub const EXERCISE_HTML_CONTRACT: &str = "\
The exercise_html runs isolated in a sandbox iframe (allow-scripts, NO same-origin): \
it cannot see the token or the page. So it may use HTML/CSS/JS/SVG freely — including \
interactive visualizations, not just text fields.\n\
How the answer comes back (§8):\n\
- Simple case: wrap the answer fields in a <form> and include exactly ONE submit \
button, labeled in the SAME language as the content (e.g. \"Enviar resposta\"). \
The page captures the form submission and collects the fields automatically — do \
NOT add a second button.\n\
- Interactive/custom case: when the student finishes, call it yourself \
parent.postMessage({type:'learnive-answer', answer: JSON.stringify(ARTIFACT)}, '*'), \
where ARTIFACT is a structured JSON the rubric can grade.\n\
Never write the grading criteria inside the exercise_html (they are server-only).\n\
CRITICAL: never reveal the answer. Inputs start empty/unselected; do NOT \
pre-check, pre-fill, highlight, mark, or hint which option is correct, and do NOT \
include the solution anywhere in the exercise_html. The student must produce it.";

/// Cold-start objective proposal (§6.1/§S4) — precedes the outline call;
/// its (possibly user-edited) output anchors it.
pub fn propose_objective(topic: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(
            "You are a personal tutor doing the cold start of a living \
             curriculum (§6.1). Read the learner's raw request and propose a \
             compact, single-sentence curriculum objective — concrete enough to \
             anchor every future decision, not a restatement of the topic. \
             Scope it precisely: anything the objective does not explicitly \
             name is implicitly out of scope, so do not pad it with a vague \
             general phrase just to be safe. Also give the document a short \
             title (2-5 words, same language as the request) — a name the \
             learner will recognize in a list of their documents, not a \
             sentence. Respond ONLY with JSON: \
             {\"text\":\"...\",\"title\":\"...\"}.",
        ),
        ChatMessage::user(format!("Learner's request: {topic}")),
    ]
}

pub fn outline(topic: &str, objective: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(
            "You plan a learning curriculum. Given a topic and its confirmed \
             objective, respond ONLY with a JSON array of strings — the concept \
             titles, from most basic to advanced, atomic (§6), each one a \
             transitive prerequisite of the objective. No comments, no markdown.",
        ),
        ChatMessage::user(format!("Topic: {topic}\nCurriculum objective: {objective}")),
    ]
}

/// Formats retrieved passages into a user-message block the model can cite
/// from. Empty string when there is no grounding (index still filling, §14).
pub fn sources_block(sources: &str) -> String {
    if sources.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nSOURCES (cite by the exact id/locator shown):\n{sources}")
    }
}

/// Remediation EXPLANATION only (§8.2): a worked, step-by-step solution of the
/// problem the student just got wrong. It deliberately does NOT propose the
/// next problem (that is a separate, gradeable sandbox exercise) and must not
/// leak the answer to that upcoming problem.
pub fn remediation(
    item_title: &str,
    exercise_html: &str,
    answer: &str,
    unmet_summary: &str,
    attempt: u32,
) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(format!(
            "Remediation session (§8.2). The student got the check wrong. Explain \
             the concept IN THE CONTEXT of this exercise: walk through a worked, \
             step-by-step solution OF THE PROBLEM THEY JUST MISSED, naming the \
             misconception their answer suggests. This is attempt {attempt}: the \
             higher it is, the more concrete and closely scaffolded the walkthrough. \
             Do NOT pose a new problem here and do NOT state the answer to any \
             future exercise — a separate practice problem follows.\n\n\
             {PROSE_HTML_CONTRACT}"
        )),
        ChatMessage::user(format!(
            "Concept: {item_title}\nExercise: {exercise_html}\n\
             Student's answer: {answer}\nObjectives not demonstrated:\n{unmet_summary}"
        )),
    ]
}

/// Formats the caller's reading context (`api::reading_context`) — the
/// passage the question is about plus its neighbours, and the selected
/// span when there was a selection — as its own labelled block. Shared by
/// the three question-handling prompts so they describe the learner's
/// position in the document identically. Empty when the anchor didn't
/// resolve to a block.
fn reading_context_block(reading_context: Option<&str>) -> String {
    match reading_context {
        Some(t) => format!("\n\nWHERE THE LEARNER IS READING:\n{t}"),
        None => String::new(),
    }
}

/// A question asked mid-reading (§S6, §9). `reading_context` locates the
/// question in the document — the passage it was asked from, what
/// surrounds it, and the exact selected span if there was one; `None`
/// only when the anchor names no block. Same adversarial-not-flattering
/// instruction `confront` uses (§7), but scoped to only when the question
/// actually states a position — a plain clarifying question just gets a
/// clear answer.
pub fn answer_question(
    topic: &str,
    item_title: &str,
    node_context: &str,
    reading_context: Option<&str>,
    question: &str,
) -> Vec<ChatMessage> {
    let anchor_block = reading_context_block(reading_context);
    vec![
        ChatMessage::system(format!(
            "The learner is reading a node of a living document and asked a \
             question, in place. Your answer is not a reply in a chat — it is \
             spliced into the document itself, immediately after the passage \
             they asked about (§9). Write it as the explanation that passage \
             was missing for this learner: it has to read as part of the text \
             at that point. Do not open with pleasantries, do not restate the \
             question, do not sign off. Answer directly and honestly, grounded \
             in the node's content. If the question states a position or \
             disagreement, engage with it dialectically — build the strongest \
             honest counter-argument rather than flattering or simply validating \
             it (§7) — but only when there is actually a stance to engage with; a \
             plain clarifying question just gets a clear, honest answer. Do not \
             repeat the whole node; resolve the specific doubt.\n\n\
             {PROSE_HTML_CONTRACT}"
        )),
        ChatMessage::user(format!(
            "Topic: {topic}\nConcept of this node: {item_title}\n\
             Node content so far: {node_context}{anchor_block}\n\nQuestion: {question}"
        )),
    ]
}

/// Decides whether a question gets answered inline or spawns a sub-node
/// (§7/§S8).
pub fn decide_ask_response(
    topic: &str,
    item_title: &str,
    node_context: &str,
    reading_context: Option<&str>,
    question: &str,
) -> Vec<ChatMessage> {
    let anchor_block = reading_context_block(reading_context);
    vec![
        ChatMessage::system(
            "The learner asked a question while reading a node of a living \
             document (§9). Decide how to answer it: INLINE, a short direct \
             answer woven right where they asked (the default — most \
             questions), or SPAWN, a real new section of the document, when \
             the question opens up enough new ground that a full \
             self-contained elaboration serves the learner better than a \
             short reply — e.g. it asks about a related concept the node \
             doesn't cover, or wants a genuinely deeper treatment than a \
             paragraph can give. Do not spawn for a simple clarification, a \
             rephrase, or anything answerable in a few sentences.\n\
             Respond ONLY with JSON: {\"spawn\":true|false,\"title\":\"...\"} \
             — a short section title if spawning (same language as the \
             question), empty string otherwise.",
        ),
        ChatMessage::user(format!(
            "Topic: {topic}\nConcept of this node: {item_title}\n\
             Node content so far: {node_context}{anchor_block}\n\nQuestion: {question}"
        )),
    ]
}

/// A spawned sub-node's prose (§7/§S8): a self-contained elaboration, not
/// a reply — it must make sense read on its own, spliced permanently
/// into the document right after the paragraph that prompted it.
pub fn subnode_prose(
    topic: &str,
    sub_title: &str,
    parent_title: &str,
    node_context: &str,
    reading_context: Option<&str>,
    question: &str,
) -> Vec<ChatMessage> {
    let anchor_block = reading_context_block(reading_context);
    vec![
        ChatMessage::system(format!(
            "The learner asked a question that warrants a real new section of \
             the living document (§7/§9), not a short inline reply — it will be \
             spliced permanently into the document, right after the paragraph \
             where they asked. Write it as a self-contained elaboration titled \
             \"{sub_title}\": someone reading only this section, without the \
             surrounding conversation, should still follow it. Answer the \
             question directly; if it states a position or disagreement, engage \
             dialectically rather than flattering or simply validating it (§7).\n\n\
             {PROSE_HTML_CONTRACT}"
        )),
        ChatMessage::user(format!(
            "Topic: {topic}\nParent concept: {parent_title}\nNew section title: \
             {sub_title}\nParent node content so far: {node_context}{anchor_block}\n\n\
             Question: {question}"
        )),
    ]
}

/// A NEW practice problem for the remediation loop (§8.2): a fresh, gradeable
/// exercise + locked rubric (same JSON contract as [`exercise_rubric`]),
/// *similar* to the failed one with similarity growing per `attempt`
/// (scaffolding converges toward the worked example, then ramps back up). Runs
/// sandboxed and never reveals its answer.
pub fn remediation_exercise(
    item_title: &str,
    failed_exercise: &str,
    attempt: u32,
    sources: &str,
) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(format!(
            "Generate a NEW practice problem AND its grading rubric TOGETHER (§8), \
             for a student who just failed a check and was given a worked example. \
             It must test the SAME objective but be a DIFFERENT instance (new \
             numbers/scenario), similar to the failed one — attempt {attempt}: the \
             higher it is, the closer to the worked example (more scaffolding). \
             Respond ONLY with JSON: \
             {{\"exercise_html\":\"<form>...</form>\",\"objectives\":[{{\"id\":\"o1\",\
             \"kind\":\"knowledge|application|synthesis\",\"description\":\"...\",\
             \"criteria\":\"what counts as demonstrated\",\"transfer\":true|false}}]}}.\n\n\
             {EXERCISE_HTML_CONTRACT}"
        )),
        ChatMessage::user(format!(
            "Concept: {item_title}\nThe problem the student just failed:\n{failed_exercise}\
             {}",
            sources_block(sources)
        )),
    ]
}

pub fn grading(rubric: &Rubric, exercise_html: &str, answer: &str) -> Vec<ChatMessage> {
    let rubric_json = serde_json::to_string(rubric).unwrap_or_default();
    vec![
        ChatMessage::system(
            "Grade the student's answer AGAINST the locked rubric, without leniency \
             (§8). For each objective give the grade {not_demonstrated|partial|\
             demonstrated} and short feedback. Respond ONLY with JSON: \
             {\"grades\":[{\"objective_id\":\"o1\",\"grade\":\"...\",\
             \"feedback\":\"...\"}]}.",
        ),
        ChatMessage::user(format!(
            "Rubric: {rubric_json}\nExercise: {exercise_html}\nStudent's answer: {answer}"
        )),
    ]
}
