use super::{
    AskDecision, Assessment, EngineError, ObjectiveProposal, OutlineItemType, ProposedOutlineNode,
};
use crate::source::{ProposedItem, SourceKind};
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

/// Reading list (S27e, PLAN.md §27): a JSON array shaped exactly like
/// `source::ProposedItem` — `{title, authors, year, edition, identifier,
/// kind}` — one object per book/article, foundational-first. Deliberately
/// the SAME schema the model is asked for as what S27d's
/// `verify_bibliography` consumes, rather than a wrapping `{title,
/// children}` shape that then carries a nested bibliography object: one
/// schema for the whole round trip is less for the model to get wrong, and
/// nothing about `children` needs to appear here (see
/// `ProposedOutlineNode`'s doc comment — it's always empty coming out of
/// this call). `kind` becomes `item_type` (`Book`/`Article` only — `parse`
/// itself is what enforces that a reading-list response can never mint a
/// `Chapter`/`Node` item, since `SourceKind` has no such variants to parse
/// into in the first place).
///
/// An empty array is NOT treated specially here — `engine::propose_outline`
/// rejects it (there is always at least one work covering the objective
/// itself) — this function only reports whether the text was readable,
/// schema-conforming JSON at all.
pub fn outline_tree(text: &str) -> Option<Vec<ProposedOutlineNode>> {
    let json = extract_json(text)?;
    let items = serde_json::from_str::<Vec<ProposedItem>>(json).ok()?;
    Some(
        items
            .into_iter()
            .map(|item| {
                let item_type = match item.kind {
                    SourceKind::Book => OutlineItemType::Book,
                    SourceKind::Article => OutlineItemType::Article,
                };
                ProposedOutlineNode {
                    title: item.title.clone(),
                    children: Vec::new(),
                    item_type,
                    bibliography: Some(item),
                    verification: None,
                }
            })
            .collect(),
    )
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

/// A model's read of a printed contents page (S27k) — `[{"title":"...",
/// "page":N|null}, ...]`, mapped straight onto `source::toc::TocLlmEntry`
/// (same schema, no wrapping needed). `None` on anything unparseable —
/// `engine::propose_toc`'s caller degrades to the heading heuristic on this,
/// not a hard failure (SPEC: no PDF is ever rejected over its TOC).
pub fn toc_entries(text: &str) -> Option<Vec<crate::source::toc::TocLlmEntry>> {
    let json = extract_json(text)?;
    serde_json::from_str(json).ok()
}

pub fn assessment(text: &str) -> Result<Assessment, EngineError> {
    let json = extract_json(text).ok_or_else(|| EngineError::Parse("no JSON".to_string()))?;
    serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))
}
