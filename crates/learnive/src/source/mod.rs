//! Source acquisition (§11, §11.1).
//!
//! Node content is grounded in real sources cited by book+chapter or article
//! (§11). Acquisition is agent-driven and behind ONE swappable facade so the
//! backend is an implementation detail — origin is a deliberately open
//! question (§11.1, 2026-08-23: reopened when the app moved to personal-use
//! only and chapter-granularity nodes, still pending which backend covers the
//! user's own book/paper library).
//!
//! **No backend is wired in by default (2026-08-23):** the earlier OpenStax
//! and Wikipedia backends were deleted at the user's request once §11.1's
//! origin reopened, rather than left running as a default nobody had chosen
//! anymore — see git history around that date to restore either one if a
//! future backend decision wants to reuse them. Backends are selected by
//! configuration (see `build_source`/`build_fallback_source` in
//! `api/cold_start.rs`) — when none is configured, [`Source::Unconfigured`]
//! makes every call fail fast with [`SourceError::Unconfigured`] instead of
//! silently acquiring nothing or panicking — same shape as `ai::Provider`'s
//! `Unconfigured` variant (§22). LibGen and Sci-Hub backends are available
//! behind the same facade; they talk only to the mirror URL the user points
//! them at via `LEARNIVE_LIBGEN_URL` / `LEARNIVE_SCIHUB_URL`.
//!
//! **`Source::LocalPdf` (PLAN.md S27a, added 2026-08-27):** §11.1's
//! always-present fallback tier — the user's own `<data>/library/`, scanned
//! by [`local::LocalPdfSource`]. Not wired in as the active backend yet
//! (`build_source`/`build_fallback_source` still return `LibGen`/`SciHub`);
//! this slice only proves the app can see a manually-placed PDF. A local
//! library is **matched**, not searched, so it opts out of the
//! `search`/`fetch` shape below (see [`local`]'s module doc).
//!
//! **PDF structure reading (PLAN.md S27b, added 2026-08-27):** [`pdf::read_pdf`]
//! turns a PDF on disk into text + embedded outline + page map — the sumário
//! and mapa de páginas the S27a library scan deliberately didn't build (see
//! `local`'s module doc). Standalone, no caller yet; see [`pdf`]'s module doc.
//!
//! **Acervo validation gate (PLAN.md S27c, added 2026-08-27):**
//! [`acervo::validate_acervo`] runs SPEC §11.1's six checks (presence,
//! identity, text layer, table of contents, page map, retrieval index) as a
//! pure function over the local library + a small [`acervo::ExpectedItem`]
//! list, returning a per-item [`acervo::AcervoReport`]. Standalone, no
//! caller yet (the cold-start wiring is S27g+); see [`acervo`]'s module doc,
//! including its scope note on the retrieval-index check.
//!
//! **Bibliographic existence verification (PLAN.md S27d, added 2026-08-27):**
//! [`bibliography::verify_bibliography`] answers a narrower, deliberately
//! *lenient* question against public catalogs (OpenLibrary/Google Books for
//! books, Crossref/arXiv/OpenAlex for articles) before the acervo gate's
//! strict per-file check ever runs: "does something like this exist?" — see
//! [`bibliography`]'s module doc for the calibration rationale and the
//! `Verified`/`NotFound`/`Unavailable` three-way result. Standalone, no
//! caller yet (the model call site that produces a
//! [`bibliography::ProposedItem`] is S27e).
//!
//! Swap seam: [`Source`] is an enum facade (same idiom as `ai::Provider`); a new
//! backend is a new variant, no call-site changes — this is what keeps the
//! open §11.1 question from blocking anything else.
//! Everything a backend returns is **normalized** to one internal representation
//! ([`FetchedSource`]: extracted plain text for grounding/retrieval + the
//! original PDF bytes as the canonical, displayed artifact — PDF is the sole
//! canonical format, S28/pivot 2026-08-23; extracted text is index-only,
//! never rendered).
//!
//! Not fully consumed at runtime yet (grounding is Phase B); hence the temporary
//! `allow`s (mirrors `ai::mod`).
#![allow(dead_code, unused_imports)]

pub mod acervo;
pub mod bibliography;
pub mod corpus;
pub mod libgen;
pub mod local;
pub mod manual_match;
mod matching;
pub mod mock;
pub mod pdf;
pub mod scihub;
pub mod toc;
/// S27g measurement harness — test-only, never compiled into the binary.
#[cfg(test)]
mod toc_bench;
pub mod toc_confirm;

pub use acervo::{
    AcervoReport, CachedChunk, CandidateMatch, ExpectedItem, IdentityCheck, IndexCheck, ItemReport,
    MatchConfidence, PageMapCheck, PresenceCheck, TextLayerCheck, TocCheck, build_index_cache,
    resolve_matched_filename, search_index_cache, validate_acervo,
};
pub use bibliography::{
    BibliographyCache, BibliographyClient, Catalog, Identifier, ProposedItem, VerificationOutcome,
    verification_plan, verify_bibliography,
};
pub use corpus::{Corpus, CorpusError};
pub use libgen::LibGenSource;
pub use local::{LibraryEntry, LocalPdfSource};
pub use manual_match::{ManualMatch, ManualMatchStore};
pub use mock::MockSource;
pub use pdf::{
    OutlineEntry, PageMap, PdfDocument, PdfReadError, pdftext_cache_dir, read_pdf, read_pdf_cached,
};
pub use scihub::SciHubSource;
pub use toc_confirm::{
    ConfirmedToc, ConfirmedTocEntry, TocConfirmStore, match_chapter, sub_entries_within,
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

/// Where a source came from — kept for attribution and for the §16 note that the
/// backend must stay swappable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "backend")]
pub enum Origin {
    /// Kept for existing corpus entries acquired while this backend was
    /// wired in; the backend itself was deleted 2026-08-23 (see the module
    /// doc comment) — no code constructs this variant anymore.
    OpenStax,
    /// Free, keyless, CC BY-SA — was §11.1's "internet search" fallback
    /// tier. Kept for existing corpus entries; the backend itself was
    /// deleted 2026-08-23 (see the module doc comment).
    Wikipedia,
    LibreTexts,
    /// Open-access book registries (DOAB/OAPEN) — future backend.
    OpenAccessBook,
    /// arXiv / PMC / OpenAlex — future backend.
    OpenAccessPaper,
    /// Public domain (Project Gutenberg / Internet Archive) — future backend.
    PublicDomain,
    /// Library Genesis mirror — configured backend (`LEARNIVE_LIBGEN_URL`).
    LibGen,
    /// Sci-Hub mirror — configured backend (`LEARNIVE_SCIHUB_URL`).
    SciHub,
    /// Canned content for demo/tests (no network).
    Mock,
}

/// A candidate returned by [`Source::search`], before the (heavier) fetch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    pub title: String,
    pub authors: Vec<String>,
    pub kind: SourceKind,
    pub origin: Origin,
    /// Human-readable license (e.g. `CC BY 4.0`, `Public Domain`).
    pub license: String,
    /// Backend-specific opaque handle used to [`fetch`](Source::fetch) this hit.
    pub handle: String,
    /// Reported length in pages, when the backend exposes it (used to tell a
    /// real textbook apart from a 3-page journal excerpt). `#[serde(default)]`.
    #[serde(default)]
    pub pages: Option<u32>,
    /// Reported file size in bytes, when the backend exposes it (textbooks are
    /// large; a tiny file is a strong "this is a paper, not a book" signal).
    /// `#[serde(default)]`.
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

/// Picks the hit that makes the best *grounding* source: a real book is worth
/// far more than a 3-page journal excerpt, and among equals a bigger file is
/// more likely to be a complete textbook. Used by acquisition so it prefers
/// textbooks over papers even when the search ranks a paper first.
/// Ranks search hits for acquisition: textbooks first (any real book grounds
/// the topic), ordered **smallest first** so the slimmer, more downloadable
/// PDFs are tried before the 35 MB "Manga guide" class that the mirrors
/// routinely reset on; journal articles are a last resort, largest first
/// (most content). `acquire` walks this list and tries each hit until one
/// downloads, so a single flaky reset doesn't sink the whole acquisition.
pub(crate) fn ranked_hits(hits: &[SearchHit]) -> Vec<SearchHit> {
    let mut books: Vec<SearchHit> = hits
        .iter()
        .filter(|h| h.kind == SourceKind::Book)
        .cloned()
        .collect();
    books.sort_by_key(|h| h.size_bytes.unwrap_or(u64::MAX));
    let mut articles: Vec<SearchHit> = hits
        .iter()
        .filter(|h| h.kind == SourceKind::Article)
        .cloned()
        .collect();
    articles.sort_by_key(|h| std::cmp::Reverse(h.size_bytes.unwrap_or(0)));
    books.extend(articles);
    books
}

pub(crate) fn pick_best_hit(hits: &[SearchHit]) -> Option<SearchHit> {
    ranked_hits(hits).into_iter().next()
}

/// Immutable metadata for a source that lives in the corpus (§4). Stable across
/// reuse; the `id` is what a `<cite data-source-id=...>` points at (§4.3).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceMeta {
    /// Stable corpus id (slug + short hash) — the citation target.
    pub id: String,
    pub title: String,
    pub authors: Vec<String>,
    pub kind: SourceKind,
    pub license: String,
    pub origin: Origin,
    /// Filename (within `assets/`) of the canonical PDF artifact for this
    /// source, when one was stored (`source.pdf`). `#[serde(default)]` so
    /// sources acquired before this field existed deserialize without it.
    #[serde(default)]
    pub pdf_asset: Option<String>,
}

/// One addressable piece of a source — the unit a `data-locator` names (§4.3)
/// and the unit that gets chunked/embedded for retrieval (§10, Phase B).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Section {
    /// Locator string, e.g. `chap:3;sec:2` — goes verbatim into
    /// `<cite data-locator="...">` so a citation resolves back here (§4.3).
    /// This is `Corpus`'s own convention (S28 item 5b, PLAN.md): it applies
    /// only to sources acquired through this module's `LibGen`/`SciHub`
    /// backends (§11.1 route A). The live bibliographic grounding path
    /// (`search_index_cache`, route B, what `LocalPdfSource` feeds) never
    /// constructs a `Section` and locates by physical page instead (`p:N`).
    pub locator: String,
    pub title: String,
    /// Extracted plain text — the normalization target used for retrieval
    /// (§10) and for grounding citations. Not rendered for display: the
    /// canonical, displayed artifact is the original PDF (§4/§11, browser's
    /// native viewer), so this text is index-only.
    pub text: String,
}

/// A source after acquisition, **normalized** (§11.1): metadata + addressable
/// sections of extracted text. Whatever the on-disk format was (EPUB/PDF/HTML),
/// downstream only ever sees this.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FetchedSource {
    pub meta: SourceMeta,
    pub sections: Vec<Section>,
    /// The original PDF bytes, kept as the canonical corpus artifact (§4/§11,
    /// PDF is the canonical format). `None` for backends that never produced a
    /// PDF (e.g. `Mock`). `Corpus::store` writes this to `assets/source.pdf`
    /// when present; `#[serde(default)]` so sources acquired before this field
    /// existed deserialize without it.
    #[serde(default)]
    pub pdf: Option<Vec<u8>>,
}

/// A section's locator+title, without its body — what a table of contents is
/// made of (§11.1 item 4, S19).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SectionSummary {
    pub locator: String,
    pub title: String,
}

/// A source's metadata + table of contents, without any section body (§11.1
/// item 4) — the cheap thing `GET /api/sources/{id}` returns instead of the
/// whole [`FetchedSource`], so opening the source panel or reading the
/// sumário doesn't ship a whole book. A section's body comes separately,
/// addressed by locator (`Corpus::load_section`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceIndex {
    pub meta: SourceMeta,
    pub toc: Vec<SectionSummary>,
}

impl FetchedSource {
    /// Total extracted characters — a cheap proxy for "did we get real content".
    pub fn char_len(&self) -> usize {
        self.sections.iter().map(|s| s.text.len()).sum()
    }
}

/// Acquisition errors. Network/parse failures are recoverable — the caller falls
/// back down the §11.1 chain (OER → OA → web search).
#[derive(Debug)]
pub enum SourceError {
    /// Backend reached but returned nothing usable for the query.
    NoResult,
    /// Network/transport failure.
    Network(String),
    /// Fetched but could not be normalized to text.
    Normalize(String),
    /// Persisting to / reading from the corpus failed.
    Corpus(CorpusError),
    /// No acquisition backend is configured (§11.1's origin is a
    /// deliberately open question, 2026-08-23). Distinct from `NoResult` so
    /// callers/logs can tell "nothing wired up yet" apart from "a real
    /// backend tried and found nothing" — mirrors `ai::ProviderError::Unconfigured`.
    Unconfigured,
    /// The backend doesn't implement this operation at all — distinct from
    /// `Unconfigured` (which means "no backend chosen yet"). So far this is
    /// only `Source::LocalPdf`: a local library is **matched**, not
    /// **searched** (PLAN.md S28 item 5), so `Source::search`/`Source::fetch`
    /// are the wrong shape for it — see [`LocalPdfSource`]'s own methods.
    Unsupported(&'static str),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::NoResult => write!(f, "no adequate source found"),
            SourceError::Network(e) => write!(f, "acquisition network error: {e}"),
            SourceError::Normalize(e) => write!(f, "could not normalize source: {e}"),
            SourceError::Corpus(e) => write!(f, "corpus error: {e}"),
            SourceError::Unconfigured => {
                write!(f, "no source acquisition backend is configured")
            }
            SourceError::Unsupported(msg) => write!(f, "unsupported for this backend: {msg}"),
        }
    }
}

impl std::error::Error for SourceError {}

impl From<CorpusError> for SourceError {
    fn from(e: CorpusError) -> Self {
        SourceError::Corpus(e)
    }
}

/// Swappable acquisition facade (§11.1). Each variant is one backend; the app
/// asks by intent (search a topic, fetch a hit) without knowing which backend
/// serves it. A new backend is a new variant plus the match arms below — no
/// call-site changes, which is what keeps the open §11.1 question from
/// blocking anything else.
pub enum Source {
    /// Canned content — demo mode and tests, no network.
    Mock(MockSource),
    /// Library Genesis mirror (configured via `LEARNIVE_LIBGEN_URL`).
    LibGen(LibGenSource),
    /// Sci-Hub mirror (configured via `LEARNIVE_SCIHUB_URL`).
    SciHub(SciHubSource),
    /// No acquisition backend is configured (§11.1's origin is deliberately
    /// open, 2026-08-23). Every call fails fast with
    /// [`SourceError::Unconfigured`] instead of silently acquiring nothing —
    /// mirrors `ai::Provider::Unconfigured` (§22).
    Unconfigured,
    /// The local PDF library (§11.1's always-present fallback tier, PLAN.md
    /// S27a) — the user's own `<data>/library/`. **Not wired in as the
    /// active/chosen backend yet**: `api::cold_start::build_source`/
    /// `build_fallback_source` still return `Source::LibGen`/`Source::SciHub`
    /// (S27c/f/g wire this in for real). `search`/`fetch` are the wrong shape
    /// for it — see the `LocalPdf` arms below and [`LocalPdfSource`]'s doc
    /// comment ("matched, not searched").
    LocalPdf(LocalPdfSource),
    // Future, behind the same facade (kept as doc so the seam is explicit):
    //   LibreTexts(LibreTextsSource),
    //   OpenAccess(OpenAccessSource),// DOAB/OAPEN, arXiv/PMC
}

impl Source {
    /// Searches the backend for sources relevant to `query`, cheaply (no fetch).
    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>, SourceError> {
        match self {
            Source::Mock(m) => m.search(query).await,
            Source::LibGen(b) => b.search(query).await,
            Source::SciHub(b) => b.search(query).await,
            Source::Unconfigured => Err(SourceError::Unconfigured),
            Source::LocalPdf(_) => Err(SourceError::Unsupported(
                "local library is matched by bibliographic identity, not searched by \
                 query — use LocalPdfSource::scan/get",
            )),
        }
    }

    /// Fetches and **normalizes** a hit to the internal representation (§11.1).
    /// The caller then stores it once in the immutable [`Corpus`] and reuses it.
    pub async fn fetch(&self, hit: &SearchHit) -> Result<FetchedSource, SourceError> {
        match self {
            Source::Mock(m) => m.fetch(hit).await,
            Source::LibGen(b) => b.fetch(hit).await,
            Source::SciHub(b) => b.fetch(hit).await,
            Source::Unconfigured => Err(SourceError::Unconfigured),
            Source::LocalPdf(_) => Err(SourceError::Unsupported(
                "local library is matched by bibliographic identity, not fetched by \
                 search hit — use LocalPdfSource::scan/get",
            )),
        }
    }
}

/// Normalizes raw PDF bytes into a [`FetchedSource`] (§11.1) shared by the
/// PDF-backed acquisition backends (LibGen/Sci-Hub): the extracted text becomes
/// the single `p:1` section used for retrieval/grounding, and the original
/// bytes are kept as the canonical artifact (§4/§11, PDF is canonical). Text
/// extraction is best-effort — a PDF with no extractable text layer yields a
/// near-empty section rather than failing the whole acquisition.
pub(crate) fn fetched_from_pdf(hit: &SearchHit, pdf: &[u8]) -> Result<FetchedSource, SourceError> {
    let text = extract_pdf_text(pdf);
    let id = corpus_id(&hit.title, &hit.handle);
    Ok(FetchedSource {
        meta: SourceMeta {
            id,
            title: hit.title.clone(),
            authors: hit.authors.clone(),
            kind: hit.kind,
            license: hit.license.clone(),
            origin: hit.origin.clone(),
            pdf_asset: Some("source.pdf".into()),
        },
        sections: vec![Section {
            locator: "p:1".into(),
            title: "Full text".into(),
            text,
        }],
        pdf: Some(pdf.to_vec()),
    })
}

/// `pdf-extract` reads from a file path, so the in-memory PDF is spilled to a
/// uniquely-named temp file, extracted, then removed. Best-effort: any failure
/// yields an empty string rather than failing the acquisition.
fn extract_pdf_text(pdf: &[u8]) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path =
        std::env::temp_dir().join(format!("learnive-pdf-{}-{stamp}.pdf", std::process::id()));
    if std::fs::write(&path, pdf).is_err() {
        return String::new();
    }
    // Per-page extraction, not `pdf_extract::extract_text`'s whole-document
    // call — matches `source::pdf::read_pdf`'s `extract_pages_resilient`
    // fix (2026-08-29, live bug twice the same day, see that function's own
    // doc comment for the full story): a malformed content stream can make
    // the crate PANIC on one page rather than return `Err`, and the
    // whole-document functions abort the ENTIRE extraction on the first
    // such page — verified against a real 1,308-page book where only 12
    // pages were malformed but the whole-document call lost all 1,308.
    // `catch_unwind` per page (not just around one whole-document call)
    // means one bad page degrades to `""` on its own, and also means the
    // cleanup below always runs — a bare panic would otherwise skip it and
    // leak the temp file on top of losing the caller's text.
    let text = extract_pdf_text_resilient(&path);
    let _ = std::fs::remove_file(&path);
    text
}

/// Standalone copy of `source::pdf::read_pdf`'s per-page extraction
/// strategy — deliberately not shared code (this module's own doc comment
/// already states the "independent of `source::pdf`" stance, for the same
/// reason: sharing would mean a caller neither module wants to touch).
/// Unlike that sibling, this caller only wants the flat joined text (no
/// `PageMap`/`OutlineEntry`), so it skips straight to a `String`.
fn extract_pdf_text_resilient(path: &std::path::Path) -> String {
    let Ok(doc) = pdf_extract::Document::load(path) else {
        return String::new();
    };
    let mut page_nums: Vec<u32> = doc.get_pages().keys().copied().collect();
    page_nums.sort_unstable();

    let mut out = String::new();
    for page_num in page_nums {
        let mut s = String::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut output = pdf_extract::PlainTextOutput::new(&mut s);
            pdf_extract::output_doc_page(&doc, &mut output, page_num)
        }));
        if let Ok(Ok(())) = result {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&s);
        }
    }
    out
}

/// Derives a stable corpus id from a title (slug) + a short content hash, so the
/// same source fetched twice lands on the same id and is reused, not duplicated.
pub(crate) fn corpus_id(title: &str, disambiguator: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut slug = String::with_capacity(title.len());
    let mut dash = false;
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
            dash = false;
        } else if !dash && !slug.is_empty() {
            slug.push('-');
            dash = true;
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    hasher.update(b"\0");
    hasher.update(disambiguator.as_bytes());
    let hash = hasher.finalize();
    format!(
        "{slug}-{:x}{:x}{:x}{:x}",
        hash[0], hash[1], hash[2], hash[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(title: &str, kind: SourceKind, size: Option<u64>) -> SearchHit {
        SearchHit {
            title: title.to_string(),
            authors: vec![],
            kind,
            origin: Origin::LibGen,
            license: String::new(),
            handle: format!("https://example/{title}"),
            pages: None,
            size_bytes: size,
        }
    }

    #[test]
    fn pick_best_hit_prefers_book_over_article() {
        let hits = vec![
            hit(
                "A 3-page journal article",
                SourceKind::Article,
                Some(300_000),
            ),
            hit("Linear Algebra textbook", SourceKind::Book, Some(5_000_000)),
        ];
        let best = pick_best_hit(&hits).expect("should pick a hit");
        assert_eq!(best.kind, SourceKind::Book);
        assert_eq!(best.title, "Linear Algebra textbook");
    }

    #[test]
    fn pick_best_hit_falls_back_to_article_when_only_articles() {
        let hits = vec![
            hit("Paper A", SourceKind::Article, Some(200_000)),
            hit("Paper B", SourceKind::Article, Some(900_000)),
        ];
        // No book available: pick the larger article (more content).
        let best = pick_best_hit(&hits).expect("should pick a hit");
        assert_eq!(best.title, "Paper B");
    }

    #[test]
    fn pick_best_hit_prefers_smaller_book_when_several() {
        // Slimmer textbooks download reliably from flaky mirrors; the largest
        // "Manga guide" class of source is the one that resets mid-download.
        let hits = vec![
            hit("Small book", SourceKind::Book, Some(1_000_000)),
            hit("Big book", SourceKind::Book, Some(9_000_000)),
        ];
        let best = pick_best_hit(&hits).expect("should pick a hit");
        assert_eq!(best.title, "Small book");
    }

    #[test]
    fn corpus_id_is_slug_plus_stable_hash() {
        let a = corpus_id("Calculus, Volume 1", "openstax");
        let b = corpus_id("Calculus, Volume 1", "openstax");
        assert_eq!(a, b, "same input → same id (reuse, not duplicate)");
        assert!(a.starts_with("calculus-volume-1-"));
        let c = corpus_id("Calculus, Volume 1", "libretexts");
        assert_ne!(a, c, "disambiguator changes the id");
    }

    #[test]
    fn corpus_id_handles_unicode_and_symbols() {
        let id = corpus_id("Naïve Café Résumé & Exposé!!!", "x");
        assert!(!id.starts_with('-') && !id.contains("--"));
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
