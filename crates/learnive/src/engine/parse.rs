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

/// Reading list (S27e, PLAN.md §27): a JSON array shaped almost exactly like
/// `source::ProposedItem` — `{title, authors, year, edition, identifier,
/// kind}`, plus one extra field this call alone reads, `topics` (S27g,
/// 2026-08-29) — one object per book/article, foundational-first.
/// Deliberately the SAME bibliographic schema S27d's `verify_bibliography`
/// consumes, rather than inventing a parallel one: `Raw` below only adds
/// `topics` on top via `#[serde(flatten)]`, so `ProposedItem` itself (used
/// for verification, `ExpectedItem` construction, and everywhere else in
/// `source`) never needs to know this concept exists — one schema for the
/// bibliographic round trip, a private extension for the one caller that
/// needs more.
///
/// `topics`, when non-empty, becomes `Chapter`-typed `children` under this
/// item — see `OutlineItemType::Chapter`'s doc comment for why `Chapter` is
/// the right type for an unresolved topic-scoped proposal (not `Node`: a
/// topic hasn't been matched against the real book's contents yet, and
/// `Node` claims a concept is ready to teach). `bibliography: None` on each
/// child — a chapter has no bibliographic identity of its own, it inherits
/// its parent's (`engine::resolve_grounding_source`). `kind` on the OUTER
/// item becomes `item_type` (`Book`/`Article` only — the reading-list schema
/// itself still cannot mint a top-level `Chapter`/`Node`, since `SourceKind`
/// has no such variants to parse into; only a `topics` entry, nested under a
/// verified bibliographic parent, can).
///
/// An empty array is NOT treated specially here — `engine::propose_outline`
/// rejects it (there is always at least one work covering the objective
/// itself) — this function only reports whether the text was readable,
/// schema-conforming JSON at all.
pub fn outline_tree(text: &str) -> Option<Vec<ProposedOutlineNode>> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(flatten)]
        item: ProposedItem,
        #[serde(default)]
        topics: Vec<String>,
    }
    let json = extract_json(text)?;
    let raw_items = serde_json::from_str::<Vec<Raw>>(json).ok()?;
    Some(
        raw_items
            .into_iter()
            .map(|raw| {
                let item_type = match raw.item.kind {
                    SourceKind::Book => OutlineItemType::Book,
                    SourceKind::Article => OutlineItemType::Article,
                };
                let children = raw
                    .topics
                    .into_iter()
                    .filter(|t| !t.trim().is_empty())
                    .map(|topic| ProposedOutlineNode {
                        title: topic,
                        children: Vec::new(),
                        item_type: OutlineItemType::Chapter,
                        bibliography: None,
                        verification: None,
                    })
                    .collect();
                ProposedOutlineNode {
                    title: raw.item.title.clone(),
                    children,
                    item_type,
                    bibliography: Some(raw.item),
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
