use super::Rubric;
use crate::ai::ChatMessage;
use crate::locale::{Locale, language_directive};

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
How the answer comes back:\n\
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
include the solution anywhere in the exercise_html. The student must produce it.\n\
CRITICAL: the student sees exercise_html and NOTHING else — never the rubric/criteria. \
Any concrete parameter your criteria will grade against (a specific number, range, \
step, name, scenario — anything more specific than the general objective) MUST be \
stated in plain language inside exercise_html itself. Never grade on a detail the \
exercise_html did not literally tell the student.\n\
CRITICAL: every objective you list must be demonstrable from the ONE task in \
exercise_html — never add an objective that would need a different task than the one \
you actually wrote (e.g. don't grade a sum-of-a-range objective against an exercise \
that only asked to print a range). An objective the exercise never asked for can never \
be satisfied, and the student would be stuck unable to pass no matter what they answer. \
Before writing criteria, work out the correct answer to the EXACT task you wrote — the \
real result for the specific numbers/options/scenario you chose, not a generic \
description of the objective — and write criteria consistent with THAT solution (the \
solution itself still never appears in exercise_html, per the reveal rule above). \
Criteria invented in parallel with the question, rather than derived from actually \
solving it, is how a rubric ends up grading a different answer than the one the \
exercise itself calls for.\n\
CRITICAL: put that worked-out solution in a \"reference_solution\" field of your JSON \
response (server-only — never sent to the student, never rendered in exercise_html). \
You already have to compute it to write correct criteria; this just keeps it instead of \
throwing it away. It becomes the answer key an automated grader checks the student's \
submission against, so state it as the concrete correct answer to the exact task \
(the specific number/option/derivation/output), not a restatement of the objective.\n\
CRITICAL: ground the exercise in what the student was actually taught, in a scenario \
the student hasn't already seen. Every objective must test a concept, fact, or \
procedure that appears in the content/context given to you above (what the node has \
taught so far, the concept/topic named for it, and/or any SOURCES provided) — never a \
different topic the student was never given. But do not just restate an example \
already used in that text with the same numbers/framing — put the concept in a scenario, \
numbers, or framing the student has NOT already seen, so answering takes applying the \
concept, not recalling or copying an example. The concept must be familiar; the \
specific instance should not be. (Objectives marked transfer=true push this further \
still — genuinely novel application, not just reworded numbers.)";

/// Cold-start objective proposal (§6.1/§S4) — precedes the outline call;
/// its (possibly user-edited) output anchors it.
pub fn propose_objective(topic: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(
            "You are a personal tutor doing the cold start of a living \
             curriculum. Read the learner's raw request and propose a \
             compact, single-sentence curriculum objective — concrete enough to \
             anchor every future decision, not a restatement of the topic. \
             Scope it precisely: anything the objective does not explicitly \
             name is implicitly out of scope, so do not pad it with a vague \
             general phrase just to be safe. A simple question gets a simple, \
             single-clause objective — do not expand it into a checklist of \
             sub-skills or related operations the learner didn't ask about; \
             this objective drives how many concepts get planned next, and a \
             stacked list of verbs plans one node per verb. Also give the \
             document a short \
             title (2-5 words, same language as the request) — a name the \
             learner will recognize in a list of their documents, not a \
             sentence. Respond ONLY with JSON: \
             {\"text\":\"...\",\"title\":\"...\"}.",
        ),
        ChatMessage::user(format!("Learner's request: {topic}")),
    ]
}

/// Runs on `Tier::Robust` (`engine::propose_outline`) — plans the FULL
/// outline in one call: prerequisite background the objective presupposes,
/// then the objective's own content, as ONE ordered JSON array (see
/// `engine::ProposedOutlineNode`'s doc comment for the exact contract: every
/// element but the last is a prerequisite, the last is the objective
/// itself). Unifies what used to be two separate calls (`outline` for the
/// objective's own flat breakdown, `propose_prerequisites` for a
/// separately-generated forest) whose independently-generated results then
/// had to be glued together in Rust — that graft was the actual bug behind
/// a live report (2026-08-19, "Funções Recursivas"): prerequisites nested
/// under an unrelated main-line item because the graft code had nothing
/// better to attach them to. A single call can place prerequisites as
/// siblings in the right order and give the objective its own titled
/// container the same way any other bundle of separable sub-skills gets
/// one, so the structure is correct by construction instead of reassembled
/// after the fact.
///
/// Carries the hard-won lessons of both prompts it replaces:
/// - Scope discipline for the objective's own decomposition (confirmed live,
///   2026-08-17: "termodinâmica para engenharia" collapsed to a single node
///   under an over-terse prompt) — judge scope from the objective, not a
///   fixed count.
/// - Inclusion bias for prerequisites (confirmed live, 2026-08-18: the old
///   "don't scaffold prerequisites the question already assumes the learner
///   has" guidance produced both a whole document's own topic — "the French
///   Revolution" — proposed as a prerequisite of ITSELF, and a topic that
///   should have decomposed — Big-O notation — collapsing to one node with
///   nothing pushed to prerequisites either) — the confirmation screen
///   (learn/review/skip, one click per item, prior mastery detected and
///   pre-filled) is the real backstop against runaway breadth, not prompt
///   restraint, so this asks for honest, inclusion-biased prerequisites
///   rather than terseness.
///
/// The first example is deliberately in English while the request will
/// often be Portuguese (this session's live testing so far skews pt-BR) —
/// the explicit "regardless of the language" instruction below exists
/// because an early probe run WITHOUT it leaked English into a Portuguese
/// request's output, the model matching the nearest example's language
/// instead of the live request's.
pub fn propose_outline(topic: &str, objective: &str) -> Vec<ChatMessage> {
    let request = |t: &str, o: &str| format!("Topic: {t}\nCurriculum objective: {o}");
    vec![
        ChatMessage::system(
            "You plan the FULL structure of a living curriculum for one \
             objective, as ONE tree: background prerequisites the learner \
             needs first, then the objective's own content — in that order, \
             in a single JSON array, most basic first. The LAST element of \
             the array is always the objective itself; every element before \
             it is a prerequisite the objective presupposes.\n\n\
             PREREQUISITES (every element except the last): background the \
             learner needs before the objective itself makes sense, never \
             the objective's own content. Err toward INCLUDING a \
             prerequisite whenever you are unsure whether the learner \
             already has it: this tree is a proposal, not a commitment — \
             the learner reviews it themselves and marks each item \
             skip/review/learn with one click before anything is generated, \
             and any item they already demonstrated elsewhere is shown to \
             them pre-filled and disabled. Assuming familiarity instead of \
             listing it costs the learner nothing to fix if you are wrong \
             the other way — silently leaving it out is the mistake, \
             because a gap the learner cannot see and cannot click away is \
             a gap that stays. The ONLY time there are NO prerequisites at \
             all (the array has just the one final element) is an \
             objective that presupposes nothing beyond basic \
             literacy/numeracy and everyday reasoning — that floor is low \
             and should be rare to actually hit; do not treat \"someone \
             asking this probably already knows X\" as a reason to omit X. \
             Never restate the objective's own topic as a prerequisite of \
             itself.\n\n\
             THE OBJECTIVE (the last element): titled for the topic itself, \
             at the granularity a good textbook's own table of contents \
             would use for it — a chapter or section, not the whole field. \
             Judge the SCOPE from the objective, not a fixed count: a \
             narrow, self-contained question needs no decomposition at all \
             (empty children) — do not pad it with sections its own \
             phrasing doesn't ask for. A broader subject gets one child per \
             genuinely distinct sub-topic a table of contents would list as \
             its own section, most basic first; under-planning a broad \
             objective down to no decomposition is as much a mistake as \
             over-planning a narrow one. Compress a little more than a real \
             textbook would, though: this app's real depth grows from the \
             learner's own follow-up questions and the outline-revision \
             move that fires as they read, so this initial plan does not \
             need to enumerate every sub-case, formula variant, or \
             worked-example category — those emerge later, from questions, \
             not from you now.\n\n\
             DECOMPOSITION (applies to any node, prerequisite or objective, \
             at any depth): give a node its own children only when it is \
             genuinely a bundle of separable sub-skills that each need to \
             be demonstrated on their own — apply this same test \
             recursively at every level. A node that is already atomic gets \
             no children (an empty array, not omitted).\n\n\
             Write every title in the SAME language as the request below, \
             regardless of what language these instructions or the \
             examples use. Respond ONLY with a JSON array, each element \
             shaped {\"title\":\"...\",\"children\":[...]} (children is the \
             same shape, recursively, may be an empty array). No comments, \
             no markdown, no prose outside the JSON.",
        ),
        ChatMessage::user(request(
            "how does binary search work",
            "Understand how binary search finds a target in a sorted array and implement it correctly",
        )),
        ChatMessage::assistant(r#"[{"title":"Binary search over a sorted array","children":[]}]"#),
        ChatMessage::user(request(
            "cálculo integral",
            "Aprender os fundamentos de cálculo integral: integrais indefinidas, definidas e o teorema fundamental do cálculo",
        )),
        ChatMessage::assistant(
            r#"[{"title":"Álgebra básica","children":[]},
                {"title":"Limites","children":[]},
                {"title":"Derivadas","children":[]},
                {"title":"Integração","children":[
                    {"title":"Antiderivadas e a integral indefinida","children":[]},
                    {"title":"Técnicas de integração: substituição","children":[]},
                    {"title":"Técnicas de integração: integração por partes","children":[]},
                    {"title":"Somas de Riemann e a integral definida","children":[]},
                    {"title":"O teorema fundamental do cálculo","children":[]},
                    {"title":"Aplicações da integral: área entre curvas e volume de sólidos de revolução","children":[]}
                ]}]"#,
        ),
        ChatMessage::user(request(
            "recursion in C",
            "Understand how recursive functions work in C and be able to write them correctly",
        )),
        ChatMessage::assistant(
            r#"[{"title":"C data types and variables","children":[]},
                {"title":"C functions","children":[
                    {"title":"Function definition and prototype","children":[]},
                    {"title":"Parameters and return values","children":[]},
                    {"title":"Calling functions","children":[]},
                    {"title":"Local vs global scope","children":[]}
                ]},
                {"title":"Recursion in C","children":[
                    {"title":"What is recursion in C","children":[]},
                    {"title":"The base case in recursive functions","children":[]},
                    {"title":"The recursive case in C functions","children":[]},
                    {"title":"The call stack and how recursion executes","children":[]},
                    {"title":"Syntax for recursive functions in C","children":[]},
                    {"title":"Common recursive patterns and pitfalls","children":[]}
                ]}]"#,
        ),
        ChatMessage::user(request(topic, objective)),
    ]
}

/// Derives a short catalog-search phrase for source acquisition (§11) from
/// the learner's own topic/objective — NOT a semantic search: the acquisition
/// backend matches against textbook titles, so a full question or sentence
/// (e.g. "how does binary search work?") reliably returns zero hits even for
/// subjects the catalog covers, while a short general subject phrase (e.g.
/// "python programming", "algebra") reliably matches. Confirmed live against
/// the OpenStax catalog API across a dozen topics before writing this prompt.
pub fn search_subject(topic: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(
            "Name the general SUBJECT or textbook this request belongs to, the \
             way it would appear in a library catalog or a textbook's own \
             title — 2 to 4 words, no punctuation, no question, no specific \
             detail from the request itself. Examples: a question about \
             appending to a list in Python -> \"python programming\"; a \
             question about derivatives -> \"calculus\"; a question about \
             supply and demand -> \"economics\". Respond with ONLY the \
             subject phrase, nothing else.",
        ),
        ChatMessage::user(format!("Request: {topic}")),
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
/// leak the answer to that upcoming problem. The system message must read as
/// pure pedagogy — no internal terminology ("remediation", a spec citation,
/// "attempt N") in text the model might echo back into its own output: seen
/// live, 2026-08-15, the model opened its explanation with a heading quoting
/// this prompt's own former "(§8.2)" back at the learner. `chapter_html` (the
/// node's own already-taught content, §4.3) is what lets the model tell
/// "already explained, don't repeat" apart from "genuinely missing, explain
/// it now" — without it this call had no way to know what the learner had
/// already read, and would routinely re-teach the whole concept from scratch.
pub fn remediation(
    item_title: &str,
    chapter_html: &str,
    exercise_html: &str,
    answer: &str,
    unmet_summary: &str,
    attempt: u32,
    locale: Locale,
) -> Vec<ChatMessage> {
    let lang = language_directive(locale);
    vec![
        ChatMessage::system(format!(
            "You are a personal tutor. The student just got a check wrong. Write \
             ONLY the tutor's own next words, continuing the conversation \
             naturally — never a heading, label, or aside naming this moment \
             as \"remediation\", a \"session\", an \"attempt\", or any other \
             internal bookkeeping term; the learner sees a tutor talking to \
             them, never the machinery behind it.\n\n\
             Walk through a worked, step-by-step solution OF THE EXACT PROBLEM \
             THEY JUST MISSED, naming the misconception their answer suggests. \
             \"Chapter content already taught\" below is what the learner \
             already read — do NOT re-explain anything it already covers; stay \
             tightly focused on correcting THIS mistake. Only add new \
             conceptual explanation for the specific point behind the \
             student's error if the chapter did not cover it, or covered it \
             too thinly to account for this particular mistake. The higher \
             the attempt number below, the more concrete and closely \
             scaffolded the walkthrough should be. Do NOT pose a new problem \
             here and do NOT state the answer to any future exercise — a \
             separate practice problem follows.\n\n\
             {lang}\n\n{PROSE_HTML_CONTRACT}"
        )),
        ChatMessage::user(format!(
            "Concept: {item_title}\nChapter content already taught: {chapter_html}\n\
             Exercise: {exercise_html}\nStudent's answer: {answer}\n\
             Objectives not demonstrated:\n{unmet_summary}\nAttempt number: {attempt}"
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
    locale: Locale,
) -> Vec<ChatMessage> {
    let anchor_block = reading_context_block(reading_context);
    let lang = language_directive(locale);
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
             {lang}\n\n{PROSE_HTML_CONTRACT}"
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
    locale: Locale,
) -> Vec<ChatMessage> {
    let anchor_block = reading_context_block(reading_context);
    let lang = language_directive(locale);
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
             {lang}\n\n{PROSE_HTML_CONTRACT}"
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
    locale: Locale,
) -> Vec<ChatMessage> {
    let lang = language_directive(locale);
    vec![
        ChatMessage::system(format!(
            "Generate a NEW practice problem AND its grading rubric TOGETHER, \
             for a student who just failed a check and was given a worked example. \
             It must test the SAME objective but be a DIFFERENT instance (new \
             numbers/scenario), similar to the failed one — attempt {attempt}: the \
             higher it is, the closer to the worked example (more scaffolding). \
             Respond ONLY with JSON: \
             {{\"exercise_html\":\"<form>...</form>\",\"reference_solution\":\"...\",\
             \"objectives\":[{{\"id\":\"o1\",\
             \"kind\":\"knowledge|application|synthesis\",\"description\":\"...\",\
             \"criteria\":\"what counts as demonstrated\",\"transfer\":true|false}}]}}.\n\n\
             {lang}\n\n{EXERCISE_HTML_CONTRACT}"
        )),
        ChatMessage::user(format!(
            "Concept: {item_title}\nThe problem the student just failed:\n{failed_exercise}\
             {}",
            sources_block(sources)
        )),
    ]
}

/// `reference_solution` (S16) is the answer key computed at generation time
/// (`EXERCISE_HTML_CONTRACT`) — grading against a concrete solution, not
/// just the rubric's prose criteria, is the leniency fix. Deliberately
/// instructed never to leak it back through feedback: the student can see
/// the feedback text (remediation may show the same exercise again), so
/// restating the reference solution there would hand out the answer through
/// the one channel meant to grade it.
pub fn grading(
    rubric: &Rubric,
    exercise_html: &str,
    answer: &str,
    reference_solution: &str,
    locale: Locale,
) -> Vec<ChatMessage> {
    let rubric_json = serde_json::to_string(rubric).unwrap_or_default();
    let lang = language_directive(locale);
    let solution_block = if reference_solution.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\nReference solution (for YOUR evaluation only — the student never sees \
             this; do NOT quote, paraphrase, or otherwise reveal it in \"feedback\", \
             even to explain a wrong answer): {reference_solution}"
        )
    };
    vec![
        ChatMessage::system(format!(
            "Grade the student's answer AGAINST the locked rubric, without leniency \
             (§8). When a reference solution is given below, use it as the answer key — \
             a submission that doesn't match it on the graded specifics fails that \
             objective regardless of how confident or well-written it sounds; do not \
             award credit for reasoning that merely sounds plausible. For each \
             objective give the grade {{not_demonstrated|partial|demonstrated}} and \
             short feedback that never restates the reference solution itself. \
             Respond ONLY with JSON: \
             {{\"grades\":[{{\"objective_id\":\"o1\",\"grade\":\"...\",\
             \"feedback\":\"...\"}}]}}.\n\n{lang}"
        )),
        ChatMessage::user(format!(
            "Rubric: {rubric_json}\nExercise: {exercise_html}{solution_block}\n\
             Student's answer: {answer}"
        )),
    ]
}
