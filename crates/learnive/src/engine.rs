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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineItem {
    pub title: String,
}

/// Skeleton of the living document (§6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outline {
    pub topic: String,
    pub items: Vec<OutlineItem>,
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
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Provider(e) => write!(f, "provider: {e}"),
            EngineError::Parse(m) => write!(f, "model response could not be read: {m}"),
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

/// Generates the initial outline from the topic (§6, §6.1). Light tier (planning).
pub async fn generate_outline(ai: &Ai, topic: &str) -> Result<Outline, EngineError> {
    let text = collect(ai, Tier::Fast, prompt::outline(topic)).await?;
    let titles =
        parse::outline(&text).ok_or_else(|| EngineError::Parse("empty outline".to_string()))?;
    Ok(Outline {
        topic: topic.to_string(),
        items: titles
            .into_iter()
            .map(|title| OutlineItem { title })
            .collect(),
    })
}

/// Generates a node's exercise + rubric (§8), separate from the prose (§14). Light tier.
pub async fn generate_exercise_and_rubric(
    ai: &Ai,
    topic: &str,
    item_title: &str,
    prose: &str,
) -> Result<ExerciseAndRubric, EngineError> {
    let text = collect(
        ai,
        Tier::Fast,
        prompt::exercise_rubric(topic, item_title, prose),
    )
    .await?;
    parse::exercise_rubric(&text)
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

/// Collects a full stream into a String (for non-streamed calls).
async fn collect(ai: &Ai, tier: Tier, messages: Vec<ChatMessage>) -> Result<String, EngineError> {
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
    pub const PROSE_HTML_CONTRACT: &str = "\
HTML rules (the content is sanitized — anything that violates them disappears on render):\n\
- Use ONLY static semantic HTML: <h2>-<h4>, <p>, <ul>/<ol>/<li>, <table>/<tr>/\
<td>, <code>/<pre>, <blockquote>, <strong>/<em>, <a href> (only http(s), mailto: \
or #) and <img> (only src https: or data:image/).\n\
- NEVER use: <script>, <style>, <form>, <iframe>, <object>, <embed>, <link>, \
<meta>, <base>; nor on* attributes (onclick, onerror...), nor inline style \
attributes, nor javascript: URLs. All of that is discarded.\n\
- Do not generate id/data-* attributes — the server assigns them.\n\
- Need interactivity, a dynamic visualization, JS or custom CSS? That does NOT \
belong in prose (it would be removed); it goes in the exercise block, which runs \
isolated in a sandbox iframe.";

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
- Simple case: include a <form> (or an <input>/<textarea>) — the page injects the \
\"Submit\" button and collects the fields automatically.\n\
- Interactive/custom case: when the student finishes, call it yourself \
parent.postMessage({type:'learnive-answer', answer: JSON.stringify(ARTIFACT)}, '*'), \
where ARTIFACT is a structured JSON the rubric can grade.\n\
Never write the grading criteria inside the exercise_html (they are server-only).";

    pub fn outline(topic: &str) -> Vec<ChatMessage> {
        vec![
            ChatMessage::system(
                "You plan a learning curriculum. Given a topic, respond ONLY with a \
                 JSON array of strings — the concept titles, from most basic to \
                 advanced, atomic (§6). No comments, no markdown.",
            ),
            ChatMessage::user(format!("Topic: {topic}")),
        ]
    }

    pub fn prose(topic: &str, item_title: &str, context: &str) -> Vec<ChatMessage> {
        vec![
            ChatMessage::system(format!(
                "You are a tutor. Write short blocks of explanatory prose in semantic \
                 HTML. Be atomic: explain only this concept and stop before any \
                 exercise (the check comes in a separate step).\n\n\
                 {PROSE_HTML_CONTRACT}"
            )),
            ChatMessage::user(format!(
                "Overall topic: {topic}\nConcept of this node: {item_title}\n\
                 Context of what has been taught so far: {context}"
            )),
        ]
    }

    pub fn exercise_rubric(topic: &str, item_title: &str, prose: &str) -> Vec<ChatMessage> {
        vec![
            ChatMessage::system(format!(
                "Generate a comprehension check for the concept and its grading \
                 rubric, TOGETHER (§8). Respond ONLY with JSON in the format: \
                 {{\"exercise_html\":\"<form>...</form>\",\"objectives\":[{{\"id\":\"o1\",\
                 \"kind\":\"knowledge|application|synthesis\",\"description\":\"...\",\
                 \"criteria\":\"what counts as demonstrated\",\"transfer\":true|false}}]}}. \
                 Every 'application' objective must have at least one transfer item \
                 (transfer=true) for a scenario NOT covered in the text.\n\n\
                 {EXERCISE_HTML_CONTRACT}"
            )),
            ChatMessage::user(format!(
                "Topic: {topic}\nConcept: {item_title}\nNode prose:\n{prose}"
            )),
        ]
    }

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
                 the concept IN THE CONTEXT of the exercise: give a worked example / \
                 step by step, and propose a new similar problem. This is attempt \
                 {attempt}: the higher it is, the MORE similar to the worked example \
                 the new problem should be (scaffolding by increasing proximity).\n\n\
                 {PROSE_HTML_CONTRACT}"
            )),
            ChatMessage::user(format!(
                "Concept: {item_title}\nExercise: {exercise_html}\n\
                 Student's answer: {answer}\nObjectives not demonstrated:\n{unmet_summary}"
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
    use super::{Assessment, EngineError, ExerciseAndRubric, Rubric, RubricObjective};
    use learnive_core::ObjectiveType;
    use serde::Deserialize;

    /// Extracts the first JSON block (`{...}` or `[...]`) from the text, tolerating
    /// markdown fences and surrounding text.
    fn extract_json(text: &str) -> Option<&str> {
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
    fn sanitized_surfaces_carry_the_prose_contract() {
        // Prose and remediation go to the app origin and are sanitized, so the
        // model must be told the contract (otherwise it generates something that
        // disappears).
        let prose_sys = &prompt::prose("t", "c", "ctx")[0].content;
        assert!(prose_sys.contains("NEVER use"));
        assert!(prose_sys.contains("<script>"));

        let rem_sys = &prompt::remediation("c", "<form></form>", "a", "o1: x", 2)[0].content;
        assert!(rem_sys.contains(prompt::PROSE_HTML_CONTRACT));

        // The exercise runs in the sandbox: opposite contract (may use JS, must postMessage).
        let ex_sys = &prompt::exercise_rubric("t", "c", "prose")[0].content;
        assert!(ex_sys.contains("postMessage"));
        assert!(ex_sys.contains("sandbox"));
    }

    #[tokio::test]
    async fn generate_outline_via_mock() {
        let ai = mock_ai(r#"["Introduction", "Sets", "Functions"]"#);
        let outline = generate_outline(&ai, "mathematics").await.unwrap();
        assert_eq!(outline.topic, "mathematics");
        assert_eq!(outline.items.len(), 3);
        assert_eq!(outline.items[1].title, "Sets");
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
