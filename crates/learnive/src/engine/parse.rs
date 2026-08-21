use super::{
    AskDecision, Assessment, EngineError, ExerciseAndRubric, ObjectiveProposal,
    ProposedOutlineNode, Rubric, RubricObjective,
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

/// Full outline tree (§S15/§S16, unified 2026-08-19): a JSON array of
/// `{title, children}`, parsed recursively by `serde` directly
/// (`ProposedOutlineNode::children` already defaults to empty). Unlike the
/// old prerequisite-only forest this replaces, an empty array is NOT treated
/// specially here — `engine::propose_outline` rejects it (the objective's
/// own node is always at least one element) — this function only reports
/// whether the text was readable JSON at all.
pub fn outline_tree(text: &str) -> Option<Vec<ProposedOutlineNode>> {
    let json = extract_json(text)?;
    serde_json::from_str::<Vec<ProposedOutlineNode>>(json).ok()
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

pub fn exercise_rubric(text: &str) -> Result<ExerciseAndRubric, EngineError> {
    #[derive(Deserialize)]
    struct Raw {
        exercise_html: String,
        #[serde(default)]
        reference_solution: String,
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
        reference_solution: raw.reference_solution,
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
