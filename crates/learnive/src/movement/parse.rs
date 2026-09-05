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
struct RawGroundingVerdict {
    /// Claim text the model quotes/paraphrases from GENERATED, one entry
    /// per unsupported claim (`movement::grounding`'s verification call).
    /// An empty (or omitted) array is the "fully supported" verdict — there
    /// is no separate boolean to fall out of sync with the list.
    #[serde(default)]
    unsupported_claims: Vec<String>,
    /// The citation mapping (2026-09-05: the same call assigns citations —
    /// the model's own vocabulary no longer contains them). Empty or
    /// omitted when nothing cited / the caller asked for verification only.
    #[serde(default)]
    citations: Vec<RawCitation>,
}

/// One block→passage citation from the verification call's mapping:
/// `b` is GENERATED's 1-based block number (`learnive_core::numbered_blocks`),
/// `s` the 1-based SOURCE passage number, `loc` the passage's own locator
/// copied back. All three are validated by the caller before anything is
/// inserted — a citation can only land on a source the server itself
/// selected.
#[derive(Debug, Deserialize)]
pub struct RawCitation {
    pub b: usize,
    pub s: usize,
    pub loc: String,
}

/// The grounding-verification call's full verdict (§S21,
/// `movement::grounding`): the unsupported-claims list plus the citation
/// mapping.
#[derive(Debug)]
pub struct GroundingVerdict {
    pub unsupported_claims: Vec<String>,
    pub citations: Vec<RawCitation>,
}

/// Parses the grounding-verification call's verdict (§S21,
/// `movement::grounding`). Empty `unsupported_claims` means every claim
/// checked out; empty `citations` means nothing was mapped.
pub fn grounding_verdict(text: &str) -> Result<GroundingVerdict, EngineError> {
    let json = extract_json(text).ok_or_else(|| EngineError::Parse("no JSON".to_string()))?;
    let raw: RawGroundingVerdict =
        serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))?;
    Ok(GroundingVerdict {
        unsupported_claims: raw.unsupported_claims,
        citations: raw.citations,
    })
}
