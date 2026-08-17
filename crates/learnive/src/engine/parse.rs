use super::{
    AskDecision, Assessment, EngineError, ExerciseAndRubric, ObjectiveProposal, PrereqNode, Rubric,
    RubricObjective,
};
use learnive_core::ObjectiveType;
use serde::Deserialize;

/// Cold-start objective proposal (§S4): `{"text":"...","title":"..."}`.
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

/// Prerequisite tree (§S15): a JSON array of `{title, children}`, parsed
/// recursively by `serde` directly (`PrereqNode::children` already defaults
/// to empty). `Some(vec![])` for an explicit empty array is a valid, common
/// answer (most objectives need no prerequisites) — only unparseable text
/// returns `None`.
pub fn prereq_tree(text: &str) -> Option<Vec<PrereqNode>> {
    let json = extract_json(text)?;
    serde_json::from_str::<Vec<PrereqNode>>(json).ok()
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
