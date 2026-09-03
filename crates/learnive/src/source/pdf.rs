//! Reading a PDF's structure: text, embedded outline (bookmarks), and page
//! map (§11, PLAN.md S27b).
//!
//! `pdf-extract` (already a dependency — see `fetched_from_pdf`/`extract_pdf_text`
//! in `source/mod.rs`, used by the remote PDF-backed acquisition backends) only
//! extracts body text; it does not read a PDF's `/Outlines` (bookmarks) or
//! `/PageLabels` (front-matter numbering distinct from physical page index).
//! Both matter for this pivot: the sumário is the input to contextual
//! chapter/section outline expansion (§6.3, later slices S27e/g), and the page
//! map is what a citation deep-link (`#page=N`, S27j) needs to point at the
//! page a book itself calls "page N", not just the Nth physical sheet.
//! `lopdf` reads both.
//!
//! **Deliberately independent of `fetched_from_pdf`/`extract_pdf_text`** even
//! though it duplicates the "spill best-effort, never panic" shape — sharing
//! that code would mean touching its call sites, which live in the
//! LibGen/Sci-Hub backends this slice must not touch (PLAN.md S27b brief).
//!
//! **Path-based entry point, not bytes:** `Source::LocalPdf`'s library
//! (`source/local.rs`) hands out PDFs already sitting on disk under
//! `<data>/library/`, so there is no in-memory-only caller to serve, and
//! `pdf-extract`'s own API is path-based anyway.
//!
//! **Best-effort like `extract_pdf_text`,** but only for the *parsing*, not
//! for reading the file: a PDF with no `/Outlines` yields an empty outline,
//! not an error; a PDF with no `/PageLabels` falls back to plain physical
//! page numbers ("1", "2", ...), not an error. Only a genuinely
//! unreadable/corrupt file (`lopdf::Document::load` failing outright) is an
//! error here.
//!
//! No caller yet — this is a standalone, testable utility, matching how S27a
//! landed with a passing test and no caller. Wiring into `Source::LocalPdf`
//! and the acervo gate is S27c/e and later, out of this slice's scope.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One entry in a PDF's embedded outline (bookmark) tree. Mirrors the PDF's
/// real `/Outlines` nesting (chapter → section → ...) rather than flattening
/// it: the pivot's node granularity is exactly chapter/section (§6.3), and
/// that hierarchy is real information the PDF already carries for free —
/// losing it here would mean re-deriving it heuristically later for no
/// reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutlineEntry {
    pub title: String,
    /// 1-based physical page the bookmark's destination points at.
    pub page: usize,
    pub children: Vec<OutlineEntry>,
}

/// A single `/PageLabels` number-tree run: from physical page `start`
/// (0-based) onward, until the next run's `start`, pages are labelled with
/// `style` (or no numeric part at all, if the PDF omitted `/S`) prefixed by
/// `prefix`, counting up from `start_num`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PageLabelRun {
    start: usize,
    style: Option<LabelStyle>,
    prefix: String,
    start_num: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum LabelStyle {
    Decimal,
    UpperRoman,
    LowerRoman,
    UpperAlpha,
    LowerAlpha,
}

/// A PDF's page-numbering map: physical page count, plus — when the PDF
/// carries a `/PageLabels` number tree — the label the book itself uses for
/// a given physical page (front matter in roman numerals, body in arabic,
/// etc.). Falls back to the plain 1-based physical page number when the PDF
/// has no `/PageLabels`, which is the common case and must never be an
/// error.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PageMap {
    pub page_count: usize,
    labels: Vec<PageLabelRun>,
}

impl PageMap {
    /// Whether the PDF carried a real `/PageLabels` number tree, rather than
    /// [`label`](Self::label) falling back to plain physical numbering. Added
    /// for PLAN.md S27c's acervo-gate page-map check, which needs to report
    /// *which* mode applied (per §11.1: real numbering is a pass, physical
    /// fallback is also a pass — neither blocks — but the report should say
    /// which one it saw).
    pub fn has_labels(&self) -> bool {
        !self.labels.is_empty()
    }

    /// The display label for a 1-based physical page. Falls back to the
    /// plain physical page number (as a string) when the PDF has no
    /// `/PageLabels` at all (no runs to match against). When it does, the
    /// last run extends to cover every later page (a number tree has no
    /// explicit end), so a page past the document's own page count still
    /// gets a computed label, not a fallback — callers that care should
    /// bound `page` by `page_count` themselves.
    pub fn label(&self, page: usize) -> String {
        if page == 0 {
            return page.to_string();
        }
        let idx0 = page - 1;
        match self.labels.iter().rev().find(|run| run.start <= idx0) {
            None => page.to_string(),
            Some(run) => {
                let n = run.start_num + (idx0 - run.start) as i64;
                let numeric = match run.style {
                    Some(LabelStyle::Decimal) => n.to_string(),
                    Some(LabelStyle::UpperRoman) => to_roman(n),
                    Some(LabelStyle::LowerRoman) => to_roman(n).to_lowercase(),
                    Some(LabelStyle::UpperAlpha) => to_alpha(n, true),
                    Some(LabelStyle::LowerAlpha) => to_alpha(n, false),
                    None => String::new(),
                };
                format!("{}{numeric}", run.prefix)
            }
        }
    }
}

/// A PDF's extracted structure: index-only text (never displayed — §11,
/// the original PDF stays the canonical, displayed artifact), the same text
/// split per physical page, the embedded outline tree, and the page map.
///
/// `page_texts` is carried alongside `text` because a later slice needs it:
/// the outline gives a chapter's starting *page*, and slicing that chapter's
/// own text out of one flat string would mean re-deriving page boundaries
/// this module already computed once. **`text` is derived from
/// `page_texts`** (`page_texts.join("\n")`, see [`read_pdf`]) — the other
/// way around, on purpose: extraction runs per page (`extract_pages_resilient`)
/// so one page's malformed content stream can't take the rest of the
/// document's text down with it (2026-08-29 live bug, see `read_pdf`'s doc
/// comment), so `page_texts` is the real source and `text` is just its join,
/// for callers that don't care about page boundaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PdfDocument {
    pub text: String,
    /// `text`, split per 1-based physical page (`page_texts[0]` is page 1).
    pub page_texts: Vec<String>,
    pub outline: Vec<OutlineEntry>,
    pub pages: PageMap,
    /// True when the page content streams DO contain text-showing operators
    /// (`Tj`/`TJ`/`'`/`"`) even though [`Self::text`] came out empty — i.e.
    /// the book has a real text layer and **our extractor is what failed**.
    ///
    /// This distinction is user-facing, not cosmetic. Measured 2026-08-30:
    /// K&R (a scan carrying a perfectly good invisible OCR layer — `3 Tr`,
    /// non-embedded WinAnsi Helvetica, hex-encoded ASCII) extracts as 236 of
    /// 236 EMPTY pages under `pdf_extract`, while poppler's `pdftotext` reads
    /// the same file fine. Without this flag the acervo gate reports "no text
    /// layer", which tells the user to go re-acquire a book that is already
    /// correct — the worst possible instruction under the acquisition route
    /// where the user pays for every download by hand (SPEC §11.1).
    ///
    /// Only computed when extraction produced nothing, so a healthy book
    /// never pays for the scan.
    pub text_layer_unreadable: bool,
    /// `/Info` `Title`, read in the same `lopdf` pass that produces the
    /// outline and page map. Carried here so the content-hash cache below
    /// carries it too (S32, bug reported live 2026-09-03): `load_candidates`
    /// used to call `read_info_metadata` — a SECOND full `lopdf::Document::
    /// load` per file, per validation — even when everything else came from
    /// this cache, so a warm, fully-indexed library still paid a full
    /// structural reparse of every book on every pass. `#[serde(default)]`
    /// because entries written before this field existed must keep loading.
    #[serde(default)]
    pub meta_title: Option<String>,
    /// `/Info` `Author` — same rationale as [`Self::meta_title`].
    #[serde(default)]
    pub meta_author: Option<String>,
    /// Whether [`Self::meta_title`]/[`Self::meta_author`] were actually read
    /// from the file's `/Info` dictionary — `true` for every entry
    /// [`read_pdf`] produced (the same `lopdf` pass that parses the document
    /// reads `/Info`), and `false` only for entries written before the
    /// metadata fields existed. That distinction is load-bearing: `None`
    /// alone can't distinguish "the PDF has no `/Info` title" from "this
    /// entry predates the field", and the difference matters — a scanned
    /// book whose first page extracts with per-glyph spacing ("C ALCULUS",
    /// Stewart, reported live 2026-09-03) has NO clean title in its text, so
    /// `/Info` is the only signal the acervo matching layers have. An
    /// unprobed entry gets exactly one probe-and-rewrite in
    /// [`read_pdf_cached`], never a reparse per validation.
    #[serde(default)]
    pub meta_probed: bool,
}

/// A genuinely unreadable/corrupt PDF — the only failure mode this module
/// surfaces as an error (see the module doc's best-effort-for-parsing rule).
#[derive(Debug)]
pub struct PdfReadError(String);

impl std::fmt::Display for PdfReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PdfReadError {}

/// Reads ONLY the embedded outline, skipping text extraction entirely —
/// test-only (`source::toc_bench`'s S27g measurement harness), never
/// compiled into the binary. [`read_pdf`] spends nearly all its time in
/// [`extract_pages_resilient`] (222s for a 1,300-page textbook); a harness
/// that just wants each library book's table of contents doesn't need a
/// single page of text. Degrades to an empty outline on any failure, same
/// best-effort rule as [`read_outline`] itself.
#[cfg(test)]
pub(crate) fn read_outline_for_test(path: impl AsRef<Path>) -> Vec<OutlineEntry> {
    match lopdf::Document::load(path.as_ref()) {
        Ok(doc) => read_outline(&doc),
        Err(_) => Vec::new(),
    }
}

/// Reads a PDF from disk: extracted text (best-effort, empty string on
/// failure — mirrors `extract_pdf_text`'s convention), the embedded outline
/// (empty when absent), and the page map (plain physical numbers when no
/// `/PageLabels`). Errors only when the file itself cannot be parsed as a
/// PDF at all.
pub fn read_pdf(path: impl AsRef<Path>) -> Result<PdfDocument, PdfReadError> {
    let path = path.as_ref();
    let doc = lopdf::Document::load(path)
        .map_err(|e| PdfReadError(format!("failed to read {}: {e}", path.display())))?;

    let page_count = doc.get_pages().len();
    let outline = read_outline(&doc);
    let labels = read_page_labels(&doc);
    let (meta_title, meta_author) = read_info(&doc);
    // `pdf_extract::extract_text{,_by_pages}` doesn't just return `Err` on a
    // malformed content stream — a real library PDF (reported live,
    // 2026-08-29) hit a content operator (`w`, set line width) with zero
    // operands and PANICKED inside the crate (`operands[0]` indexed
    // unconditionally, pdf-extract 0.12.0), which unwound straight through
    // `.unwrap_or_default()` and crashed the `spawn_blocking` task calling
    // this (`source::acervo::validate_acervo`) — the module doc's own
    // "best-effort, never an error for extraction" promise didn't hold
    // because a panic isn't a `Result`. A first fix wrapped the two
    // whole-document calls in `catch_unwind` (closed the crash), but the
    // SAME live book (reported again, same day) then came back "no text
    // layer" for a real book with genuine, selectable text (confirmed
    // independently with `pdftotext` and Python's `pypdf`, both read it
    // cleanly). Root cause: `extract_text`/`extract_text_by_pages` process
    // pages sequentially and the panic aborts the WHOLE call on the first
    // bad page — verified directly against this book (1,308 pages): only
    // 12 pages carry the malformed operator, but the whole-document
    // functions lost the text of all 1,308 along with them. Fixed by
    // driving `pdf_extract`'s own per-page primitive ([`extract_pages_resilient`])
    // ourselves, `catch_unwind`-ing each page individually — a bad page now
    // degrades to an empty string for just that page, not the whole book.
    let page_texts = extract_pages_resilient(path);
    let text = page_texts.join("\n");

    // Only pay for this when there is a failure to explain (see
    // `PdfDocument::text_layer_unreadable`).
    let text_layer_unreadable = text.trim().is_empty() && has_text_operators(&doc);

    Ok(PdfDocument {
        text,
        page_texts,
        outline,
        pages: PageMap { page_count, labels },
        text_layer_unreadable,
        // Read from the same already-loaded document — no extra parse (see
        // `meta_probed` for why recording that fact matters).
        meta_title,
        meta_author,
        meta_probed: true,
    })
}

/// `/Info` `Title`/`Author` from an already-loaded document — the part of
/// `read_pdf`'s single `lopdf` pass that used to be paid for a second time
/// per validation by `read_info_metadata`'s own `Document::load` (see
/// [`PdfDocument::meta_title`]). Mirrors `source::acervo`'s path-based
/// version, which remains only for callers that have no parsed document in
/// hand (the rare `ensure_library_file_index` backfill).
fn read_info(doc: &lopdf::Document) -> (Option<String>, Option<String>) {
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

/// Path-based variant of [`read_info`] for callers holding only a file path:
/// one `lopdf` load, then the same trailer walk. A full structural parse, so
/// NOT for validation passes — those get metadata from the (cached)
/// [`PdfDocument`] itself. Its two remaining callers are one-shot backfills:
/// `api::reading`'s `ensure_library_file_index` (S31) and
/// [`read_pdf_cached`]'s one-time probe of entries written before the
/// metadata fields existed (S32, 2026-09-03 — see
/// [`PdfDocument::meta_probed`]).
pub(crate) fn read_info_metadata(path: &Path) -> (Option<String>, Option<String>) {
    match lopdf::Document::load(path) {
        Ok(doc) => read_info(&doc),
        Err(_) => (None, None),
    }
}

/// Sibling of the acervo gate's other content-hash-keyed caches
/// (`TocConfirmStore`'s `<data>/index/toc/`, the embeddings index's
/// `<data>/index/library/`) — same convention, `<data>/index/pdftext/`.
pub fn pdftext_cache_dir(data_dir: impl AsRef<Path>) -> PathBuf {
    data_dir.as_ref().join("index").join("pdftext")
}

/// [`read_pdf`], but checks a content-hash-keyed disk cache first (S27o, bug
/// reported live 2026-08-31: an 11-book library made every acervo-gate check
/// take many minutes, because [`super::acervo::validate_acervo_with_progress`]'s
/// `load_candidates` step re-extracted text from EVERY PDF in the library on
/// EVERY call — this module's own doc comment on `read_pdf` already measured
/// that single cost at 222s for one 1,300-page textbook). Every production
/// caller of `read_pdf` already computes `content_hash` from the same bytes
/// it hands to `read_pdf` (to key the embeddings index / manual-match store /
/// `TocConfirmStore`), so hashing first and checking a cache before parsing
/// costs almost nothing extra on a cache hit, and turns every REPEAT visit to
/// an already-seen PDF into one file read instead of a multi-minute reparse.
///
/// Keyed by [`super::acervo::content_hash`], exactly like every other derived
/// cache this module's siblings already use — a changed file naturally gets
/// a fresh entry (no invalidation logic needed), and the whole directory is a
/// rebuildable derived cache (CLAUDE.md: "files are the source of truth;
/// indexes are rebuildable derived caches") — safe to delete wholesale if it
/// ever needs a reset. A corrupt/unreadable cache entry degrades to a full
/// reparse rather than erroring, same "one bad file must not sink the batch"
/// stance as the rest of `source::acervo`. Returns the hash alongside the
/// parsed document since every caller needs both and would otherwise hash
/// the bytes a second time.
pub fn read_pdf_cached(
    path: impl AsRef<Path>,
    cache_dir: impl AsRef<Path>,
) -> Result<(String, PdfDocument), PdfReadError> {
    let path = path.as_ref();
    let bytes = fs::read(path)
        .map_err(|e| PdfReadError(format!("failed to read {}: {e}", path.display())))?;
    let hash = super::acervo::content_hash(&bytes);
    let cache_dir = cache_dir.as_ref();
    let cache_path = cache_dir.join(format!("{hash}.json"));

    if let Ok(cached_bytes) = fs::read(&cache_path)
        && let Ok(mut cached) = serde_json::from_slice::<PdfDocument>(&cached_bytes)
    {
        // S32 follow-up (bug reported live 2026-09-03): entries written
        // before `meta_title`/`meta_author` existed load with both `None`,
        // and for a book whose first page extracts with per-glyph spacing
        // (Stewart's cover: "C ALCULUS") that metadata was the ONLY clean
        // title/author signal the acervo matching layers had — losing it
        // flipped a working library to "Missing" and the grounding gate to
        // "no resolved file". Recovering it costs one `lopdf` metadata parse
        // per legacy entry, paid ONCE: the entry is rewritten right after
        // (with `meta_probed: true`), so no later validation reparses.
        // A PDF with no `/Info` at all probes once, records `meta_probed`
        // with `None`s, and never probes again.
        if !cached.meta_probed {
            let (title, author) = read_info_metadata(path);
            cached.meta_title = title;
            cached.meta_author = author;
            cached.meta_probed = true;
            if fs::create_dir_all(cache_dir).is_ok()
                && let Ok(json) = serde_json::to_vec(&cached)
            {
                let tmp = cache_path.with_extension("json.tmp");
                if fs::write(&tmp, &json).is_ok() {
                    let _ = fs::rename(&tmp, &cache_path);
                }
            }
        }
        return Ok((hash, cached));
    }

    let pdf = read_pdf(path)?;

    // Best-effort write, atomically (tmp + rename, same idiom as
    // `TocConfirmStore::put`/`build_index_cache`) — a failure here must
    // never fail the read itself; the cache is a pure optimization.
    if fs::create_dir_all(cache_dir).is_ok()
        && let Ok(json) = serde_json::to_vec(&pdf)
    {
        let tmp = cache_path.with_extension("json.tmp");
        if fs::write(&tmp, &json).is_ok() {
            let _ = fs::rename(&tmp, &cache_path);
        }
    }

    Ok((hash, pdf))
}

/// Do the page content streams contain any text-showing operator at all?
///
/// Answers "is there text in this file that we failed to read?" without
/// caring what the text says — enough to tell an image-only scan (nothing to
/// extract, the user really does need a different copy) apart from an
/// extractor failure (the text is right there and we produced nothing).
///
/// Stops at the first page that carries text: a document only has to prove
/// the point once, and a scan with no text layer anywhere is the case where
/// walking every page would be most expensive.
fn has_text_operators(doc: &lopdf::Document) -> bool {
    // Deliberately a LEXICAL scan of the raw bytes, not a `Content::decode`
    // walk. This is a detector for "the extractor failed", so it must not
    // share a failure mode with the extractor: `lopdf`'s content decoder is
    // exactly what returns zero operations on a stream it dislikes (the `%`
    // comment bug the vendored `pdf-extract` patch fixes), which would make
    // this report "no text" for precisely the files it exists to catch.
    // Looking for a bare `BT` token is coarse but cannot be defeated the same
    // way — and coarse is right here, since the question is only whether text
    // is present at all, never what it says.
    let pages = doc.get_pages();
    let mut page_nums: Vec<u32> = pages.keys().copied().collect();
    page_nums.sort_unstable();
    page_nums.into_iter().any(|n| {
        let Some(&page_id) = pages.get(&n) else {
            return false;
        };
        let content = doc.get_page_content(page_id);
        content.windows(2).enumerate().any(|(i, w)| {
            w == b"BT"
                && content
                    .get(i.wrapping_sub(1))
                    .is_none_or(|b| !b.is_ascii_alphanumeric())
                && content
                    .get(i + 2)
                    .is_none_or(|b| !b.is_ascii_alphanumeric())
        })
    })
}

/// Extracts text one physical page at a time via `pdf_extract`'s own
/// lower-level primitives (`output_doc_page`/`PlainTextOutput`) instead of
/// its whole-document `extract_text{,_by_pages}` — see `read_pdf`'s doc
/// comment for why: those functions abort the entire extraction on the
/// first page whose content stream panics the crate, discarding every good
/// page's text along with the bad one. Wrapping `catch_unwind` around each
/// page individually instead means a bad page degrades to `""` on its own,
/// with the rest of the document unaffected.
///
/// Loads its own `pdf_extract::Document` — deliberately **not** reusing the
/// `lopdf::Document` already loaded in [`read_pdf`]: `pdf_extract` vendors
/// and re-exports its own `lopdf` (`pub use lopdf::*`), pinned to a
/// different version (0.42) than this crate's own direct `lopdf` dependency
/// (0.44, used for `read_outline`/`read_page_labels` above) — the two
/// `Document` types aren't interchangeable, so this is a second, separate
/// parse of the same file. A file whose `pdf_extract::Document::load` (or
/// `get_pages`) itself fails degrades to no pages at all, consistent with
/// `read_pdf`'s own "genuinely unreadable file" case being the only hard
/// error this module surfaces.
fn extract_pages_resilient(path: &Path) -> Vec<String> {
    let Ok(doc) = pdf_extract::Document::load(path) else {
        return Vec::new();
    };
    let mut page_nums: Vec<u32> = doc.get_pages().keys().copied().collect();
    page_nums.sort_unstable();

    page_nums
        .into_iter()
        .map(|page_num| {
            let mut s = String::new();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut output = pdf_extract::PlainTextOutput::new(&mut s);
                pdf_extract::output_doc_page(&doc, &mut output, page_num)
            }));
            match result {
                Ok(Ok(())) => s,
                // Either this page's own extraction returned `Err`, or it
                // panicked (caught) — both degrade to an empty page, never
                // propagate, matching the module's best-effort rule.
                _ => String::new(),
            }
        })
        .collect()
}

/// Reads the embedded outline via `lopdf`'s own `get_toc` (a flat,
/// level-tagged list resolved against the page tree already), then
/// reconstructs the hierarchy `get_toc` flattened away. Any failure —
/// including the common case of no `/Outlines` at all — degrades to an
/// empty outline, never an error (module doc's best-effort rule).
fn read_outline(doc: &lopdf::Document) -> Vec<OutlineEntry> {
    match doc.get_toc() {
        Ok(toc) => build_outline_tree(&toc.toc),
        Err(_) => Vec::new(),
    }
}

/// Rebuilds the hierarchy from `lopdf::TocType`'s flat `(level, title,
/// page)` list. `lopdf::Document::get_toc` numbers top-level entries level
/// `1`, incrementing by one per nesting depth in the common case — but it
/// silently *drops* an entry whose destination isn't in the page map while
/// still emitting that entry's children at `level + 1` (see its own
/// `get_toc`, which has no `else` branch when the page lookup misses), so a
/// level jump of more than one is possible in the flat list. This walk
/// handles that benignly: a dropped parent's children are simply promoted to
/// its would-be level (`item.level < level` is the only break condition),
/// never a panic or lost entries.
fn build_outline_tree(toc: &[lopdf::TocType]) -> Vec<OutlineEntry> {
    fn build(items: &[lopdf::TocType], idx: &mut usize, level: usize) -> Vec<OutlineEntry> {
        let mut nodes = Vec::new();
        while *idx < items.len() {
            let item = &items[*idx];
            if item.level < level {
                break;
            }
            *idx += 1;
            let children = build(items, idx, level + 1);
            nodes.push(OutlineEntry {
                title: item.title.clone(),
                page: item.page,
                children,
            });
        }
        nodes
    }
    let mut idx = 0;
    build(toc, &mut idx, 1)
}

/// Walks the catalog's `/PageLabels` number tree (`/Nums` directly, or
/// `/Kids` for a subdivided tree) into a flat, sorted list of label runs.
/// Absent `/PageLabels`, an absent catalog, or any malformed entry along the
/// way degrades to an empty list — [`PageMap::label`] already falls back to
/// plain physical numbers for that case.
fn read_page_labels(doc: &lopdf::Document) -> Vec<PageLabelRun> {
    let mut runs = Vec::new();
    if let Ok(catalog) = doc.catalog()
        && let Ok(root) = doc.get_dict_in_dict(catalog, b"PageLabels")
    {
        collect_number_tree(doc, root, &mut runs, 0);
    }
    runs.sort_by_key(|r| r.start);
    runs
}

/// Recursion depth guard for a malformed/cyclic number tree — real
/// `/PageLabels` trees are shallow (one level per few thousand pages), so
/// this is far beyond anything legitimate.
const MAX_NUMBER_TREE_DEPTH: usize = 16;

fn collect_number_tree(
    doc: &lopdf::Document,
    node: &lopdf::Dictionary,
    out: &mut Vec<PageLabelRun>,
    depth: usize,
) {
    if depth > MAX_NUMBER_TREE_DEPTH {
        return;
    }
    if let Ok(nums) = node.get(b"Nums").and_then(lopdf::Object::as_array) {
        let mut pairs = nums.iter();
        while let (Some(key_obj), Some(val_obj)) = (pairs.next(), pairs.next()) {
            let Ok(start) = key_obj.as_i64() else {
                continue;
            };
            if start < 0 {
                continue;
            }
            let label_dict = match val_obj {
                lopdf::Object::Reference(id) => {
                    doc.get_object(*id).ok().and_then(|o| o.as_dict().ok())
                }
                lopdf::Object::Dictionary(d) => Some(d),
                _ => None,
            };
            if let Some(label_dict) = label_dict {
                out.push(parse_page_label(start as usize, label_dict));
            }
        }
    }
    if let Ok(kids) = node.get(b"Kids").and_then(lopdf::Object::as_array) {
        for kid in kids {
            if let lopdf::Object::Reference(id) = kid
                && let Ok(kid_dict) = doc.get_object(*id).and_then(lopdf::Object::as_dict)
            {
                collect_number_tree(doc, kid_dict, out, depth + 1);
            }
        }
    }
}

fn parse_page_label(start: usize, dict: &lopdf::Dictionary) -> PageLabelRun {
    let style = dict
        .get(b"S")
        .ok()
        .and_then(|o| o.as_name().ok())
        .and_then(|name| match name {
            b"D" => Some(LabelStyle::Decimal),
            b"R" => Some(LabelStyle::UpperRoman),
            b"r" => Some(LabelStyle::LowerRoman),
            b"A" => Some(LabelStyle::UpperAlpha),
            b"a" => Some(LabelStyle::LowerAlpha),
            _ => None,
        });
    let prefix = dict
        .get(b"P")
        .ok()
        .and_then(|o| o.as_str().ok())
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default();
    let start_num = dict
        .get(b"St")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(1);
    PageLabelRun {
        start,
        style,
        prefix,
        start_num,
    }
}

/// Roman-numeral rendering (uppercase; `PageMap::label` lowercases for the
/// `r` style) for the `/S /R` and `/S /r` `/PageLabels` styles (PDF 32000-1
/// §12.4.2). `n <= 0` has no valid roman form and yields an empty string —
/// PDF page numbering never goes below the label's own `/St`, which the spec
/// requires to be a positive integer.
fn to_roman(mut n: i64) -> String {
    if n <= 0 {
        return String::new();
    }
    const VALUES: [(i64, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for &(value, symbol) in &VALUES {
        while n >= value {
            out.push_str(symbol);
            n -= value;
        }
    }
    out
}

/// Letter rendering for the `/S /A` and `/S /a` `/PageLabels` styles (PDF
/// 32000-1 §12.4.2): `1..=26` is `a..=z`, then the letter repeats —
/// `27` is `aa`, `28` is `bb`, ..., `52` is `zz`, `53` is `aaa`. This is
/// **not** bijective base-26 (spreadsheet-column) numbering; the PDF spec's
/// scheme repeats a single letter rather than combining different ones.
fn to_alpha(n: i64, upper: bool) -> String {
    if n <= 0 {
        return String::new();
    }
    let n = n as u64;
    let cycle = (n - 1) / 26 + 1;
    let letter_index = ((n - 1) % 26) as u8;
    let mut letter = (b'a' + letter_index) as char;
    if upper {
        letter = letter.to_ascii_uppercase();
    }
    std::iter::repeat_n(letter, cycle as usize).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::content::{Content, Operation};
    use lopdf::{Bookmark, Document, Object, ObjectId, Stream, dictionary};

    /// Builds a minimal multi-page PDF with real, distinct text per page —
    /// enough for `pdf-extract` to find something, and a page tree `lopdf`
    /// can enumerate. Returns the document plus each page's `ObjectId`, so
    /// tests can attach bookmarks/labels to specific pages without
    /// re-deriving ids from the built document.
    fn build_test_document(page_texts: &[&str]) -> (Document, Vec<ObjectId>) {
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
            let content = Content {
                operations: vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 24.into()]),
                    Operation::new("Td", vec![72.into(), 700.into()]),
                    Operation::new("Tj", vec![Object::string_literal(*text)]),
                    Operation::new("ET", vec![]),
                ],
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

        (doc, page_ids)
    }

    /// Returns the `TempDir` guard alongside the saved file's path — the
    /// caller must keep the guard alive (bind it, don't `let _ = `) for as
    /// long as it needs the file; the directory (and file) are removed when
    /// the guard drops, same convention `events.rs`/`store.rs` use.
    fn save_to_temp(doc: &mut Document, name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join(name);
        doc.save(&path).expect("save fixture pdf");
        (dir, path)
    }

    /// Reported live, 2026-08-29: a real library PDF's content stream had a
    /// `w` (set line width) operator with zero operands, and `pdf-extract`
    /// 0.12.0 indexes `operands[0]` unconditionally for that operator —
    /// PANICS instead of returning `Err`, which used to unwind straight
    /// through `read_pdf`'s `.unwrap_or_default()` and crash whatever
    /// `spawn_blocking` task called it (`source::acervo::validate_acervo`,
    /// live in production via the acervo gate). `read_pdf` must degrade to
    /// empty text/page_texts here, the same as it already does for any
    /// other extraction failure — never propagate the panic.
    #[test]
    fn a_malformed_content_stream_panic_degrades_to_empty_text_not_a_crash() {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content = Content {
            operations: vec![Operation::new("w", vec![])],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let (_dir, path) = save_to_temp(&mut doc, "malformed-w-operator.pdf");

        let read = read_pdf(&path).expect("must not error, let alone panic");
        assert_eq!(read.pages.page_count, 1, "the page tree itself is fine");
        assert!(
            read.text.is_empty(),
            "extraction panicked internally, so text degrades to empty: {:?}",
            read.text
        );
    }

    /// Reported live, same day, right after the fix above landed: the same
    /// book came back "no text layer" even though it's a real, selectable-
    /// text book (confirmed independently with `pdftotext`/`pypdf`). Root
    /// cause found by isolating `pdf_extract`'s own whole-document
    /// functions from `read_pdf`'s caller in a standalone probe: they
    /// process pages sequentially and the panic on ONE malformed page
    /// aborts the ENTIRE call, discarding every other page's text too —
    /// verified against the real 1,308-page book (only 12 pages malformed,
    /// but the whole-document call lost the text of all 1,308). A single
    /// bad page sandwiched between two good ones must not blank the good
    /// pages' text — this is what `a_malformed_content_stream_panic_
    /// degrades_to_empty_text_not_a_crash` above couldn't catch, since its
    /// fixture has only the one (bad) page.
    #[test]
    fn a_malformed_page_among_good_pages_only_blanks_that_one_page() {
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

        let good_content = |text: &str| {
            Content {
                operations: vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 24.into()]),
                    Operation::new("Td", vec![72.into(), 700.into()]),
                    Operation::new("Tj", vec![Object::string_literal(text)]),
                    Operation::new("ET", vec![]),
                ],
            }
            .encode()
            .unwrap()
        };
        let bad_content = Content {
            operations: vec![Operation::new("w", vec![])],
        }
        .encode()
        .unwrap();

        let mut page_ids = Vec::new();
        for content in [
            good_content("Page One"),
            bad_content,
            good_content("Page Three"),
        ] {
            let content_id = doc.add_object(Stream::new(dictionary! {}, content));
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            });
            page_ids.push(page_id);
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids.iter().map(|&id| id.into()).collect::<Vec<Object>>(),
                "Count" => 3,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let (_dir, path) = save_to_temp(&mut doc, "one-bad-page-among-good.pdf");

        let read = read_pdf(&path).expect("must not error, let alone panic");
        assert_eq!(read.pages.page_count, 3);
        assert_eq!(read.page_texts.len(), 3, "all three pages present");
        assert!(
            read.page_texts[0].contains("Page One"),
            "page 1 (good) must survive page 2's panic: {:?}",
            read.page_texts[0]
        );
        assert!(
            read.page_texts[1].is_empty(),
            "page 2 (malformed) degrades to empty: {:?}",
            read.page_texts[1]
        );
        assert!(
            read.page_texts[2].contains("Page Three"),
            "page 3 (good) must survive page 2's panic: {:?}",
            read.page_texts[2]
        );
        assert!(
            read.text.contains("Page One") && read.text.contains("Page Three"),
            "the joined `text` must not be blanked by one bad page: {:?}",
            read.text
        );
    }

    #[test]
    fn extracts_text_from_a_real_pdf() {
        let (mut doc, _pages) = build_test_document(&["Hello World"]);
        let (_dir, path) = save_to_temp(&mut doc, "text-only.pdf");

        let read = read_pdf(&path).expect("read a valid pdf");
        assert!(
            read.text.contains("Hello World"),
            "expected extracted text to contain the page's content, got: {:?}",
            read.text
        );
        assert_eq!(read.pages.page_count, 1);
        assert!(read.outline.is_empty(), "no bookmarks were added");
    }

    #[test]
    fn page_texts_are_split_per_physical_page() {
        let (mut doc, _pages) = build_test_document(&["First page body", "Second page body"]);
        let (_dir, path) = save_to_temp(&mut doc, "two-pages.pdf");

        let read = read_pdf(&path).expect("read a valid pdf");
        assert_eq!(read.page_texts.len(), 2, "one entry per physical page");
        assert!(read.page_texts[0].contains("First page body"));
        assert!(read.page_texts[1].contains("Second page body"));
        assert!(
            !read.page_texts[0].contains("Second page body"),
            "page 1's text must not leak page 2's content"
        );
    }

    #[test]
    fn outline_comes_back_empty_when_the_pdf_has_no_bookmarks() {
        let (mut doc, _pages) = build_test_document(&["One", "Two", "Three"]);
        let (_dir, path) = save_to_temp(&mut doc, "no-outline.pdf");

        let read = read_pdf(&path).expect("read a valid pdf");
        assert_eq!(read.outline, Vec::new());
        assert_eq!(read.pages.page_count, 3);
    }

    #[test]
    fn outline_hierarchy_mirrors_the_pdfs_real_outlines_tree() {
        let (mut doc, pages) = build_test_document(&[
            "Cover",
            "Chapter One body",
            "Section 1.1 body",
            "Section 1.2 body",
            "Chapter Two body",
        ]);

        // Chapter 1 (page 2) has two children, Section 1.1 (page 3) and
        // Section 1.2 (page 4); Chapter 2 (page 5) is a sibling top-level
        // entry with no children — a real chapter/section shape (§6.3).
        let ch1 = doc.add_bookmark(
            Bookmark::new("Chapter 1".to_string(), [0.0, 0.0, 0.0], 0, pages[1]),
            None,
        );
        doc.add_bookmark(
            Bookmark::new("1.1 Intro".to_string(), [0.0, 0.0, 0.0], 0, pages[2]),
            Some(ch1),
        );
        doc.add_bookmark(
            Bookmark::new("1.2 Details".to_string(), [0.0, 0.0, 0.0], 0, pages[3]),
            Some(ch1),
        );
        doc.add_bookmark(
            Bookmark::new("Chapter 2".to_string(), [0.0, 0.0, 0.0], 0, pages[4]),
            None,
        );
        let outlines_id = doc.build_outline().expect("bookmarks were added");
        doc.catalog_mut()
            .expect("catalog exists")
            .set("Outlines", outlines_id);

        let (_dir, path) = save_to_temp(&mut doc, "with-outline.pdf");
        let read = read_pdf(&path).expect("read a valid pdf");

        assert_eq!(read.outline.len(), 2, "two top-level chapters");
        assert_eq!(read.outline[0].title, "Chapter 1");
        assert_eq!(read.outline[0].page, 2);
        assert_eq!(
            read.outline[0].children.len(),
            2,
            "two sections under chapter 1"
        );
        assert_eq!(read.outline[0].children[0].title, "1.1 Intro");
        assert_eq!(read.outline[0].children[0].page, 3);
        assert!(read.outline[0].children[0].children.is_empty());
        assert_eq!(read.outline[0].children[1].title, "1.2 Details");
        assert_eq!(read.outline[0].children[1].page, 4);

        assert_eq!(read.outline[1].title, "Chapter 2");
        assert_eq!(read.outline[1].page, 5);
        assert!(read.outline[1].children.is_empty());
    }

    #[test]
    fn page_labels_fall_back_to_physical_numbers_when_absent() {
        let (mut doc, _pages) = build_test_document(&["One", "Two", "Three"]);
        let (_dir, path) = save_to_temp(&mut doc, "no-page-labels.pdf");

        let read = read_pdf(&path).expect("read a valid pdf");
        assert_eq!(read.pages.label(1), "1");
        assert_eq!(read.pages.label(2), "2");
        assert_eq!(read.pages.label(3), "3");
    }

    #[test]
    fn page_labels_apply_roman_front_matter_and_arabic_body() {
        let (mut doc, _pages) =
            build_test_document(&["Cover", "Preface", "Ch1 p1", "Ch1 p2", "Ch2 p1"]);

        // Physical pages 1-2 (0-based 0-1): lowercase roman, default start 1.
        // Physical pages 3-5 (0-based 2-4): decimal, restarting at 1.
        let nums = vec![
            0.into(),
            Object::Dictionary(dictionary! { "S" => "r" }),
            2.into(),
            Object::Dictionary(dictionary! { "S" => "D", "St" => 1 }),
        ];
        let page_labels_id = doc.add_object(dictionary! { "Nums" => nums });
        doc.catalog_mut()
            .expect("catalog exists")
            .set("PageLabels", page_labels_id);

        let (_dir, path) = save_to_temp(&mut doc, "with-page-labels.pdf");
        let read = read_pdf(&path).expect("read a valid pdf");

        assert_eq!(read.pages.label(1), "i");
        assert_eq!(read.pages.label(2), "ii");
        assert_eq!(read.pages.label(3), "1");
        assert_eq!(read.pages.label(4), "2");
        assert_eq!(read.pages.label(5), "3");
    }

    #[test]
    fn page_labels_support_a_prefix_and_uppercase_alpha_style() {
        let (mut doc, _pages) = build_test_document(&["Appendix A", "Appendix A p2", "Appendix B"]);

        let nums = vec![
            0.into(),
            Object::Dictionary(
                dictionary! { "S" => "A", "P" => Object::string_literal("Appendix ") },
            ),
        ];
        let page_labels_id = doc.add_object(dictionary! { "Nums" => nums });
        doc.catalog_mut()
            .expect("catalog exists")
            .set("PageLabels", page_labels_id);

        let (_dir, path) = save_to_temp(&mut doc, "with-alpha-prefix-labels.pdf");
        let read = read_pdf(&path).expect("read a valid pdf");

        assert_eq!(read.pages.label(1), "Appendix A");
        assert_eq!(read.pages.label(2), "Appendix B");
        assert_eq!(read.pages.label(3), "Appendix C");
    }

    #[test]
    fn roman_and_alpha_label_rendering() {
        assert_eq!(to_roman(1), "I");
        assert_eq!(to_roman(4), "IV");
        assert_eq!(to_roman(9), "IX");
        assert_eq!(to_roman(1994), "MCMXCIV");
        assert_eq!(to_roman(0), "");

        assert_eq!(to_alpha(1, true), "A");
        assert_eq!(to_alpha(26, true), "Z");
        assert_eq!(to_alpha(27, true), "AA");
        assert_eq!(to_alpha(52, true), "ZZ");
        assert_eq!(to_alpha(53, true), "AAA");
        assert_eq!(to_alpha(1, false), "a");
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_silent_empty_result() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("not-a-pdf.pdf");
        std::fs::write(&path, b"this is not a pdf at all").expect("write junk file");

        assert!(read_pdf(&path).is_err());
    }

    // --- read_pdf_cached (S27o, bug reported live 2026-08-31) --------------

    #[test]
    fn read_pdf_cached_returns_the_same_document_read_pdf_would() {
        let (mut doc, _) = build_test_document(&["one", "two", "three"]);
        let (_dir, path) = save_to_temp(&mut doc, "cache-fixture.pdf");
        let cache_dir = tempfile::tempdir().expect("create cache dir");

        let direct = read_pdf(&path).expect("direct read");
        let (_, cached) = read_pdf_cached(&path, cache_dir.path()).expect("cached read");
        assert_eq!(direct, cached);
    }

    #[test]
    fn read_pdf_cached_writes_an_entry_keyed_by_content_hash_and_reuses_it() {
        let (mut doc, _) = build_test_document(&["hello", "world"]);
        let (_dir, path) = save_to_temp(&mut doc, "cache-fixture.pdf");
        let cache_dir = tempfile::tempdir().expect("create cache dir");

        let (hash, _first) = read_pdf_cached(&path, cache_dir.path()).expect("first read, a miss");
        let cache_path = cache_dir.path().join(format!("{hash}.json"));
        assert!(cache_path.is_file(), "a miss must write a cache entry");

        // Overwrite the cache entry with a document that could not possibly
        // have come from re-parsing `path` (the fixture never contains this
        // string). The file on disk is untouched — if the second call
        // genuinely serves the cache instead of re-parsing, it must return
        // THIS content, not the fixture's real text.
        let planted = PdfDocument {
            text: "PLANTED CACHE CONTENT, NOT THE REAL FIXTURE".to_string(),
            page_texts: vec!["PLANTED CACHE CONTENT, NOT THE REAL FIXTURE".to_string()],
            outline: Vec::new(),
            pages: PageMap {
                page_count: 999,
                labels: Vec::new(),
            },
            text_layer_unreadable: false,
            meta_title: Some("Planted Title".to_string()),
            meta_author: None,
            meta_probed: true,
        };
        std::fs::write(&cache_path, serde_json::to_vec(&planted).unwrap())
            .expect("plant a fake cache entry");

        let (hash2, second) =
            read_pdf_cached(&path, cache_dir.path()).expect("second read must hit the cache");
        assert_eq!(
            hash, hash2,
            "the file didn't change, so the hash must not either"
        );
        assert_eq!(
            second, planted,
            "a cache hit must return the cached document verbatim, not re-parse the real file"
        );
    }

    #[test]
    fn read_pdf_cached_never_serves_stale_content_for_a_changed_file() {
        // The exact correctness property a content-hash key buys: same path,
        // different bytes, on the SAME cache directory — must never return
        // the first version's text for the second version. If the cache
        // were keyed by path/filename instead of content hash, this is
        // precisely the bug it would reintroduce.
        let cache_dir = tempfile::tempdir().expect("create cache dir");
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("same-filename.pdf");

        let (mut doc_a, _) = build_test_document(&["version A"]);
        doc_a.save(&path).expect("save version A");
        let (hash_a, pdf_a) = read_pdf_cached(&path, cache_dir.path()).expect("read version A");
        assert!(pdf_a.text.contains("version A"));

        let (mut doc_b, _) = build_test_document(&["version B, totally different"]);
        doc_b.save(&path).expect("overwrite with version B");
        let (hash_b, pdf_b) = read_pdf_cached(&path, cache_dir.path()).expect("read version B");

        assert_ne!(hash_a, hash_b, "different bytes must hash differently");
        assert!(
            pdf_b.text.contains("version B"),
            "must reflect the NEW content, not a stale cache entry keyed by the old hash: {:?}",
            pdf_b.text
        );
        assert!(!pdf_b.text.contains("version A"));
    }

    #[test]
    fn read_pdf_cached_degrades_to_a_full_reparse_on_a_corrupt_cache_entry() {
        let (mut doc, _) = build_test_document(&["still readable"]);
        let (_dir, path) = save_to_temp(&mut doc, "cache-fixture.pdf");
        let cache_dir = tempfile::tempdir().expect("create cache dir");

        let (hash, _) = read_pdf_cached(&path, cache_dir.path()).expect("first read");
        let cache_path = cache_dir.path().join(format!("{hash}.json"));
        std::fs::write(&cache_path, b"not valid json at all").expect("corrupt the cache entry");

        let (_, pdf) =
            read_pdf_cached(&path, cache_dir.path()).expect("must reparse, not error out");
        assert!(pdf.text.contains("still readable"));
    }

    /// A cache entry written BEFORE `meta_title`/`meta_author` existed (the
    /// shape on disk across the S31→S32 boundary) must keep loading —
    /// `#[serde(default)]` on the new fields, not a full reparse of every
    /// book, is what a field addition to a persistent cache costs.
    #[test]
    fn read_pdf_cached_still_loads_entries_written_before_the_metadata_fields() {
        let (mut doc, _) = build_test_document(&["pre-metadata entry"]);
        let (_dir, path) = save_to_temp(&mut doc, "old-shape-cache.pdf");
        let cache_dir = tempfile::tempdir().expect("create cache dir");

        let (hash, first) = read_pdf_cached(&path, cache_dir.path()).expect("first read");
        let cache_path = cache_dir.path().join(format!("{hash}.json"));
        // Rewrite the entry as the old shape: valid PdfDocument JSON with the
        // two new fields stripped.
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&cache_path).expect("read cache entry"))
                .expect("entry is valid json");
        let removed = value
            .as_object_mut()
            .expect("entry is an object")
            .remove("meta_title")
            .is_some()
            && value
                .as_object_mut()
                .expect("entry is an object")
                .remove("meta_author")
                .is_some();
        assert!(removed, "fixture must actually strip the new fields");
        std::fs::write(&cache_path, serde_json::to_vec(&value).expect("serialize")).unwrap();

        let (_, second) = read_pdf_cached(&path, cache_dir.path())
            .expect("an old-shape entry must load, not force a full reparse");
        assert_eq!(second, first);
        assert_eq!(second.meta_title, None, "absent field degrades to default");
    }

    /// S32 follow-up (reported live 2026-09-03): an entry written BEFORE the
    /// metadata fields existed must be probed exactly once — `/Info`
    /// recovered into the meta fields, `meta_probed` set — and rewritten, so
    /// no later validation reparses. Stewart's real cover extracts as
    /// "C ALCULUS" (per-glyph spacing), so `/Info` was the only clean
    /// title/author signal the acervo matching layers had; losing it on the
    /// legacy entries flipped a working library to "Missing" and the
    /// grounding gate to "no resolved file".
    #[test]
    fn read_pdf_cached_probes_and_backfills_the_info_metadata_of_a_legacy_entry() {
        let (mut doc, _) = build_test_document(&["legacy entry"]);
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("Stewart - Calculus - Early Transcedentals 6e"),
            "Author" => Object::string_literal("James Stewart"),
        });
        doc.trailer.set("Info", info_id);
        let (_dir, path) = save_to_temp(&mut doc, "legacy-meta.pdf");
        let cache_dir = tempfile::tempdir().expect("create cache dir");

        // Write a modern entry, then downgrade it to the pre-S32 shape: no
        // meta fields at all (the exact bytes an S31-era binary left on
        // disk).
        let (hash, _) = read_pdf_cached(&path, cache_dir.path()).expect("first read");
        let cache_path = cache_dir.path().join(format!("{hash}.json"));
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&cache_path).expect("read cache entry"))
                .expect("entry is valid json");
        let obj = value.as_object_mut().expect("entry is an object");
        let stripped = obj.remove("meta_title").is_some()
            && obj.remove("meta_author").is_some()
            && obj.remove("meta_probed").is_some();
        assert!(stripped, "fixture must actually strip the metadata fields");
        fs::write(&cache_path, serde_json::to_vec(&value).expect("serialize")).unwrap();

        let (_, backfilled) = read_pdf_cached(&path, cache_dir.path()).expect("second read");
        assert_eq!(
            backfilled.meta_title.as_deref(),
            Some("Stewart - Calculus - Early Transcedentals 6e")
        );
        assert_eq!(backfilled.meta_author.as_deref(), Some("James Stewart"));
        assert!(
            backfilled.meta_probed,
            "the probe must mark the entry probed"
        );

        // The backfill is PERSISTED: the on-disk entry now carries the
        // recovered metadata and the probed marker, so every later read is a
        // plain hit — the one-time probe stays one-time.
        let on_disk: PdfDocument =
            serde_json::from_slice(&fs::read(&cache_path).expect("re-read cache entry"))
                .expect("rewritten entry is valid json");
        assert!(on_disk.meta_probed);
        assert_eq!(
            on_disk.meta_title.as_deref(),
            Some("Stewart - Calculus - Early Transcedentals 6e")
        );
    }
}
