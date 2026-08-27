//! Acervo validation gate — the six-check engine (§11.1, PLAN.md S27c).
//!
//! `SPEC.md`'s "Portão do acervo" paragraph (§11.1) spells out six checks, in
//! cost order, that run against the local library (`<data>/library/`,
//! [`super::local::LocalPdfSource`]) before a reading-list item is allowed to
//! generate content: **presence** (does a matching PDF exist at all?),
//! **identity** (is it *really* the claimed work — not just a right-sounding
//! filename?), **text layer** (extractable, or an image-only scan?),
//! **table of contents** (embedded bookmarks → heuristic over the text → user
//! confirmation — never a hard fail on its own), **page map** (real numbering
//! vs. physical index, for the `#page=N` deep-link), and **retrieval index**
//! (the embeddings pass, paid once per PDF).
//!
//! This module is PLAN.md's own description of S27c: "os seis checks... como
//! função pura sobre a biblioteca + a lista esperada, devolvendo um
//! relatório." Pure Rust, no UI, no caller yet — same pattern S27a/S27b
//! landed with (a passing test suite, nothing wired in). [`validate_acervo`]
//! is the entry point.
//!
//! **Identity is the strict layer, deliberately** (PLAN.md's S27d contrast:
//! *"o portão do acervo é um segundo filtro, muito mais forte"* — the sibling
//! bibliographic-existence check (S27d, not built here) is lenient on
//! purpose because a real PDF still has to pass *this* check; this one runs
//! against the actual file, so a wrong file with a right-sounding filename
//! must not sail through). Matching still uses normalized comparison
//! (lowercase, punctuation-insensitive, optional subtitle) rather than exact
//! strings — SPEC's own words for the lenient sibling check apply here too,
//! just at a stricter bar (title AND at least one author's surname, not
//! title alone).
//!
//! **Scope reduction on the retrieval-index check** (flagged per the task
//! brief, not silently stubbed): building a real embeddings index inline
//! would mean loading `Embedder` (a model download on first use — see
//! `retrieval::Embedder::load`) from a validation pass that otherwise never
//! touches the network or a multi-hundred-millisecond model load. That's a
//! separate design surface (when to trigger it, how it composes with the
//! existing corpus-shaped `VectorIndex`/`Retriever`, whether library PDFs
//! join that same index or a parallel one) that belongs to whichever slice
//! wires this gate into cold start (S27g+), not this one. So the check here
//! only reports **present/missing** against a content-hash-keyed cache under
//! `<data>/index/library/` (the same `<data>/index/` root the existing
//! `Retriever` uses for its own `vectors.json`, just a separate subpath so
//! the two caches never collide). [`build_index_cache`] is the real builder
//! — implemented and exercised by an `#[ignore]`d test (same convention as
//! `retriever_live_end_to_end` in `retrieval/index.rs`), but **not called
//! by [`validate_acervo`]** itself, so a plain validation pass never forces
//! a model download.

use std::fs;
use std::path::{Path, PathBuf};

use super::SourceKind;
use super::local::{LibraryEntry, LocalPdfSource};
use super::matching::{normalize, primary_title, surname_of};
use super::pdf::{OutlineEntry, PdfDocument, read_pdf};
use crate::retrieval::{Embedder, chunk_text};

/// A minimal, standalone description of one reading-list item to validate
/// against the library. **Deliberately not `OutlineItem`/`ProposedOutlineNode`**
/// (the current outline types, `engine`/PLAN.md S27e territory) — those don't
/// carry a source pointer yet and won't until S27e rebuilds outline
/// generation around the reading list. This type is this engine's own small
/// input; expect it to get consumed differently once S27e's real reading-list
/// type exists.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedItem {
    pub title: String,
    /// Authors, "First Last" order assumed for the surname heuristic
    /// ([`surname_of`]) — good enough for matching, not a bibliography
    /// formatter.
    pub authors: Vec<String>,
    pub kind: SourceKind,
}

/// Check 1 (cheapest): does *some* PDF in the library plausibly correspond
/// to the expected item at all? A coarse, title-only signal — the strict
/// title+author confirmation is [`IdentityCheck`], not this one.
#[derive(Debug, Clone, PartialEq)]
pub enum PresenceCheck {
    Found {
        filename: String,
    },
    /// Listed by bibliographic title (SPEC: "o que falta é listado pelo nome
    /// bibliográfico, não pelo nome de arquivo") — there is nothing else to
    /// name it by, since no candidate was found.
    Missing,
}

/// Check 2: is the matched PDF *really* the claimed work — not just a
/// right-sounding filename/title? Normalized title **and** at least one
/// author's surname must both be found in the PDF's embedded metadata or
/// first-page text; a `Book`-kind match also needs a plausible page count
/// (see [`MIN_PLAUSIBLE_BOOK_PAGES`] — the only implausibility signal
/// available until a later slice's reading-list type carries an expected
/// page count of its own).
#[derive(Debug, Clone, PartialEq)]
pub enum IdentityCheck {
    Match,
    Mismatch {
        reason: String,
    },
    /// No candidate to check — [`PresenceCheck`] already failed.
    Skipped,
}

/// Check 3: does the matched PDF have an extractable text layer, or is it an
/// image-only scan with no OCR? A PDF that fails this check "não alimenta
/// índice, grounding nem citação" (SPEC) — must surface here, not later when
/// a node comes out sourceless.
#[derive(Debug, Clone, PartialEq)]
pub enum TextLayerCheck {
    Extractable { chars: usize },
    NoText,
    Skipped,
}

/// Check 4: table of contents, in SPEC's own cascade — embedded bookmarks
/// (the good path) → best-effort heuristic over the extracted text → user
/// confirmation (a later slice's UI, not this one). **Never a hard fail on
/// its own** — [`ItemReport::blocking_failures`] never includes this check,
/// by design, even when the heuristic finds nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum TocCheck {
    Embedded {
        entries: usize,
    },
    Heuristic {
        entries: usize,
    },
    /// Neither the embedded outline nor the heuristic found anything —
    /// still not a block (SPEC: "Nenhum PDF é rejeitado por não ter
    /// bookmarks"); a later slice's confirmation screen is the real net.
    Unavailable,
    Skipped,
}

impl TocCheck {
    /// Whether this result still needs the S27f confirmation screen (SPEC's
    /// cascade step 3) before a chapter/section outline can be built from
    /// it. True for anything short of a real embedded outline.
    pub fn needs_user_confirmation(&self) -> bool {
        !matches!(self, TocCheck::Embedded { .. })
    }
}

/// Check 5: real page numbering (a `/PageLabels` tree) vs. plain physical
/// index. Never blocks — [`super::pdf::PageMap::label`] always has a
/// fallback — but the report should say which mode a citation deep-link
/// (`#page=N`, S27j) will actually be pointing through.
#[derive(Debug, Clone, PartialEq)]
pub enum PageMapCheck {
    Labeled { page_count: usize },
    PhysicalOnly { page_count: usize },
    Skipped,
}

/// Check 6 (most expensive): the retrieval/embeddings index for this PDF's
/// content. **Reports only present/missing against a content-hash-keyed
/// cache** — see the module doc's "scope reduction" note. Actually building
/// the cache is [`build_index_cache`], not triggered here.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexCheck {
    Cached { path: PathBuf },
    Missing,
    Skipped,
}

/// One expected item's outcome across all six checks — expressive enough for
/// a future caller (S27g+) to explain what's wrong and to enforce "a PDF
/// that fails blocks the entire generation" (PLAN.md S27, user decision
/// 2026-08-26) without this module building that caller itself.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemReport {
    pub expected: ExpectedItem,
    pub presence: PresenceCheck,
    pub identity: IdentityCheck,
    pub text_layer: TextLayerCheck,
    pub toc: TocCheck,
    pub page_map: PageMapCheck,
    pub index: IndexCheck,
}

impl ItemReport {
    /// Which checks hard-block generation for this item, per SPEC's own
    /// wording ("PDF que falha bloqueia tudo"). Only presence, identity and
    /// text layer can appear here — TOC never blocks (falls through to
    /// heuristic + confirmation) and the page map never blocks (falls back
    /// to physical numbers). The retrieval index is deliberately **not**
    /// included: this slice only reports whether a cache exists, it doesn't
    /// build one, so treating "missing" as a hard block here would make
    /// every first-time library item unconditionally fail with no way to
    /// resolve it short of code this slice doesn't ship. A caller that wires
    /// this into the real gate (S27g+) should read [`Self::index`]
    /// separately and decide once index-building exists.
    pub fn blocking_failures(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if matches!(self.presence, PresenceCheck::Missing) {
            out.push("presence");
        }
        if matches!(self.identity, IdentityCheck::Mismatch { .. }) {
            out.push("identity");
        }
        if matches!(self.text_layer, TextLayerCheck::NoText) {
            out.push("text_layer");
        }
        out
    }

    pub fn passes(&self) -> bool {
        self.blocking_failures().is_empty()
    }
}

/// The full acervo report: one [`ItemReport`] per expected item, in the same
/// order they were given.
#[derive(Debug, Clone, PartialEq)]
pub struct AcervoReport {
    pub items: Vec<ItemReport>,
}

impl AcervoReport {
    /// Whether every item cleared the hard-blocking checks. Consistent with
    /// SPEC's "sem fonte, sem geração": a caller enforcing the gate should
    /// refuse to start generation unless this is true for the whole reading
    /// list, not just the item about to be read.
    pub fn all_pass(&self) -> bool {
        self.items.iter().all(ItemReport::passes)
    }

    pub fn failing_items(&self) -> Vec<&ItemReport> {
        self.items.iter().filter(|r| !r.passes()).collect()
    }
}

/// A `Book`-kind PDF shorter than this is implausible as *any* real
/// textbook, independent of which book it claims to be — catches "a 3-page
/// excerpt with the right title" (SPEC's own worry) without needing an
/// expected page count, which [`ExpectedItem`] doesn't carry yet. Articles
/// have no equivalent floor; a short paper is completely normal.
const MIN_PLAUSIBLE_BOOK_PAGES: usize = 8;

/// One library PDF, read once and reused across every expected item it's
/// compared against (so a library of N PDFs and M expected items costs N
/// reads, not N×M).
struct LibraryCandidate {
    entry: LibraryEntry,
    /// SHA-256 hex digest of the file's bytes — the retrieval-index cache
    /// key (content-addressed, so renaming the file doesn't force a
    /// rebuild and a byte-identical duplicate reuses the same cache entry).
    hash: String,
    pdf: PdfDocument,
    meta_title: Option<String>,
    meta_author: Option<String>,
}

/// Runs all six checks for every expected item against the scanned library.
/// A library PDF that can't be read at all (corrupt file — see
/// [`read_pdf`]'s own error case) is silently excluded from candidate
/// matching rather than failing the whole pass; it simply can't match
/// anything, which surfaces as [`PresenceCheck::Missing`] for whatever
/// expected item it might have covered.
///
/// `index_cache_dir` should be `<data_dir>/index/library` by convention
/// (parallel to the existing `Retriever`'s `<data_dir>/index/vectors.json` —
/// same `<data>/index/` root, a separate subpath so the two caches never
/// collide).
pub fn validate_acervo(
    library: &LocalPdfSource,
    expected: &[ExpectedItem],
    index_cache_dir: impl AsRef<Path>,
) -> std::io::Result<AcervoReport> {
    let candidates = load_candidates(library)?;
    let index_cache_dir = index_cache_dir.as_ref();
    let items = expected
        .iter()
        .map(|item| build_item_report(item, &candidates, index_cache_dir))
        .collect();
    Ok(AcervoReport { items })
}

fn load_candidates(library: &LocalPdfSource) -> std::io::Result<Vec<LibraryCandidate>> {
    let entries = library.scan()?;
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let path = library.root().join(&entry.filename);
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(pdf) = read_pdf(&path) else {
            // Genuinely unreadable file: not a candidate for anything. Left
            // out rather than erroring the whole validation pass, matching
            // the module's "one bad file must not sink the batch" stance.
            continue;
        };
        let hash = content_hash(&bytes);
        let (meta_title, meta_author) = read_info_metadata(&path);
        out.push(LibraryCandidate {
            entry,
            hash,
            pdf,
            meta_title,
            meta_author,
        });
    }
    Ok(out)
}

fn build_item_report(
    item: &ExpectedItem,
    candidates: &[LibraryCandidate],
    index_cache_dir: &Path,
) -> ItemReport {
    let Some(cand) = find_candidate(item, candidates) else {
        return ItemReport {
            expected: item.clone(),
            presence: PresenceCheck::Missing,
            identity: IdentityCheck::Skipped,
            text_layer: TextLayerCheck::Skipped,
            toc: TocCheck::Skipped,
            page_map: PageMapCheck::Skipped,
            index: IndexCheck::Skipped,
        };
    };

    ItemReport {
        expected: item.clone(),
        presence: PresenceCheck::Found {
            filename: cand.entry.filename.clone(),
        },
        identity: check_identity(item, cand),
        text_layer: check_text_layer(&cand.pdf),
        toc: check_toc(&cand.pdf),
        page_map: check_page_map(&cand.pdf),
        index: check_index_cache(&cand.hash, index_cache_dir),
    }
}

/// The text a candidate is matched against: embedded `/Info` metadata plus
/// the first physical page's extracted text, normalized. First-page text is
/// the SPEC-named identity signal alongside embedded metadata ("metadados
/// embutidos, título/autor na primeira página").
fn candidate_haystack(cand: &LibraryCandidate) -> String {
    let mut s = String::new();
    if let Some(t) = &cand.meta_title {
        s.push_str(&normalize(t));
        s.push(' ');
    }
    if let Some(a) = &cand.meta_author {
        s.push_str(&normalize(a));
        s.push(' ');
    }
    match cand.pdf.page_texts.first() {
        Some(first_page) => s.push_str(&normalize(first_page)),
        None => s.push_str(&normalize(&cand.pdf.text)),
    }
    s
}

/// Presence's coarse title-only match: is there a candidate whose metadata
/// or first page plausibly contains this item's title at all? Picks the
/// first candidate that matches, which is enough for this slice's "pure
/// function over the library" scope — real disambiguation between several
/// plausible candidates is the S27f manual-match screen's job, not this
/// engine's.
fn find_candidate<'a>(
    item: &ExpectedItem,
    candidates: &'a [LibraryCandidate],
) -> Option<&'a LibraryCandidate> {
    let target = normalize(primary_title(&item.title));
    if target.is_empty() {
        return None;
    }
    candidates
        .iter()
        .find(|c| candidate_haystack(c).contains(&target))
}

/// Strict identity confirmation on an already-presence-matched candidate:
/// title (re-checked, cheap) **and** at least one author's surname, plus a
/// page-count plausibility floor for books. This is the check SPEC calls out
/// as the one that must catch "um arquivo errado com nome certo" — a
/// candidate that cleared presence's coarse title match but turns out to be
/// a different work (e.g. a same-titled book by a different author).
fn check_identity(item: &ExpectedItem, cand: &LibraryCandidate) -> IdentityCheck {
    let haystack = candidate_haystack(cand);
    let target_title = normalize(primary_title(&item.title));
    if target_title.is_empty() || !haystack.contains(&target_title) {
        return IdentityCheck::Mismatch {
            reason: format!(
                "expected title \"{}\" was not found in the PDF's metadata or first page",
                item.title
            ),
        };
    }

    if !item.authors.is_empty() {
        let author_ok = item.authors.iter().any(|a| {
            let surname = normalize(surname_of(a));
            !surname.is_empty() && haystack.contains(&surname)
        });
        if !author_ok {
            return IdentityCheck::Mismatch {
                reason: format!(
                    "none of the expected authors ({:?}) were found in the PDF's metadata or \
                     first page — this looks like a different work with a matching title",
                    item.authors
                ),
            };
        }
    }

    if item.kind == SourceKind::Book && cand.pdf.pages.page_count < MIN_PLAUSIBLE_BOOK_PAGES {
        return IdentityCheck::Mismatch {
            reason: format!(
                "only {} page(s) — too short to plausibly be the claimed book",
                cand.pdf.pages.page_count
            ),
        };
    }

    IdentityCheck::Match
}

fn check_text_layer(pdf: &PdfDocument) -> TextLayerCheck {
    let trimmed = pdf.text.trim();
    if trimmed.is_empty() {
        TextLayerCheck::NoText
    } else {
        TextLayerCheck::Extractable {
            chars: trimmed.chars().count(),
        }
    }
}

fn check_toc(pdf: &PdfDocument) -> TocCheck {
    if !pdf.outline.is_empty() {
        return TocCheck::Embedded {
            entries: count_outline_entries(&pdf.outline),
        };
    }
    let heuristic = heuristic_toc(pdf);
    if heuristic.is_empty() {
        TocCheck::Unavailable
    } else {
        TocCheck::Heuristic {
            entries: heuristic.len(),
        }
    }
}

fn check_page_map(pdf: &PdfDocument) -> PageMapCheck {
    if pdf.pages.has_labels() {
        PageMapCheck::Labeled {
            page_count: pdf.pages.page_count,
        }
    } else {
        PageMapCheck::PhysicalOnly {
            page_count: pdf.pages.page_count,
        }
    }
}

fn check_index_cache(content_hash: &str, index_cache_dir: &Path) -> IndexCheck {
    let path = index_cache_dir.join(format!("{content_hash}.json"));
    if path.is_file() {
        IndexCheck::Cached { path }
    } else {
        IndexCheck::Missing
    }
}

/// Builds and persists a minimal chunk+embedding cache for one PDF's
/// extracted text, keyed by its content hash. The real builder behind check
/// 6 — **not called by [`validate_acervo`]** (see the module doc's scope
/// note): a plain validation pass must never force a model download. A
/// caller that decides to actually pay for the index (S27g+) calls this
/// directly, then re-runs [`validate_acervo`] (or just checks
/// [`IndexCheck::Cached`] itself) to see it reflected.
///
/// The cache format here (`Vec<(chunk text, vector)>`) is deliberately
/// minimal, not the corpus-shaped `retrieval::VectorIndex` — how library
/// PDFs' chunks eventually join real grounding/retrieval (their own index,
/// or folded into the existing one) is exactly the "genuinely large, separate
/// design surface" this slice defers, per the task brief.
pub fn build_index_cache(
    pdf: &PdfDocument,
    content_hash: &str,
    index_cache_dir: &Path,
    embedder: &Embedder,
) -> std::io::Result<PathBuf> {
    fs::create_dir_all(index_cache_dir)?;
    let path = index_cache_dir.join(format!("{content_hash}.json"));

    let chunks = chunk_text(&pdf.text);
    let vectors = if chunks.is_empty() {
        Vec::new()
    } else {
        embedder.embed_batch(&chunks)
    };
    let cached: Vec<(String, Vec<f32>)> = chunks.into_iter().zip(vectors).collect();

    let json = serde_json::to_vec(&cached)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

fn count_outline_entries(entries: &[OutlineEntry]) -> usize {
    entries
        .iter()
        .map(|e| 1 + count_outline_entries(&e.children))
        .sum()
}

/// Best-effort chapter/heading detection over extracted text, used only when
/// the PDF has no embedded `/Outlines` — never a hard TOC failure by itself
/// (SPEC's cascade: bookmarks → heuristic → user confirmation). Two simple,
/// independent signals, tried in order: (1) a literal contents/sumário page,
/// whose lines ending in a page number look like a real TOC; (2) failing
/// that, "Chapter N" / numbered-heading lines (`1.2 Title`) scattered through
/// the body. Deliberately unsophisticated — the real safety net is the S27f
/// user-confirmation screen, not this heuristic.
fn heuristic_toc(pdf: &PdfDocument) -> Vec<String> {
    let from_contents_page = toc_page_heuristic(pdf);
    if !from_contents_page.is_empty() {
        return from_contents_page;
    }
    heading_line_heuristic(pdf)
}

fn toc_page_heuristic(pdf: &PdfDocument) -> Vec<String> {
    for page in &pdf.page_texts {
        let head: String = page.chars().take(60).collect::<String>().to_lowercase();
        if !(head.contains("contents") || head.contains("sumário") || head.contains("sumario")) {
            continue;
        }
        let entries: Vec<String> = page
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                let last_tok = trimmed.split_whitespace().next_back()?;
                let looks_like_page_number = !last_tok.is_empty()
                    && last_tok.len() <= 4
                    && last_tok.chars().all(|c| c.is_ascii_digit());
                if !looks_like_page_number {
                    return None;
                }
                let title_part = trimmed[..trimmed.len() - last_tok.len()]
                    .trim_end_matches(['.', ' '])
                    .trim();
                (!title_part.is_empty()).then(|| title_part.to_string())
            })
            .collect();
        if !entries.is_empty() {
            return entries;
        }
    }
    Vec::new()
}

fn heading_line_heuristic(pdf: &PdfDocument) -> Vec<String> {
    let mut out = Vec::new();
    for page in &pdf.page_texts {
        for line in page.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let lower = trimmed.to_lowercase();
            if lower.starts_with("chapter ")
                || lower.starts_with("capítulo ")
                || lower.starts_with("capitulo ")
                || looks_like_numbered_heading(trimmed)
            {
                out.push(trimmed.to_string());
            }
        }
    }
    out
}

/// `"1.2 Derivatives"`-shaped lines: a leading run of digits/dots, then a
/// capitalized word, on a short line (headings aren't paragraphs).
fn looks_like_numbered_heading(line: &str) -> bool {
    if line.len() >= 80 {
        return false;
    }
    let Some((number, rest)) = line.split_once(char::is_whitespace) else {
        return false;
    };
    let is_number = !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit() || c == '.')
        && number.chars().any(|c| c.is_ascii_digit());
    let rest = rest.trim();
    is_number && rest.chars().next().is_some_and(|c| c.is_uppercase())
}

/// SHA-256 hex digest of a PDF's bytes — the content-addressed key both the
/// retrieval-index cache and [`build_index_cache`] use.
fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Reads `/Info` dictionary `Title`/`Author` strings, when present. Loads its
/// own `lopdf::Document` rather than extending [`PdfDocument`] with metadata
/// fields — keeps `pdf.rs`'s existing shape (text/outline/pages, all it
/// promises today) untouched, at the cost of parsing the PDF a second time.
/// Fine for a validation pass that runs once per library item, not a hot
/// path.
fn read_info_metadata(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(doc) = lopdf::Document::load(path) else {
        return (None, None);
    };
    let info_dict = doc
        .trailer
        .get(b"Info")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .and_then(|id| doc.get_object(id).ok())
        .and_then(|o| o.as_dict().ok());
    let field = |name: &[u8]| {
        info_dict
            .and_then(|d| d.get(name).ok())
            .and_then(|o| o.as_str().ok())
            .map(|b| String::from_utf8_lossy(b).into_owned())
    };
    (field(b"Title"), field(b"Author"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::content::{Content, Operation};
    use lopdf::{Bookmark, Document, Object, ObjectId, Stream, dictionary};

    /// Builds a multi-page PDF with real per-page text and, optionally,
    /// `/Info` `Title`/`Author` metadata — the same builder-API technique
    /// `pdf.rs`'s own tests use (no byte-literal fixture, no real book
    /// content needed for validation-logic tests).
    fn build_document(
        page_texts: &[&str],
        info_title: Option<&str>,
        info_author: Option<&str>,
    ) -> (Document, Vec<ObjectId>) {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });

        let mut page_ids = Vec::new();
        for text in page_texts {
            let content = if text.is_empty() {
                Content { operations: vec![] }
            } else {
                Content {
                    operations: vec![
                        Operation::new("BT", vec![]),
                        Operation::new("Tf", vec!["F1".into(), 12.into()]),
                        Operation::new("Td", vec![72.into(), 700.into()]),
                        Operation::new("Tj", vec![Object::string_literal(*text)]),
                        Operation::new("ET", vec![]),
                    ],
                }
            };
            let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            });
            page_ids.push(page_id);
        }

        let count = page_ids.len() as i64;
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids.iter().map(|&id| id.into()).collect::<Vec<Object>>(),
                "Count" => count,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        if info_title.is_some() || info_author.is_some() {
            let mut info = dictionary! {};
            if let Some(t) = info_title {
                info.set("Title", Object::string_literal(t));
            }
            if let Some(a) = info_author {
                info.set("Author", Object::string_literal(a));
            }
            let info_id = doc.add_object(info);
            doc.trailer.set("Info", info_id);
        }

        (doc, page_ids)
    }

    /// Saves the PDF straight into a temp `<data>/library/` directory (as a
    /// user would drop it by hand) and opens a [`LocalPdfSource`] over it.
    /// The `TempDir` guard must be kept alive by the caller for as long as
    /// the library is used.
    fn place_in_library(doc: &mut Document, filename: &str) -> (tempfile::TempDir, LocalPdfSource) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let lib = LocalPdfSource::open(dir.path().join("data")).expect("open library");
        doc.save(lib.root().join(filename))
            .expect("save fixture pdf");
        (dir, lib)
    }

    fn empty_library() -> (tempfile::TempDir, LocalPdfSource) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let lib = LocalPdfSource::open(dir.path().join("data")).expect("open library");
        (dir, lib)
    }

    fn index_dir(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join("data").join("index").join("library")
    }

    // -- Presence -----------------------------------------------------

    #[test]
    fn presence_fails_when_no_matching_pdf_exists() {
        let (tmp, lib) = empty_library();
        let expected = vec![ExpectedItem {
            title: "Introduction to the Theory of Computation".into(),
            authors: vec!["Michael Sipser".into()],
            kind: SourceKind::Book,
        }];

        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        assert_eq!(report.items.len(), 1);
        let item = &report.items[0];
        assert_eq!(item.presence, PresenceCheck::Missing);
        assert_eq!(item.identity, IdentityCheck::Skipped);
        assert_eq!(item.text_layer, TextLayerCheck::Skipped);
        assert_eq!(item.toc, TocCheck::Skipped);
        assert_eq!(item.page_map, PageMapCheck::Skipped);
        assert_eq!(item.index, IndexCheck::Skipped);
        assert!(!item.passes());
        assert_eq!(item.blocking_failures(), vec!["presence"]);
    }

    #[test]
    fn presence_fails_when_library_has_only_unrelated_pdfs() {
        let (mut doc, _pages) = build_document(
            &["A completely unrelated cookbook, by Jane Chef."],
            Some("The Joy of Baking"),
            Some("Jane Chef"),
        );
        let (tmp, lib) = place_in_library(&mut doc, "baking.pdf");

        let expected = vec![ExpectedItem {
            title: "Introduction to the Theory of Computation".into(),
            authors: vec!["Michael Sipser".into()],
            kind: SourceKind::Book,
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        assert_eq!(report.items[0].presence, PresenceCheck::Missing);
    }

    // -- Identity -------------------------------------------------------

    #[test]
    fn identity_fails_when_the_author_genuinely_does_not_match() {
        // Right-sounding title, wrong actual work — exactly SPEC's worry
        // ("um arquivo errado com nome certo envenena o grounding em
        // silêncio").
        let (mut doc, _pages) = build_document(
            &["Introduction to the Theory of Computation, by Someone Else."],
            Some("Introduction to the Theory of Computation"),
            Some("Someone Else"),
        );
        let (tmp, lib) = place_in_library(&mut doc, "sipser.pdf");

        let expected = vec![ExpectedItem {
            title: "Introduction to the Theory of Computation".into(),
            authors: vec!["Michael Sipser".into()],
            kind: SourceKind::Article, // sidestep the book page-count floor
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        let item = &report.items[0];
        assert_eq!(
            item.presence,
            PresenceCheck::Found {
                filename: "sipser.pdf".into()
            },
            "the title match is enough to find a candidate"
        );
        assert!(
            matches!(item.identity, IdentityCheck::Mismatch { .. }),
            "the author does not match, so identity must fail: {:?}",
            item.identity
        );
        assert_eq!(item.blocking_failures(), vec!["identity"]);
        assert!(!item.passes());
    }

    #[test]
    fn identity_fails_when_a_book_is_implausibly_short() {
        let (mut doc, _pages) = build_document(
            &["Introduction to the Theory of Computation, by Michael Sipser."],
            Some("Introduction to the Theory of Computation"),
            Some("Michael Sipser"),
        );
        let (tmp, lib) = place_in_library(&mut doc, "sipser-excerpt.pdf");

        let expected = vec![ExpectedItem {
            title: "Introduction to the Theory of Computation".into(),
            authors: vec!["Michael Sipser".into()],
            kind: SourceKind::Book,
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        let item = &report.items[0];
        assert!(
            matches!(item.identity, IdentityCheck::Mismatch { .. }),
            "a 1-page 'book' must not pass identity: {:?}",
            item.identity
        );
    }

    #[test]
    fn identity_passes_when_title_and_author_both_match() {
        let pages: Vec<&str> =
            vec!["Introduction to the Theory of Computation, Michael Sipser."; 10];
        let (mut doc, _pages) = build_document(
            &pages,
            Some("Introduction to the Theory of Computation"),
            Some("Michael Sipser"),
        );
        let (tmp, lib) = place_in_library(&mut doc, "sipser.pdf");

        let expected = vec![ExpectedItem {
            title: "Introduction to the Theory of Computation".into(),
            authors: vec!["Michael Sipser".into()],
            kind: SourceKind::Book,
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        let item = &report.items[0];
        assert_eq!(item.identity, IdentityCheck::Match);
        assert_eq!(item.blocking_failures(), Vec::<&str>::new());
    }

    // -- Text layer -------------------------------------------------------

    #[test]
    fn text_layer_passes_with_a_normal_text_layer() {
        let (mut doc, _pages) = build_document(
            &["Real extractable body text about computation."],
            Some("Real Book"),
            None,
        );
        let (tmp, lib) = place_in_library(&mut doc, "real.pdf");

        let expected = vec![ExpectedItem {
            title: "Real Book".into(),
            authors: vec![],
            kind: SourceKind::Article,
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        assert!(matches!(
            report.items[0].text_layer,
            TextLayerCheck::Extractable { .. }
        ));
    }

    #[test]
    fn text_layer_fails_without_a_panic_on_an_image_only_pdf() {
        // No text operations at all on the page — the closest a
        // programmatically-built fixture gets to an image-only scan
        // (`pdf-extract` yields an empty string either way; the check must
        // not panic on it).
        let (mut doc, _pages) = build_document(&[""], Some("Scanned Book"), None);
        let (tmp, lib) = place_in_library(&mut doc, "scanned.pdf");

        let expected = vec![ExpectedItem {
            title: "Scanned Book".into(),
            authors: vec![],
            kind: SourceKind::Article,
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        assert_eq!(report.items[0].text_layer, TextLayerCheck::NoText);
        assert_eq!(report.items[0].blocking_failures(), vec!["text_layer"]);
    }

    // -- Table of contents ------------------------------------------------

    #[test]
    fn toc_passes_via_embedded_bookmarks() {
        let (mut doc, pages) = build_document(
            &["Cover page.", "Chapter 1 body.", "Chapter 2 body."],
            Some("Bookmarked Book"),
            None,
        );
        let ch1 = doc.add_bookmark(
            Bookmark::new("Chapter 1".to_string(), [0.0, 0.0, 0.0], 0, pages[1]),
            None,
        );
        let _ = ch1;
        doc.add_bookmark(
            Bookmark::new("Chapter 2".to_string(), [0.0, 0.0, 0.0], 0, pages[2]),
            None,
        );
        let outlines_id = doc.build_outline().expect("bookmarks were added");
        doc.catalog_mut()
            .expect("catalog exists")
            .set("Outlines", outlines_id);

        let (tmp, lib) = place_in_library(&mut doc, "bookmarked.pdf");
        let expected = vec![ExpectedItem {
            title: "Bookmarked Book".into(),
            authors: vec![],
            kind: SourceKind::Article,
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        assert_eq!(report.items[0].toc, TocCheck::Embedded { entries: 2 });
        assert!(!report.items[0].toc.needs_user_confirmation());
    }

    #[test]
    fn toc_falls_through_to_heuristic_and_never_hard_fails_without_bookmarks() {
        // No embedded outline; a page whose text looks like a table of
        // contents (heading lines ending in a page number).
        let toc_page = "Contents\nChapter One .... 1\nChapter Two .... 12\n";
        let (mut doc, _pages) = build_document(
            &[toc_page, "Chapter One body.", "Chapter Two body."],
            Some("Heuristic Book"),
            None,
        );
        let (tmp, lib) = place_in_library(&mut doc, "heuristic.pdf");

        let expected = vec![ExpectedItem {
            title: "Heuristic Book".into(),
            authors: vec![],
            kind: SourceKind::Article,
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        let item = &report.items[0];
        assert!(
            matches!(item.toc, TocCheck::Heuristic { entries } if entries >= 2),
            "expected a heuristic hit with at least 2 entries, got {:?}",
            item.toc
        );
        assert!(item.toc.needs_user_confirmation());
        // Never a hard fail on its own, even mid-cascade.
        assert!(!item.blocking_failures().contains(&"toc"));
        assert!(item.passes());
    }

    #[test]
    fn toc_is_unavailable_but_still_not_a_hard_fail_when_nothing_is_found() {
        let (mut doc, _pages) = build_document(
            &["Just some plain prose with no headings or contents page at all."],
            Some("Plain Book"),
            None,
        );
        let (tmp, lib) = place_in_library(&mut doc, "plain.pdf");

        let expected = vec![ExpectedItem {
            title: "Plain Book".into(),
            authors: vec![],
            kind: SourceKind::Article,
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        let item = &report.items[0];
        assert_eq!(item.toc, TocCheck::Unavailable);
        assert!(item.toc.needs_user_confirmation());
        assert!(
            item.passes(),
            "a missing TOC alone must never block generation: {item:?}"
        );
    }

    // -- Page map -----------------------------------------------------

    #[test]
    fn page_map_reports_physical_only_when_the_pdf_has_no_page_labels() {
        let (mut doc, _pages) =
            build_document(&["One", "Two", "Three"], Some("No Labels Book"), None);
        let (tmp, lib) = place_in_library(&mut doc, "nolabels.pdf");

        let expected = vec![ExpectedItem {
            title: "No Labels Book".into(),
            authors: vec![],
            kind: SourceKind::Article,
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        assert_eq!(
            report.items[0].page_map,
            PageMapCheck::PhysicalOnly { page_count: 3 }
        );
    }

    #[test]
    fn page_map_reports_labeled_when_the_pdf_has_real_page_labels() {
        let (mut doc, _pages) =
            build_document(&["Cover", "Ch1 p1", "Ch1 p2"], Some("Labeled Book"), None);
        let nums = vec![
            0.into(),
            Object::Dictionary(dictionary! { "S" => "r" }),
            1.into(),
            Object::Dictionary(dictionary! { "S" => "D", "St" => 1 }),
        ];
        let page_labels_id = doc.add_object(dictionary! { "Nums" => nums });
        doc.catalog_mut()
            .expect("catalog exists")
            .set("PageLabels", page_labels_id);

        let (tmp, lib) = place_in_library(&mut doc, "labeled.pdf");
        let expected = vec![ExpectedItem {
            title: "Labeled Book".into(),
            authors: vec![],
            kind: SourceKind::Article,
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        assert_eq!(
            report.items[0].page_map,
            PageMapCheck::Labeled { page_count: 3 }
        );
    }

    // -- Retrieval index ------------------------------------------------

    #[test]
    fn index_check_reports_missing_when_nothing_is_cached() {
        let (mut doc, _pages) = build_document(&["Some body text."], Some("Index Book"), None);
        let (tmp, lib) = place_in_library(&mut doc, "indexed.pdf");

        let expected = vec![ExpectedItem {
            title: "Index Book".into(),
            authors: vec![],
            kind: SourceKind::Article,
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        assert_eq!(report.items[0].index, IndexCheck::Missing);
        // Missing index is reported but deliberately not a blocker in this
        // slice's scope (see the module doc's scope-reduction note).
        assert!(!report.items[0].blocking_failures().contains(&"index"));
    }

    #[test]
    fn index_check_reports_cached_once_a_cache_file_exists() {
        let (mut doc, _pages) = build_document(&["Some body text."], Some("Index Book"), None);
        let (tmp, lib) = place_in_library(&mut doc, "indexed.pdf");
        let cache_dir = index_dir(&tmp);
        fs::create_dir_all(&cache_dir).unwrap();

        // Compute the same hash validate_acervo will use, and pre-seed a
        // cache entry directly (no embedder needed for this test — it only
        // proves the presence check, not the builder).
        let bytes = fs::read(lib.root().join("indexed.pdf")).unwrap();
        let hash = content_hash(&bytes);
        fs::write(cache_dir.join(format!("{hash}.json")), b"[]").unwrap();

        let expected = vec![ExpectedItem {
            title: "Index Book".into(),
            authors: vec![],
            kind: SourceKind::Article,
        }];
        let report = validate_acervo(&lib, &expected, &cache_dir).expect("validate");
        assert!(matches!(report.items[0].index, IndexCheck::Cached { .. }));
    }

    /// Exercises the real builder end-to-end with a live embedder — proves
    /// the "documented follow-up" isn't just prose. Ignored by default (downloads
    /// the embedding model); run with
    /// `cargo test -p learnive index_cache_round_trip_with_a_real_embedder -- --ignored`.
    #[test]
    #[ignore = "downloads the embedding model"]
    fn index_cache_round_trip_with_a_real_embedder() {
        let (mut doc, _pages) = build_document(
            &["A limit describes the value a function approaches."],
            Some("Calculus"),
            None,
        );
        let (tmp, lib) = place_in_library(&mut doc, "calculus.pdf");
        let cache_dir = index_dir(&tmp);

        let path = lib.root().join("calculus.pdf");
        let bytes = fs::read(&path).unwrap();
        let hash = content_hash(&bytes);
        let pdf = read_pdf(&path).unwrap();

        let embedder = Embedder::default_model().expect("load embedder");
        let built = build_index_cache(&pdf, &hash, &cache_dir, &embedder).expect("build cache");
        assert!(built.is_file());

        let expected = vec![ExpectedItem {
            title: "Calculus".into(),
            authors: vec![],
            kind: SourceKind::Article,
        }];
        let report = validate_acervo(&lib, &expected, &cache_dir).expect("validate");
        assert_eq!(report.items[0].index, IndexCheck::Cached { path: built });
    }

    // -- Full report shape ------------------------------------------------

    #[test]
    fn a_fully_matching_well_formed_pdf_passes_every_blocking_check() {
        let pages: Vec<&str> =
            vec!["Introduction to the Theory of Computation, Michael Sipser."; 10];
        let (mut doc, page_ids) = build_document(
            &pages,
            Some("Introduction to the Theory of Computation"),
            Some("Michael Sipser"),
        );
        doc.add_bookmark(
            Bookmark::new("Chapter 1".to_string(), [0.0, 0.0, 0.0], 0, page_ids[1]),
            None,
        );
        let outlines_id = doc.build_outline().expect("bookmarks were added");
        doc.catalog_mut()
            .expect("catalog exists")
            .set("Outlines", outlines_id);

        let (tmp, lib) = place_in_library(&mut doc, "sipser.pdf");
        let expected = vec![ExpectedItem {
            title: "Introduction to the Theory of Computation".into(),
            authors: vec!["Michael Sipser".into()],
            kind: SourceKind::Book,
        }];
        let report = validate_acervo(&lib, &expected, index_dir(&tmp)).expect("validate");
        let item = &report.items[0];
        assert!(item.passes(), "{item:?}");
        assert!(report.all_pass());
        assert!(report.failing_items().is_empty());
    }
}
