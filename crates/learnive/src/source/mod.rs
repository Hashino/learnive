//! Source grounding (§11, §11.1) — the local PDF library is the only source.
//!
//! Node content is grounded in real sources cited by book+chapter/page or
//! article (§11), and the source of explained substance is **never the LLM**
//! (§11): no source coverage means no generation, not model prose with a
//! disclaimer. Since the corpus retirement (2026-09-09, user decision), the
//! only source that exists is the user's own `<data>/library/` (§11.1's
//! always-present tier) — the old pre-pivot acquisition store and its
//! LibGen/Sci-Hub backends are deleted, along with the retrieval index that
//! used to sit on top of them (grounding now reads the per-book PDF page
//! indexes the acervo gate builds; see `acervo::search_index_cache`).
//!
//! What lives here now:
//!
//! - [`local::LocalPdfSource`] — the library scan (`<data>/library/`).
//!   **Matched, not searched** (see [`local`]'s module doc).
//! - [`pdf`] — PDF structure reading: text + embedded outline + page map
//!   (PLAN.md S27b). Extracted text is **index-only** — the canonical,
//!   displayed artifact is always the original PDF in the browser's native
//!   viewer (§4/§11).
//! - [`acervo`] — the §11.1 validation gate (presence, identity, text layer,
//!   table of contents, page map, retrieval index) plus the per-book page
//!   index search grounding retrieves through.
//! - [`bibliography`] — metadata-only existence checks against public
//!   catalogs before a proposed book/article enters the reading list
//!   (PLAN.md S27d). Never a content fetch.
//! - [`manual_match`] / [`matching`] / [`toc`] / [`toc_confirm`] — PDF↔item
//!   matching and chapter/TOC resolution (PLAN.md S27e–S27k).
//! - [`mock`] — the canned demo fixtures (dev-only, `LEARNIVE_DEMO`).
//!
//! The acquisition facade that used to live here ([`Source`] with
//! `search`/`fetch`, `SourceError`, `SearchHit`, the `Corpus` store) is gone
//! with route A (§11.1). If a future acquisition backend decision
//! (PLAN.md's biggest open question) wants the seam back, restore it from
//! git history around this date rather than re-deriving it.
#![allow(dead_code, unused_imports)]

pub mod acervo;
pub mod bibliography;
pub mod local;
pub mod manual_match;
mod matching;
pub mod mock;
pub mod pdf;
pub mod toc;
/// S27g measurement harness — test-only, never compiled into the binary.
#[cfg(test)]
mod toc_bench;
pub mod toc_confirm;

pub use acervo::{
    AcervoReport, CachedChunk, CandidateMatch, ExpectedItem, IdentityCheck, IndexCheck, ItemReport,
    MatchConfidence, PageMapCheck, PresenceCheck, TextLayerCheck, TocCheck, build_index_cache,
    load_index_cache, resolve_matched_filename, search_index_cache, validate_acervo,
};
pub use bibliography::{
    BibliographyCache, BibliographyClient, Catalog, Identifier, ProposedItem, VerificationOutcome,
    verification_plan, verify_bibliography,
};
pub use local::{LibraryEntry, LocalPdfSource};
pub use manual_match::{ManualMatch, ManualMatchStore};
pub use pdf::{
    OutlineEntry, PageMap, PdfDocument, PdfReadError, pdftext_cache_dir, read_pdf, read_pdf_cached,
};
pub use toc_confirm::{
    ConfirmedToc, ConfirmedTocEntry, TocConfirmStore, match_chapter, split_printed_number,
    sub_entries_within,
};

/// What kind of thing a source is — steers how a locator is read (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// A book: locator is `chap:N;sec:M;p:K` style.
    Book,
    /// An article/paper: locator is `sec:N;p:K` or a paragraph index.
    Article,
}
