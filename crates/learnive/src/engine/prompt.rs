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

/// Runs on `Tier::Robust` (`engine::propose_outline`) — proposes the initial
/// READING LIST for an objective (S27e, PLAN.md §27, replacing the
/// concept-outline-by-prerequisite prompt this function used to build — see
/// git history if that prompt's exact wording is ever needed again): real
/// books and articles, foundational-first, ordered so that array position
/// alone carries the prerequisite relationship (PLAN.md §27 decision 3 —
/// there is no separate "prerequisite of concept" category anymore, and no
/// invented concept titles at all). The schema asked for is exactly
/// `source::ProposedItem`'s own shape (see `parse::outline_tree`'s doc
/// comment for why one schema, not a wrapping one) — `{"title":..., \
/// "authors":[...], "year":..., "edition":..., "identifier":..., \
/// "kind":"book"|"article"}` per item, nothing else. No `children`, no
/// chapters: this call must never guess a book's internal structure — that
/// can only be checked against the real PDF table of contents, which is
/// S27g's job, later, after the library actually has the file (same
/// anti-speculation reasoning §14 already applies to prefetch).
///
/// `rejected` names titles a prior round of this same cold start proposed
/// that then failed S27d's existence verification (bounded one-round retry,
/// `api::cold_start::propose_reading_list`) — empty on the first call. When
/// non-empty, the prompt tells the model plainly which titles did not verify
/// and asks for a real substitute covering the same role in the list, not a
/// cosmetic rename of the same unverifiable work.
pub fn propose_outline(topic: &str, objective: &str, rejected: &[String]) -> Vec<ChatMessage> {
    let request = |t: &str, o: &str| format!("Topic: {t}\nCurriculum objective: {o}");
    let mut system = String::from(
        "You propose the initial READING LIST for one learner's curriculum \
         objective: real, findable books and articles, never invented \
         concept titles — the list itself IS the curriculum plan, and \
         chapters/sections inside each work are discovered later, once the \
         actual book is in hand, not guessed now. Respond with ONE JSON \
         array, ordered foundational-first, most basic prerequisite work \
         first and the work(s) most directly covering the objective itself \
         last (a later step locks the LAST item as unskippable — the \
         'this is what was actually asked for' item — so order matters, not \
         just content).\n\n\
         Each array element is shaped EXACTLY like this, no other fields, \
         no children: {\"title\":\"...\",\"authors\":[\"Last, First\", ...], \
         \"year\":1234,\"edition\":\"...\" or null,\"identifier\":null,\
         \"kind\":\"book\"|\"article\"}. `authors` in \"Last, First\" order \
         when known; empty array if genuinely unknown (never invent a \
         name). `year`/`edition` null when unknown. `identifier` stays null \
         unless you have real, specific certainty of an ISBN/DOI/arXiv id — \
         guessing one is worse than omitting it, since a wrong identifier \
         is checked as if it were confirmed fact by the next step.\n\n\
         Err toward INCLUDING a foundational work whenever you are unsure \
         the learner already has it: this list is a proposal, not a \
         commitment — the learner reviews it themselves and marks each \
         item skip/review/learn with one click before anything happens, \
         and a work they already finished elsewhere is shown to them \
         pre-filled and disabled. Only real, specific, findable works: a \
         famous textbook or a well-known paper by title/author, never a \
         vague placeholder like \"an introductory algebra textbook\" — if \
         you cannot name a specific real work for a role, leave that role \
         out rather than inventing a plausible-sounding title, since every \
         item is checked against real library catalogs next and a made-up \
         title will simply fail that check.\n\n\
         Judge SCOPE from the objective, not a fixed count: a narrow, \
         self-contained question may need only the one work most directly \
         covering it; a broad subject needs real foundational works ahead \
         of it, most basic first. Write every title/author exactly as the \
         real work is titled (do not translate a real title into the \
         request's language) but keep this instruction's own language \
         irrelevant to your choice of works. Respond ONLY with the JSON \
         array — no comments, no markdown, no prose outside it.",
    );
    if !rejected.is_empty() {
        system.push_str(&format!(
            "\n\nThe following title(s) you proposed in an earlier attempt \
             at this SAME list did not verify against real library \
             catalogs (not found) — propose a real, different, specific \
             substitute covering the same role in the list, never the same \
             title again and never a placeholder: {}",
            rejected.join("; ")
        ));
    }
    vec![
        ChatMessage::system(system),
        ChatMessage::user(request(
            "how does binary search work",
            "Understand how binary search finds a target in a sorted array and implement it correctly",
        )),
        ChatMessage::assistant(
            r#"[{"title":"Introduction to Algorithms","authors":["Cormen, Thomas H.","Leiserson, Charles E.","Rivest, Ronald L.","Stein, Clifford"],"year":2009,"edition":"3rd","identifier":null,"kind":"book"}]"#,
        ),
        ChatMessage::user(request(
            "cálculo integral",
            "Aprender os fundamentos de cálculo integral: integrais indefinidas, definidas e o teorema fundamental do cálculo",
        )),
        ChatMessage::assistant(
            r#"[{"title":"Pré-Cálculo","authors":["Iezzi, Gelson","Murakami, Carlos"],"year":2013,"edition":"9","identifier":null,"kind":"book"},
                {"title":"Cálculo, Volume 1","authors":["Stewart, James"],"year":2015,"edition":"8","identifier":null,"kind":"book"}]"#,
        ),
        ChatMessage::user(request(
            "recursion in C",
            "Understand how recursive functions work in C and be able to write them correctly",
        )),
        ChatMessage::assistant(
            r#"[{"title":"The C Programming Language","authors":["Kernighan, Brian W.","Ritchie, Dennis M."],"year":1988,"edition":"2nd","identifier":null,"kind":"book"}]"#,
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
            "Name a specific, real book or article that covers this request — \
             respond with its exact TITLE, the way it would appear on the \
             book's own cover or a paper's own header, not a general subject \
             area. It must be a real, existing work you have genuine \
             knowledge of, never an invented or approximate title. Examples: \
             a question about appending to a list in Python -> \"Learning \
             Python\"; a question about derivatives -> \"Calculus\"; a \
             question about supply and demand -> \"Principles of \
             Economics\". Respond with ONLY the title, nothing else (no \
             author, no quotes, no year, no extra commentary).",
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
