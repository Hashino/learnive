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
//! Consumed by the loop endpoints (Task #5b); hence the temporary `allow`.
#![allow(dead_code)]

use futures_util::StreamExt;
use rand::{Rng, distributions::Alphanumeric};
use serde::{Deserialize, Serialize};

use learnive_core::{Node, ObjectiveType, ensure_block_ids};

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
    /// Set for a sub-node spawned from a question asked inside another node
    /// (§7/§S8): the id of the node it was spawned from. `None` for every
    /// item on the document's main line. A sub-node is never a prerequisite
    /// of anything (asymmetric scoping — §S8) and carries none of its own in
    /// this slice; it's excluded from the outline sidebar
    /// (`api::outline_view`) and from "next available" advance, both filtered
    /// on `parent_id.is_none()`.
    #[serde(default)]
    pub parent_id: Option<String>,
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
    #[serde(default)]
    pub non_goals: Vec<String>,
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
        });
        prev_id = Some(id);
    }
    items
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
    collect(
        ai,
        Tier::Robust,
        prompt::remediation(item_title, exercise_html, answer, &unmet_summary, attempt),
    )
    .await
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
    anchor_text: Option<&str>,
    question: &str,
) -> Result<String, EngineError> {
    collect(
        ai,
        Tier::Robust,
        prompt::answer_question(topic, item_title, node_context, anchor_text, question),
    )
    .await
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
    anchor_text: Option<&str>,
    question: &str,
) -> Result<AskDecision, EngineError> {
    let messages =
        prompt::decide_ask_response(topic, item_title, node_context, anchor_text, question);
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
    anchor_text: Option<&str>,
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
            anchor_text,
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

/// Assembles a dialect node from the generated prose and the exercise (§4.2/§4.3).
/// The server assigns the IDs (blocks, exercise, rubric).
pub fn assemble_node(
    doc_id: &str,
    node_id: &str,
    prose_inner_html: &str,
    exercise_html: &str,
    exercise_id: &str,
    rubric_id: &str,
) -> Result<Node, EngineError> {
    let blocks = ensure_block_ids(prose_inner_html, &format!("{node_id}-b"));
    let form = ensure_form_ids(exercise_html, exercise_id, rubric_id);
    let article = format!(
        "<article data-node-id=\"{node_id}\" data-doc-id=\"{doc_id}\">\n  \
         <section data-layer=\"content\">\n{blocks}\n{form}\n  </section>\n  \
         <section data-layer=\"interaction\"></section>\n</article>"
    );
    Node::parse(&article).map_err(|e| EngineError::Parse(e.to_string()))
}

/// Assembles a **content-only** node — no exercise/form (§S8 sub-nodes,
/// scoped to this slice as prose-only elaborations, never gated). `Node`'s
/// `content.exercise` parses to `None` when there's no `<form
/// data-exercise-id>` in the content section, so this is a strict subset of
/// `assemble_node`, not a different dialect.
pub fn assemble_content_node(
    doc_id: &str,
    node_id: &str,
    prose_inner_html: &str,
) -> Result<Node, EngineError> {
    let blocks = ensure_block_ids(prose_inner_html, &format!("{node_id}-b"));
    let article = format!(
        "<article data-node-id=\"{node_id}\" data-doc-id=\"{doc_id}\">\n  \
         <section data-layer=\"content\">\n{blocks}\n  </section>\n  \
         <section data-layer=\"interaction\"></section>\n</article>"
    );
    Node::parse(&article).map_err(|e| EngineError::Parse(e.to_string()))
}

/// Collects a full stream into a String (for non-streamed calls). Also reused
/// by `movement.rs` (S2) — `decide_move`/`generate_move` are non-streamed the
/// same way outline/exercise/grading are.
pub(crate) async fn collect(
    ai: &Ai,
    tier: Tier,
    messages: Vec<ChatMessage>,
) -> Result<String, EngineError> {
    let mut stream = ai.stream(tier, messages).await?;
    let mut out = String::new();
    while let Some(token) = stream.next().await {
        out.push_str(&token?);
    }
    Ok(out)
}

/// Injects `data-exercise-id`/`data-rubric-id` into the first `<form>` (wrapping
/// the exercise in one if there is none).
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
                r#" data-exercise-id="{exercise_id}" data-rubric-id="{rubric_id}""#
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
/// in `index.html`). `srcdoc` documents inherit the parent page's CSP — not
/// an oversight, a browser rule — so the moment the app's own CSP drops
/// `'unsafe-inline'` (a planned hardening), the inline harness `<script>`
/// here would stop running too. Serving this as a real HTTP response lets
/// the frame carry its **own** CSP header (via `security::guard`'s
/// insert-only-if-absent CSP, see that module), independent of the app
/// origin's policy. Isolation still comes from `sandbox="allow-scripts"`
/// with no `allow-same-origin` on the `<iframe>` (§3.1/§4.4) — this frame's
/// own CSP is a second, orthogonal layer, not the isolation boundary itself.
pub fn render_sandbox_frame(html: &str, theme: &str, graded: bool) -> String {
    let theme = if theme == "light" { "light" } else { "dark" };
    let submit_harness = if graded {
        r#"function collect(){var f=document.querySelector('form');var o={};if(f){new FormData(f).forEach(function(v,k){o[k]=v;});}else{var t=document.querySelector('textarea,input');if(t)o.answer=t.value;}return o;}
function send(){parent.postMessage({type:'learnive-answer',answer:JSON.stringify(collect())},'*');}
var form=document.querySelector('form');if(form)form.setAttribute('novalidate','');
document.addEventListener('submit',function(e){e.preventDefault();send();});
var sb=document.querySelector('button[type=submit],input[type=submit]');
if(!sb){var bs=document.querySelectorAll('button');if(bs.length===1&&(form||document.querySelector('input,textarea,select')))sb=bs[0];}
if(sb){sb.addEventListener('click',function(e){e.preventDefault();send();});}
else if(!document.querySelector('button,input[type=submit],input[type=image]')){var p=document.createElement('p');var b=document.createElement('button');b.type='button';b.textContent='Submit answer';b.addEventListener('click',function(e){e.preventDefault();send();});p.appendChild(b);document.body.appendChild(p);}"#
    } else {
        ""
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

// ---------------------------------------------------------------------------
// Prompts (§6, §8). In English — the app content language.
// ---------------------------------------------------------------------------

pub mod prompt {
    use super::Rubric;
    use crate::ai::ChatMessage;

    /// HTML contract for **sanitized** surfaces (prose, remediation): this content
    /// is inserted into the app origin and passes through the client `sanitizeHtml`
    /// (`assets/index.html`), which silently REMOVES anything outside the contract.
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
    /// (`assets/index.html`'s `hydrateIslands`), exactly like
    /// `exercise_frame` already does for the exercise. Every other caller of
    /// THIS constant (the **structured** JSON-envelope path for
    /// profile/plan/other moves, the remediation explanation, a sub-node's
    /// Q&A answer) has no gating and no hydration route — worse, for the
    /// structured path specifically, asking the model to emit raw HTML/JS
    /// inside a JSON string field risks breaking the JSON envelope itself
    /// (escaping, truncation). So it stays out of the shared contract and is
    /// opt-in per call site.
    pub const PROSE_HTML_CONTRACT: &str = "\
HTML rules (the content is sanitized — anything that violates them disappears on render):\n\
- Use ONLY static semantic HTML: <h2>-<h4>, <p>, <ul>/<ol>/<li>, <table>/<tr>/\
<td>, <code>/<pre>, <blockquote>, <strong>/<em>, <a href> (only http(s), mailto: \
or #) and <img> (only src https: or data:image/).\n\
- NEVER use: <script>, <style>, <form>, <iframe>, <object>, <embed>, <link>, \
<meta>, <base>; nor on* attributes (onclick, onerror...), nor inline style \
attributes, nor javascript: URLs. All of that is discarded.\n\
- Do not generate id/data-* attributes — the server assigns them.";

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
                 anchor every future decision, not a restatement of the topic. If \
                 the request has an obvious scope boundary (what it does NOT cover), \
                 list 1-3 short non-goals; otherwise leave non_goals empty. Respond \
                 ONLY with JSON: {\"text\":\"...\",\"non_goals\":[\"...\"]}.",
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

    /// Citation contract (§4.3/§11) — appended to the prose system prompt only
    /// when grounding sources are present. `<cite>` is the one dialect tag the
    /// client sanitizer lets through with `data-*` (the server does not reassign
    /// it — the model must use the exact ids/locators it was given).
    pub const CITE_CONTRACT: &str = "\
Grounding: you are given SOURCES below (real, openly-licensed passages). Ground \
your explanation in them and CITE where you rely on one: wrap the specific claim \
in <cite data-source-id=\"ID\" data-locator=\"LOC\">…</cite>, using ONLY an ID and \
LOC that appear verbatim in a SOURCES entry. Never invent a source id or locator; \
if nothing fits, write the sentence without a <cite>. This is the sole exception \
to the \"no data-* attributes\" rule.";

    /// Formats retrieved passages into a user-message block the model can cite
    /// from. Empty string when there is no grounding (index still filling, §14).
    pub fn sources_block(sources: &str) -> String {
        if sources.trim().is_empty() {
            String::new()
        } else {
            format!("\n\nSOURCES (cite by the exact id/locator shown):\n{sources}")
        }
    }

    pub fn prose(topic: &str, item_title: &str, context: &str, sources: &str) -> Vec<ChatMessage> {
        let cite = if sources.trim().is_empty() {
            String::new()
        } else {
            format!("\n\n{CITE_CONTRACT}")
        };
        vec![
            ChatMessage::system(format!(
                "You are a tutor. Write short blocks of explanatory prose in semantic \
                 HTML. Be atomic: explain only this concept and stop before any \
                 exercise (the check comes in a separate step).\n\n\
                 {PROSE_HTML_CONTRACT}{cite}"
            )),
            ChatMessage::user(format!(
                "Overall topic: {topic}\nConcept of this node: {item_title}\n\
                 Context of what has been taught so far: {context}{}",
                sources_block(sources)
            )),
        ]
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

    /// A question asked mid-reading (§S6, §9). `anchor_text` is the exact
    /// selected span (selection→question) or the whole current block's text
    /// (question-on-the-line, no selection) — either way the caller already
    /// resolved it against the frozen content layer; `None` only if neither
    /// applies. Same adversarial-not-flattering instruction `confront` uses
    /// (§7), but scoped to only when the question actually states a
    /// position — a plain clarifying question just gets a clear answer.
    pub fn answer_question(
        topic: &str,
        item_title: &str,
        node_context: &str,
        anchor_text: Option<&str>,
        question: &str,
    ) -> Vec<ChatMessage> {
        let anchor_line = match anchor_text {
            Some(t) => format!("\nThe learner selected/is reading this exact passage: \"{t}\""),
            None => String::new(),
        };
        vec![
            ChatMessage::system(format!(
                "The learner is reading a node of a living document and asked a \
                 question, in place — your answer becomes part of the document \
                 itself, not a side chat (§9). Answer directly and honestly, grounded \
                 in the node's content. If the question states a position or \
                 disagreement, engage with it dialectically — build the strongest \
                 honest counter-argument rather than flattering or simply validating \
                 it (§7) — but only when there is actually a stance to engage with; a \
                 plain clarifying question just gets a clear, honest answer. Do not \
                 repeat the whole node; answer the specific question.\n\n\
                 {PROSE_HTML_CONTRACT}"
            )),
            ChatMessage::user(format!(
                "Topic: {topic}\nConcept of this node: {item_title}\n\
                 Node content so far: {node_context}{anchor_line}\n\nQuestion: {question}"
            )),
        ]
    }

    /// Decides whether a question gets answered inline or spawns a sub-node
    /// (§7/§S8).
    pub fn decide_ask_response(
        topic: &str,
        item_title: &str,
        node_context: &str,
        anchor_text: Option<&str>,
        question: &str,
    ) -> Vec<ChatMessage> {
        let anchor_line = match anchor_text {
            Some(t) => format!("\nThe learner selected/is reading this exact passage: \"{t}\""),
            None => String::new(),
        };
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
                 Node content so far: {node_context}{anchor_line}\n\nQuestion: {question}"
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
        anchor_text: Option<&str>,
        question: &str,
    ) -> Vec<ChatMessage> {
        let anchor_line = match anchor_text {
            Some(t) => format!("\nThe learner selected/is reading this exact passage: \"{t}\""),
            None => String::new(),
        };
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
                 {sub_title}\nParent node content so far: {node_context}{anchor_line}\n\n\
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
}

// ---------------------------------------------------------------------------
// Tolerant parsing of the model output.
// ---------------------------------------------------------------------------

pub mod parse {
    use super::{
        AskDecision, Assessment, EngineError, ExerciseAndRubric, ObjectiveProposal, Rubric,
        RubricObjective,
    };
    use learnive_core::ObjectiveType;
    use serde::Deserialize;

    /// Cold-start objective proposal (§S4): `{"text":"...","non_goals":[...]}`.
    pub fn objective_proposal(text: &str) -> Result<ObjectiveProposal, EngineError> {
        let json = extract_json(text).ok_or_else(|| EngineError::Parse("no JSON".to_string()))?;
        serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))
    }

    /// `{"spawn":bool,"title":"..."}` (§S8) — `spawn:false` degrades to
    /// `AskDecision::Inline` regardless of `title`.
    pub fn ask_decision(text: &str) -> Result<AskDecision, EngineError> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            spawn: bool,
            #[serde(default)]
            title: String,
        }
        let json = extract_json(text).ok_or_else(|| EngineError::Parse("no JSON".to_string()))?;
        let raw: Raw = serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))?;
        if raw.spawn && !raw.title.trim().is_empty() {
            Ok(AskDecision::Spawn {
                title: raw.title.trim().to_string(),
            })
        } else {
            Ok(AskDecision::Inline)
        }
    }

    /// Extracts the first JSON block (`{...}` or `[...]`) from the text, tolerating
    /// markdown fences and surrounding text. Also reused by `movement.rs` (S2) —
    /// the move ABI's JSON contract needs the same tolerant extraction.
    pub(crate) fn extract_json(text: &str) -> Option<&str> {
        let start = text.find(['{', '['])?;
        let open = text.as_bytes()[start];
        let close = if open == b'{' { b'}' } else { b']' };
        let end = text.rfind(close as char)?;
        if end > start {
            Some(&text[start..=end])
        } else {
            None
        }
    }

    /// Outline: JSON array of strings, with a fallback to bulleted lines.
    pub fn outline(text: &str) -> Option<Vec<String>> {
        if let Some(json) = extract_json(text)
            && let Ok(list) = serde_json::from_str::<Vec<String>>(json)
            && !list.is_empty()
        {
            return Some(list.into_iter().map(|s| s.trim().to_string()).collect());
        }
        // Fallback: one line per concept, stripping bullets/numbering.
        let items: Vec<String> = text
            .lines()
            .map(|l| {
                l.trim()
                    .trim_start_matches(['-', '*', '#', '•'])
                    .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ')')
                    .trim()
                    .to_string()
            })
            .filter(|l| !l.is_empty())
            .collect();
        (!items.is_empty()).then_some(items)
    }

    pub fn exercise_rubric(text: &str) -> Result<ExerciseAndRubric, EngineError> {
        #[derive(Deserialize)]
        struct Raw {
            exercise_html: String,
            objectives: Vec<RawObjective>,
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

        let json = extract_json(text).ok_or_else(|| EngineError::Parse("no JSON".to_string()))?;
        let raw: Raw = serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))?;
        Ok(ExerciseAndRubric {
            exercise_html: raw.exercise_html,
            rubric: Rubric {
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
            },
        })
    }

    pub fn assessment(text: &str) -> Result<Assessment, EngineError> {
        let json = extract_json(text).ok_or_else(|| EngineError::Parse("no JSON".to_string()))?;
        serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))
    }
}

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
        // from the '\n' separators, and no ids collide across moves.
        let explain_move = "<h2>Limits</h2><p>Explanation.</p>";
        let ask_move = "<p>What happens as x approaches the boundary?</p>";
        let content_html = format!("{explain_move}\n{ask_move}\n");

        let node = assemble_node(
            "d1",
            "n1",
            &content_html,
            "<form><input name=\"r\"></form>",
            "ex1",
            "ru1",
        )
        .unwrap();

        assert_eq!(node.content.blocks.len(), 3);
        let ids: std::collections::HashSet<_> =
            node.content.blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids.len(), 3, "block ids must not collide across moves");
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
        let prose_sys = &prompt::prose("t", "c", "ctx", "")[0].content;
        assert!(prose_sys.contains("NEVER use"));
        assert!(prose_sys.contains("<script>"));

        let rem_sys = &prompt::remediation("c", "<form></form>", "a", "o1: x", 2)[0].content;
        assert!(rem_sys.contains(prompt::PROSE_HTML_CONTRACT));

        // The exercise runs in the sandbox: opposite contract (may use JS, must postMessage).
        let ex_sys = &prompt::remediation_exercise("c", "<form></form>", 1, "")[0].content;
        assert!(ex_sys.contains("postMessage"));
        assert!(ex_sys.contains("sandbox"));
    }

    #[test]
    fn citation_contract_only_with_sources() {
        // No sources → no <cite> instruction (avoids hallucinated citations).
        let plain = &prompt::prose("t", "c", "ctx", "")[0].content;
        assert!(!plain.contains("data-source-id"));
        let plain_user = &prompt::prose("t", "c", "ctx", "")[1].content;
        assert!(!plain_user.contains("SOURCES"));

        // With sources → citation contract + the sources block appear.
        let src = "[id: calc-1 | loc: sec:2.1 | Calculus — Limits] A limit is ...";
        let sys = &prompt::prose("t", "c", "ctx", src)[0].content;
        assert!(sys.contains("data-source-id") && sys.contains("Never invent"));
        let user = &prompt::prose("t", "c", "ctx", src)[1].content;
        assert!(user.contains("SOURCES") && user.contains("calc-1"));
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
    async fn propose_objective_via_mock() {
        let ai = mock_ai(
            r#"{"text":"Learn enough discrete math to read CS papers","non_goals":["number theory"]}"#,
        );
        let proposal = propose_objective(&ai, "discrete math for a CS degree")
            .await
            .unwrap();
        assert_eq!(
            proposal.text,
            "Learn enough discrete math to read CS papers"
        );
        assert_eq!(proposal.non_goals, vec!["number theory".to_string()]);
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
