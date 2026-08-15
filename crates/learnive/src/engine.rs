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
use futures_util::StreamExt;
use rand::{Rng, distributions::Alphanumeric};
use serde::{Deserialize, Serialize};

use learnive_core::{Node, ObjectiveType, ensure_block_ids, render_math};

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
    /// Short display name for the living document (§S12) — what the sidebar
    /// shows and what the learner renames. Ridden along on this call rather
    /// than given one of its own: the cold start already pays for a fast-tier
    /// round trip here, and a second one just to title the document would be
    /// latency (§14) and tokens (§12.2) for a label. Empty when the model
    /// omits it; the caller falls back to the topic.
    #[serde(default)]
    pub title: String,
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
    let html = collect(
        ai,
        Tier::Robust,
        prompt::remediation(item_title, exercise_html, answer, &unmet_summary, attempt),
    )
    .await?;
    // Rendered here, not at assembly: this prose goes straight into the
    // append-only interaction layer (§4.3) as `body_html` and never passes
    // through `assemble_node`, which is where every other path picks up math
    // rendering. Applying it in both places would be a wasted second pass.
    Ok(render_math(&html))
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
    reading_context: Option<&str>,
    question: &str,
) -> Result<String, EngineError> {
    let html = collect(
        ai,
        Tier::Robust,
        prompt::answer_question(topic, item_title, node_context, reading_context, question),
    )
    .await?;
    // Interaction layer, not assembled — same reason as `remediate`.
    Ok(render_math(&html))
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
    reading_context: Option<&str>,
    question: &str,
) -> Result<AskDecision, EngineError> {
    let messages =
        prompt::decide_ask_response(topic, item_title, node_context, reading_context, question);
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
    reading_context: Option<&str>,
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
            reading_context,
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
    let blocks = ensure_block_ids(&render_math(prose_inner_html), &format!("{node_id}-b"));
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
    let blocks = ensure_block_ids(&render_math(prose_inner_html), &format!("{node_id}-b"));
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
/// in the page script). `srcdoc` documents inherit the parent page's CSP — not
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

pub mod prompt;

pub mod parse;

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
        let rem_sys = &prompt::remediation("c", "<form></form>", "a", "o1: x", 2)[0].content;
        assert!(rem_sys.contains(prompt::PROSE_HTML_CONTRACT));

        // The exercise runs in the sandbox: opposite contract (may use JS, must postMessage).
        let ex_sys = &prompt::remediation_exercise("c", "<form></form>", 1, "")[0].content;
        assert!(ex_sys.contains("postMessage"));
        assert!(ex_sys.contains("sandbox"));
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
        let ai = mock_ai(r#"{"text":"Learn enough discrete math to read CS papers"}"#);
        let proposal = propose_objective(&ai, "discrete math for a CS degree")
            .await
            .unwrap();
        assert_eq!(
            proposal.text,
            "Learn enough discrete math to read CS papers"
        );
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

#[cfg(test)]
mod contract_tests {
    /// The math sub-contract must actually reach the model. `PROSE_HTML_CONTRACT`
    /// is a `const` that gets interpolated *into* other format strings, so a
    /// `{MATH_CONTRACT}` placeholder inside it is never expanded — it would ship
    /// to the model as the literal seven characters. This guards that.
    #[test]
    fn prose_contract_embeds_the_math_rules_not_a_placeholder() {
        let c = super::prompt::PROSE_HTML_CONTRACT;
        assert!(
            !c.contains("{MATH_CONTRACT}"),
            "unexpanded placeholder: {c}"
        );
        assert!(
            c.contains("$$"),
            "math delimiters missing from the contract"
        );
        assert!(c.contains("Markdown"), "markdown rule missing");
    }
}
