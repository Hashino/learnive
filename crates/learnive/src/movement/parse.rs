use super::{EngineError, GeneratedMove, MoveType, Rubric, RubricObjective};
use crate::engine::parse::extract_json;
use learnive_core::ObjectiveType;
use serde::Deserialize;

/// Parses a bare type name (e.g. `explain`, not a JSON object) by reusing
/// [`MoveType`]'s existing `snake_case` [`Deserialize`] impl. Used to read
/// `<!--move: type-->` markers off pre-S33 events (S33: move choice is
/// deterministic, so no new markers are written; old logs still carry them).
pub fn move_type_name(name: &str) -> Result<MoveType, EngineError> {
    let quoted = format!("\"{}\"", name.trim());
    serde_json::from_str(&quoted).map_err(|e| EngineError::Parse(e.to_string()))
}

/// Strips a trailing `<!--tactics: a, b-->` sentinel (see the module
/// docs on the streamed path) from streamed move output. A missing or
/// malformed sentinel just means no tactics recorded — the streamed
/// content already rendered successfully either way, so this never
/// errors.
pub fn strip_tactics_sentinel(text: &str) -> (String, Vec<String>) {
    const MARK: &str = "<!--tactics:";
    if let Some(pos) = text.rfind(MARK)
        && let Some(end) = text[pos + MARK.len()..].find("-->")
    {
        let tactics = text[pos + MARK.len()..pos + MARK.len() + end]
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        return (text[..pos].trim_end().to_string(), tactics);
    }
    (text.trim().to_string(), Vec::new())
}

#[derive(Deserialize)]
struct RawMove {
    html: String,
    #[serde(default)]
    interactive: bool,
    #[serde(default)]
    graded: bool,
    #[serde(default)]
    tactics: Vec<String>,
    #[serde(default)]
    reference_solution: String,
    #[serde(default)]
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

/// Parses + validates a **structured** move against the JSON contract.
/// `Test` is intrinsically graded (§8) — not the model's discretion, so
/// `graded` is forced true regardless of what the model set.
pub fn generated_move(move_type: MoveType, text: &str) -> Result<GeneratedMove, EngineError> {
    let json = extract_json(text).ok_or_else(|| EngineError::Parse("no JSON".to_string()))?;
    let raw: RawMove = serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))?;

    if raw.html.trim().is_empty() {
        return Err(EngineError::Parse("empty html".to_string()));
    }

    let graded = raw.graded || matches!(move_type, MoveType::Test);
    if graded && raw.objectives.is_empty() {
        return Err(EngineError::Parse(
            "graded move with no objectives".to_string(),
        ));
    }

    let rubric = graded.then(|| Rubric {
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
    });

    Ok(GeneratedMove {
        move_type,
        interactive: raw.interactive,
        graded,
        html: raw.html,
        tactics: raw.tactics,
        rubric,
        reference_solution: raw.reference_solution,
        repaired: false,
    })
}

#[derive(Deserialize)]
struct RawSupportVerdict {
    /// The 1-based numbers of the suspect PARAGRAPHS the model judged NOT
    /// supported by their cited page (`movement::grounding`'s lean
    /// adjudication call, 2026-09-05). An empty (or omitted) array is the
    /// "every paragraph checks out" verdict — there is no separate boolean
    /// to fall out of sync with the list.
    #[serde(default)]
    unsupported: Vec<usize>,
}

/// The grounding adjudication call's verdict (§S21 lean,
/// `movement::grounding`): which suspect paragraphs failed support.
#[derive(Debug)]
pub struct SupportVerdict {
    pub unsupported: Vec<usize>,
}

/// Parses the adjudication call's verdict (§S21 lean). Empty `unsupported`
/// means every suspect paragraph checked out.
pub fn support_verdict(text: &str) -> Result<SupportVerdict, EngineError> {
    let json = extract_json(text).ok_or_else(|| EngineError::Parse("no JSON".to_string()))?;
    let raw: RawSupportVerdict =
        serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))?;
    Ok(SupportVerdict {
        unsupported: raw.unsupported,
    })
}
