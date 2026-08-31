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
/// kind}`, plus one extra field this call alone reads, `chapters` (S27g,
/// introduced 2026-08-29 as `topics`, reversed to number+name 2026-08-30 —
/// see `prompt::propose_outline`'s doc for the full account) — one object
/// per book/article, foundational-first. Deliberately the SAME bibliographic
/// schema S27d's `verify_bibliography` consumes, rather than inventing a
/// parallel one: `Raw` below only adds `chapters` on top via
/// `#[serde(flatten)]`, so `ProposedItem` itself (used for verification,
/// `ExpectedItem` construction, and everywhere else in `source`) never needs
/// to know this concept exists — one schema for the bibliographic round
/// trip, a private extension for the one caller that needs more.
///
/// `chapters`, when non-empty, becomes `Chapter`-typed `children` under this
/// item, each carrying the proposed `number` on [`ProposedOutlineNode::
/// chapter_number`] and the proposed `name` as its own `title` — see
/// `OutlineItemType::Chapter`'s doc comment for why `Chapter` is the right
/// type for an unresolved chapter proposal (not `Node`: it hasn't been
/// matched against the real book's contents yet, and `Node` claims a
/// concept is ready to teach). `bibliography: None` on each child — a
/// chapter has no bibliographic identity of its own, it inherits its
/// parent's (`engine::resolve_grounding_source`). `kind` on the OUTER item
/// becomes `item_type` (`Book`/`Article` only — the reading-list schema
/// itself still cannot mint a top-level `Chapter`/`Node`, since `SourceKind`
/// has no such variants to parse into; only a `chapters` entry, nested under
/// a verified bibliographic parent, can). An entry with an empty/whitespace
/// `name` is dropped — same defensive filter the old `topics` shape used.
///
/// An empty array is NOT treated specially here — `engine::propose_outline`
/// rejects it (there is always at least one work covering the objective
/// itself) — this function only reports whether the text was readable,
/// schema-conforming JSON at all.
pub fn outline_tree(text: &str) -> Option<Vec<ProposedOutlineNode>> {
    #[derive(Deserialize)]
    struct RawChapter {
        #[serde(default)]
        number: Option<String>,
        #[serde(default)]
        name: String,
    }
    #[derive(Deserialize)]
    struct Raw {
        #[serde(flatten)]
        item: ProposedItem,
        #[serde(default)]
        chapters: Vec<RawChapter>,
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
                    .chapters
                    .into_iter()
                    .filter(|c| !c.name.trim().is_empty())
                    .map(|c| ProposedOutlineNode {
                        title: c.name,
                        children: Vec::new(),
                        item_type: OutlineItemType::Chapter,
                        chapter_number: c.number.filter(|n| !n.trim().is_empty()),
                        bibliography: None,
                        verification: None,
                    })
                    .collect();
                ProposedOutlineNode {
                    title: raw.item.title.clone(),
                    children,
                    item_type,
                    chapter_number: None,
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

/// S27g item 2: `["Sub-topic A","Sub-topic B",...]`, filtered for non-empty
/// titles after trimming. `None` only when the response has no JSON at
/// all — `engine::propose_chapter_split` degrades that identically to an
/// empty array (the chapter stays one node), per PLAN.md's "tenta é
/// literal, falhar é um desfecho normal".
pub fn chapter_split(text: &str) -> Option<Vec<String>> {
    let json = extract_json(text)?;
    let raw: Vec<String> = serde_json::from_str(json).ok()?;
    Some(
        raw.into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
    )
}

pub fn assessment(text: &str) -> Result<Assessment, EngineError> {
    let json = extract_json(text).ok_or_else(|| EngineError::Parse("no JSON".to_string()))?;
    serde_json::from_str(json).map_err(|e| EngineError::Parse(e.to_string()))
}
