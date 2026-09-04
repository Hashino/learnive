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
//!
//! **Closed by S27m (PLAN.md, 2026-08-29):** "how library PDFs' chunks
//! eventually join real grounding" (this comment used to defer that
//! question) is resolved as its own small path, not folded into the
//! corpus-shaped `retrieval::VectorIndex`/`Retriever` — pushing local
//! library PDFs through `Corpus` would mean inventing `Section`s/locators
//! for them, re-entrenching exactly the HTML-ingestion shape S28 is slated
//! to delete, and PDF text is index-only now (never a display source, per
//! the pivot). [`search_index_cache`] reads one PDF's own cache file back;
//! scoping to one source needs no filter because the cache file already
//! *is* one source. [`resolve_matched_filename`] is the S27f matching
//! screen's own resolution rule, promoted here so the S27m grounding gate
//! (`api::reading::ground_node`) can never disagree with what that screen
//! showed the user (this module used to keep a private copy in
//! `api::acervo` alone; that copy now just calls this one).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::SourceKind;
use super::local::{LibraryEntry, LocalPdfSource};
use super::manual_match::ManualMatchStore;
use super::matching::{normalize, primary_title, surname_of};
use super::pdf::{OutlineEntry, PdfDocument, read_pdf};
use super::toc_confirm::TocConfirmStore;
use crate::retrieval::{Embedder, chunk_text, cosine};

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
    Extractable {
        chars: usize,
    },
    /// The PDF genuinely has nothing to extract — an image-only scan with no
    /// OCR. The user does need a different copy of this book.
    NoText,
    /// The PDF **does** carry a text layer and our extractor produced nothing
    /// from it (`PdfDocument::text_layer_unreadable`). Still blocking — an
    /// unindexed source grounds nothing (SPEC §11.1) — but it must never be
    /// reported to the user as `NoText`: telling someone to re-acquire a book
    /// that is already correct is the worst instruction the acervo gate can
    /// give, and under manual acquisition they pay for every download by hand.
    /// Measured 2026-08-30 against K&R; see `PdfDocument::text_layer_unreadable`.
    ExtractorFailed,
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
    /// `source::toc`'s deduction cascade (S27k) read the printed contents
    /// page and placed `resolved` entries on real physical pages;
    /// `unresolved` is however many it could not place — the only thing
    /// left for the S27f confirmation screen to ask about (never a blank
    /// per-chapter form once this variant applies).
    Deduced {
        resolved: usize,
        unresolved: usize,
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
    /// it. True for anything short of a real embedded outline or a fully
    /// resolved S27k deduction (`Deduced` with nothing left `unresolved`).
    pub fn needs_user_confirmation(&self) -> bool {
        match self {
            TocCheck::Embedded { .. } => false,
            TocCheck::Deduced { unresolved, .. } => *unresolved > 0,
            _ => true,
        }
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
        if matches!(
            self.text_layer,
            TextLayerCheck::NoText | TextLayerCheck::ExtractorFailed
        ) {
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
    toc_confirm_dir: impl AsRef<Path>,
    file_index: Option<&LibraryFileIndex>,
) -> std::io::Result<AcervoReport> {
    validate_acervo_with_progress(
        library,
        expected,
        index_cache_dir,
        toc_confirm_dir,
        file_index,
        |_| {},
    )
}

/// Which stage of the validation an item is waiting on — surfaced so a
/// caller (S27f's report screen, live-reported 2026-08-29: "a tela de
/// checando acervo deveria reportar progresso") can show the user something
/// other than a blank wait while every PDF in a real library gets parsed.
/// Presence isn't its own check phase here — finding (or failing to find) a
/// candidate is the precondition for every other check, reported once as
/// part of starting an item, not as a phase transition of its own.
/// `Scanning` is likewise not one of the six checks: it covers the shared
/// `load_candidates` pass (read + hash + parse of every PDF in the library,
/// once, before any per-item check runs), which on a real library is the
/// longest single stretch of the whole validation — reported against every
/// item at once, since they all wait on it (bug reported live 2026-09-03:
/// that stretch used to emit nothing, so the report screen sat every row on
/// its initial "Queued…" label for its whole duration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcervoPhase {
    Scanning,
    Presence,
    Identity,
    TextLayer,
    Toc,
    PageMap,
    Index,
}

impl AcervoPhase {
    /// Stable wire tag — mirrors the `&'static str` fields `api::acervo`
    /// already serializes the finished check results as, so the frontend
    /// reuses one vocabulary for "in progress" and "done".
    pub fn as_str(self) -> &'static str {
        match self {
            AcervoPhase::Scanning => "scanning",
            AcervoPhase::Presence => "presence",
            AcervoPhase::Identity => "identity",
            AcervoPhase::TextLayer => "text_layer",
            AcervoPhase::Toc => "toc",
            AcervoPhase::PageMap => "page_map",
            AcervoPhase::Index => "index",
        }
    }
}

/// One progress tick: which item, which phase, emitted right before that
/// phase's check actually runs.
#[derive(Debug, Clone, PartialEq)]
pub struct AcervoProgress {
    pub title: String,
    pub phase: AcervoPhase,
}

/// Same as [`validate_acervo`], but calls `on_progress` right before each
/// check runs for each item — a plain synchronous callback (not a channel)
/// so this module stays free of any async/tokio dependency (mirrors the
/// rest of `source`); a caller that needs to stream progress across an
/// `.await` point (S27f's SSE report endpoint) sends from inside the
/// callback into whatever channel it owns. Ordering is deterministic: one
/// `Scanning` tick per item first (in `expected` order — the shared library
/// scan they all wait on), then per item in `expected` order, phases in the
/// same presence→identity→text_layer→toc→page_map→index order the six
/// checks are documented in above.
pub fn validate_acervo_with_progress(
    library: &LocalPdfSource,
    expected: &[ExpectedItem],
    index_cache_dir: impl AsRef<Path>,
    toc_confirm_dir: impl AsRef<Path>,
    file_index: Option<&LibraryFileIndex>,
    mut on_progress: impl FnMut(AcervoProgress),
) -> std::io::Result<AcervoReport> {
    // One `Scanning` tick per item up front, before the shared library scan
    // below: `load_candidates` reads, hashes, and parses EVERY PDF once,
    // before any per-item check runs, and on a real library that is the
    // longest single stretch of the whole pass — silent until now, so the
    // report screen spent it with every row on "Queued…" (bug reported live
    // 2026-09-03). Every item genuinely is waiting on this scan, so the
    // label is true for all rows at once; the per-item `Presence` tick
    // follows as soon as the scan finishes.
    for item in expected {
        on_progress(AcervoProgress {
            title: item.title.clone(),
            phase: AcervoPhase::Scanning,
        });
    }
    let candidates = load_candidates(library)?;
    let index_cache_dir = index_cache_dir.as_ref();
    // S27n: every candidate's hash is already computed above (`load_candidates`)
    // — record hash → filename here, the one place that's true for every
    // validation pass, so `GET /api/library/{hash}/pdf` never has to rescan
    // and rehash the whole library on a citation click. `file_index` is
    // `None` on the read-only gate-report GET (`api::acervo`'s SSE progress
    // route, §3.1 forbids a GET performing writes) and `Some` only on the
    // mutating POST path (`ensure_document_grounded`) — that write must not
    // happen from a GET handler no matter how convenient deriving it from
    // `index_cache_dir` would be.
    if let Some(file_index) = file_index {
        for cand in &candidates {
            file_index.set(
                &cand.hash,
                &cand.entry.filename,
                cand.meta_title.as_deref(),
                cand.meta_author.as_deref(),
            )?;
        }
    }
    let toc_confirm = TocConfirmStore::open_at(toc_confirm_dir)?;
    let items = expected
        .iter()
        .map(|item| {
            build_item_report(
                item,
                &candidates,
                index_cache_dir,
                &toc_confirm,
                &mut on_progress,
            )
        })
        .collect();
    Ok(AcervoReport { items })
}

/// `<data>/index/pdftext/` — sibling of `library.root()` (`<data>/library/`)
/// under the same `<data>/` root, derived from the library handle itself
/// rather than threaded as a parameter. This lets every one of this
/// function's callers (`candidate_matches`/`unmatched_library_files`/
/// `match_report`/`validate_acervo_with_progress`, and their own callers
/// across `api::reading`/`api::acervo` — 8+ production call sites) get the
/// cache with zero signature changes, since every one of them already has a
/// `&LocalPdfSource` in hand. See `pdf::read_pdf_cached`'s doc for why this
/// cache exists (S27o, bug reported live 2026-08-31).
fn pdftext_cache_dir_for(library: &LocalPdfSource) -> PathBuf {
    library
        .root()
        .parent()
        .map(|p| p.join("index").join("pdftext"))
        .unwrap_or_else(|| library.root().join("index").join("pdftext"))
}

fn load_candidates(library: &LocalPdfSource) -> std::io::Result<Vec<LibraryCandidate>> {
    let cache_dir = pdftext_cache_dir_for(library);
    let entries = library.scan()?;
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let path = library.root().join(&entry.filename);
        // S27o (bug reported live 2026-08-31): every library PDF used to be
        // re-extracted from scratch on EVERY acervo check — measured at
        // minutes for an 11-book library. `read_pdf_cached` skips the
        // extraction entirely on a repeat visit to an already-seen file.
        let Ok((hash, pdf)) = super::pdf::read_pdf_cached(&path, &cache_dir) else {
            // Genuinely unreadable file: not a candidate for anything. Left
            // out rather than erroring the whole validation pass, matching
            // the module's "one bad file must not sink the batch" stance.
            continue;
        };
        // From the (possibly cached) document itself, NOT from a second
        // `lopdf::Document::load` — S32, bug reported live 2026-09-03: the
        // metadata reparse kept a warm, fully-indexed library paying a full
        // structural parse of every book on every validation pass.
        let (meta_title, meta_author) = (pdf.meta_title.clone(), pdf.meta_author.clone());
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
    toc_confirm: &TocConfirmStore,
    on_progress: &mut impl FnMut(AcervoProgress),
) -> ItemReport {
    let tick = |phase: AcervoPhase, on_progress: &mut dyn FnMut(AcervoProgress)| {
        on_progress(AcervoProgress {
            title: item.title.clone(),
            phase,
        });
    };

    tick(AcervoPhase::Presence, on_progress);
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

    tick(AcervoPhase::Identity, on_progress);
    let identity = check_identity(item, cand);
    tick(AcervoPhase::TextLayer, on_progress);
    let text_layer = check_text_layer(&cand.pdf);
    tick(AcervoPhase::Toc, on_progress);
    let toc = check_toc(&cand.pdf, &cand.hash, toc_confirm);
    tick(AcervoPhase::PageMap, on_progress);
    let page_map = check_page_map(&cand.pdf);
    tick(AcervoPhase::Index, on_progress);
    let index = check_index_cache(&cand.hash, index_cache_dir);

    ItemReport {
        expected: item.clone(),
        presence: PresenceCheck::Found {
            filename: cand.entry.filename.clone(),
        },
        identity,
        text_layer,
        toc,
        page_map,
        index,
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
        if pdf.text_layer_unreadable {
            TextLayerCheck::ExtractorFailed
        } else {
            TextLayerCheck::NoText
        }
    } else {
        TextLayerCheck::Extractable {
            chars: trimmed.chars().count(),
        }
    }
}

/// `content_hash` is the candidate PDF's own hash — the same key
/// `TocConfirmStore` is addressed by everywhere else, so a deduction pass
/// run earlier (`api::acervo`'s async pre-pass, ahead of this synchronous
/// six-check engine — this module stays free of `Ai`/tokio, S27k's own
/// discipline note) shows up here without this function ever calling the
/// model itself.
fn check_toc(pdf: &PdfDocument, content_hash: &str, toc_confirm: &TocConfirmStore) -> TocCheck {
    if !pdf.outline.is_empty() {
        return TocCheck::Embedded {
            entries: count_outline_entries(&pdf.outline),
        };
    }
    if let Some(confirmed) = toc_confirm.get(content_hash)
        && (!confirmed.entries.is_empty() || !confirmed.unresolved.is_empty())
    {
        return TocCheck::Deduced {
            resolved: confirmed.entries.len(),
            unresolved: confirmed.unresolved.len(),
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

/// Resolves the single filename currently understood to represent an
/// expected item: a recorded manual pairing wins outright; otherwise a
/// unique automatic candidate; anything else (no candidate, or more than
/// one) is unresolved — the caller can't proceed without the matching
/// screen (or, as of S27m, the grounding gate refusing to generate)
/// settling it first. Promoted here from `api::acervo` (S27m, 2026-08-29)
/// so the grounding gate (`api::reading::ground_node`) shares the exact
/// same resolution rule as the S27f matching screen, instead of a second
/// copy silently drifting out of sync with it.
pub fn resolve_matched_filename(
    library: &LocalPdfSource,
    manual: &ManualMatchStore,
    item: &ExpectedItem,
) -> std::io::Result<Option<String>> {
    if let Some(m) = manual.get(item) {
        return Ok(Some(m.filename));
    }
    let candidates = candidate_matches(library, item)?;
    match candidates.len() {
        1 => Ok(Some(
            candidates.into_iter().next().expect("len == 1").filename,
        )),
        _ => Ok(None),
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
/// One cached, embedded chunk of a library PDF — page-tagged (S27m,
/// 2026-08-29 revision) so a hit can produce a real `p:N` locator for
/// `CITE_CONTRACT`/`#page=N` deep-links, not an empty one. The original
/// shape chunked `pdf.text` (the whole book concatenated) and lost the page
/// boundary entirely; chunking `pdf.page_texts` instead costs nothing extra
/// (same total text, same embedder calls) and closes that gap before
/// anything downstream comes to depend on the page-less shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedChunk {
    /// 1-based physical page number (`pdf.page_texts`' index + 1) — matches
    /// what `PageMapCheck`/`#page=N` already use elsewhere in this module.
    pub page: usize,
    pub text: String,
    pub vector: Vec<f32>,
}

/// The cache format here (page-tagged chunk + vector, [`CachedChunk`]) is
/// deliberately minimal, not the corpus-shaped `retrieval::VectorIndex` —
/// see the module doc's "Closed by S27m" note for why library PDFs get their
/// own small path instead of joining `Corpus`.
pub fn build_index_cache(
    pdf: &PdfDocument,
    content_hash: &str,
    index_cache_dir: &Path,
    embedder: &Embedder,
) -> std::io::Result<PathBuf> {
    fs::create_dir_all(index_cache_dir)?;
    let path = index_cache_dir.join(format!("{content_hash}.json"));

    let mut pages = Vec::new();
    let mut chunks = Vec::new();
    for (i, page_text) in pdf.page_texts.iter().enumerate() {
        for chunk in chunk_text(page_text) {
            pages.push(i + 1);
            chunks.push(chunk);
        }
    }
    let vectors = if chunks.is_empty() {
        Vec::new()
    } else {
        embedder.embed_batch(&chunks)
    };
    let cached: Vec<CachedChunk> = pages
        .into_iter()
        .zip(chunks)
        .zip(vectors)
        .map(|((page, text), vector)| CachedChunk { page, text, vector })
        .collect();

    let json = serde_json::to_vec(&cached)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Reads one PDF's cache back and ranks its chunks by cosine similarity to
/// `query`, page number carried along (S27m piece 2, PLAN.md). Scoped to
/// exactly this source by construction — the cache file already is one
/// PDF's content, so there is no `source_id` to filter by, unlike the
/// corpus-shaped `Retriever` this deliberately doesn't reuse (module doc).
/// Errors only on I/O/corruption; an empty/missing cache is the caller's
/// job to have ruled out via [`IndexCheck`] first.
///
/// `page_range` narrows the candidate pool to one chapter (S27g item 1,
/// PLAN.md, 2026-08-30): `Some((start, end))`, both 1-based and inclusive,
/// `end: None` meaning "to the end of the book" (the last chapter has no
/// next sibling to bound it). Filtering happens **before** ranking/`truncate`
/// — narrowing after truncation would often hand back zero chunks instead of
/// the book's own best passage. An empty result after filtering (a bad
/// `resolved_page`, a range that lands between two chunks) falls back to the
/// **whole book** rather than returning nothing: SPEC's "no source coverage
/// ⇒ no generation" must never be triggered by a narrowing bug, and this
/// costs no extra tokens either way (§15's free-tier corollary).
pub fn search_index_cache(
    index_cache_dir: &Path,
    content_hash: &str,
    embedder: &Embedder,
    query: &str,
    k: usize,
    page_range: Option<(usize, Option<usize>)>,
) -> std::io::Result<Vec<(usize, String, f32)>> {
    let path = index_cache_dir.join(format!("{content_hash}.json"));
    let json = fs::read(&path)?;
    let cached: Vec<CachedChunk> = serde_json::from_slice(&json)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let pool: Vec<CachedChunk> = match page_range {
        Some((start, end)) => {
            let scoped: Vec<CachedChunk> = cached
                .iter()
                .filter(|c| c.page >= start && end.is_none_or(|e| c.page <= e))
                .cloned()
                .collect();
            if scoped.is_empty() { cached } else { scoped }
        }
        None => cached,
    };
    let query_vector = embedder.embed(query);
    let mut scored: Vec<(usize, String, f32)> = pool
        .into_iter()
        .map(|c| {
            let score = cosine(&query_vector, &c.vector);
            (c.page, c.text, score)
        })
        .collect();
    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    Ok(scored)
}

/// Contiguous in-order section text for one chapter's page range — the
/// grounding-coverage fix (2026-09-04, live: a chapter-node's explain came
/// back banner-flagged as ungrounded while being faithful to its chapter).
/// Root cause: `ground_node` handed generation AND the grounding-verification
/// check only `search_index_cache`'s top-4 similarity chunks (~2.3k chars) of
/// a chapter that is tens of thousands of characters — the k=4 budget dates
/// from the atomic-node granularity and never followed the pivot to
/// chapter-sized nodes (S33 follow-up). A model asked to explain a 17-page
/// chapter from 4 passages fills the rest from training-data memory of the
/// (famous) book — the checker then correctly flags every true claim that
/// sat outside the passages: a false positive on faithful prose, a real
/// positive only on drift.
///
/// So the unit of grounding is the chapter (§11's structural per-node source
/// selection), delivered IN PAGE ORDER — contiguous section text the model
/// can actually read, not scattered best-passages — capped at
/// [`SECTION_TEXT_CHAR_BUDGET`] (~4k tokens) so a grounded move's prompt and
/// its verification check (which must see the exact same text, §11.1) stay
/// inside the free-tier TPM ceilings measured in
/// `docs/S27g-chapter-matching-measurements.md`. Anchoring is per node, not
/// per chapter: `anchor_page` (the page of the node title's own best-matching
/// chunk, `Some` when the caller had a hit) starts the window where THIS
/// node's material lives; without one the window starts at the range's first
/// page. Whole-book fallback matches [`search_index_cache`]'s: a narrowing
/// bug must never read as "no source coverage".
pub fn pages_text_from_cache(
    index_cache_dir: &Path,
    content_hash: &str,
    page_range: Option<(usize, Option<usize>)>,
    anchor_page: Option<usize>,
    max_chars: usize,
) -> std::io::Result<Vec<(usize, String)>> {
    let path = index_cache_dir.join(format!("{content_hash}.json"));
    let json = fs::read(&path)?;
    let cached: Vec<CachedChunk> = serde_json::from_slice(&json)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let pool: Vec<CachedChunk> = match page_range {
        Some((start, end)) => {
            let scoped: Vec<CachedChunk> = cached
                .iter()
                .filter(|c| c.page >= start && end.is_none_or(|e| c.page <= e))
                .cloned()
                .collect();
            if scoped.is_empty() { cached } else { scoped }
        }
        None => cached,
    };
    let Some(first) = pool.first() else {
        return Ok(Vec::new());
    };
    let start = anchor_page
        .filter(|a| pool.iter().any(|c| c.page >= *a))
        .unwrap_or(first.page);
    // Chunks arrive in file order = page order = in-page order (the builder
    // appends per page, `chunk_text` in order) — so a stable group-by page
    // needs no sorting.
    let mut pages: Vec<(usize, String)> = Vec::new();
    let mut used = 0usize;
    for chunk in pool.iter().filter(|c| c.page >= start) {
        if used + chunk.text.len() > max_chars && !pages.is_empty() {
            break;
        }
        match pages.last_mut() {
            Some((page, text)) if *page == chunk.page => text.push_str(&chunk.text),
            _ => pages.push((chunk.page, chunk.text.clone())),
        }
        used += chunk.text.len();
    }
    Ok(pages)
}

/// Char ceiling on [`pages_text_from_cache`] — ~4k tokens of source text,
/// the middle option the user picked (2026-09-04) between "top-4 chunks as
/// always" (4.5% of a real chapter — the false-positive banner) and "the
/// whole chapter" (a 50k+ char prompt that a free tier's ~8k TPM ceiling
/// throttles into 429s). Deliberately a constant, not config: it is a
/// model-economics knob, revisited when the default free pairing changes.
pub const SECTION_TEXT_CHAR_BUDGET: usize = 16_000;

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
pub(crate) fn heuristic_toc(pdf: &PdfDocument) -> Vec<String> {
    heuristic_toc_over(&pdf.page_texts)
}

/// Same heuristic as [`heuristic_toc`], over an arbitrary page-text slice —
/// lets S27g item 2's chapter split run it against just a chapter's own
/// page range instead of the whole book, keeping the model-call input to
/// structural signal instead of raw prose (measured 8000 TPM free-tier
/// ceiling, `docs/S27g-chapter-matching-measurements.md`).
pub(crate) fn heuristic_toc_over(pages: &[String]) -> Vec<String> {
    let from_contents_page = toc_page_heuristic(pages);
    if !from_contents_page.is_empty() {
        return from_contents_page;
    }
    heading_line_heuristic(pages)
}

fn toc_page_heuristic(pages: &[String]) -> Vec<String> {
    for page in pages {
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

fn heading_line_heuristic(pages: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for page in pages {
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

/// Persisted hash → filename lookup (S27n, PLAN.md) — lets `GET
/// /api/library/{hash}/pdf` resolve a `<cite data-source-id>` (a content
/// hash, emitted by `api::reading::ground_node`) straight back to the
/// library file it names, without rescanning and rehashing every PDF in the
/// library on every citation click. Written during
/// [`validate_acervo_with_progress`], the one place every candidate's hash
/// is already computed; a stale entry (file renamed/replaced since) simply
/// gets overwritten the next time validation runs — the store is a derived
/// cache, rebuildable from the library directory like every other index in
/// this module family, never the source of truth.
#[derive(Debug, Clone)]
pub struct LibraryFileIndex {
    dir: PathBuf,
}

/// One indexed library file — filename plus whatever embedded-metadata title
/// and author `load_candidates` already extracted while computing the hash
/// (`read_info_metadata`, S27c). Carrying title/authors here (instead of a
/// second lookup at request time) is what lets `GET /api/library/{hash}`
/// answer the source panel's meta request with zero extra PDF parsing — the
/// panel's title comes from the source's own embedded metadata, the same
/// signal the acervo gate's identity check already trusts, rather than any
/// one outline item's node title (chapters from the same book share a hash,
/// so no single outline item's title would be the right answer for all of
/// them).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryFileRecord {
    pub filename: String,
    pub title: Option<String>,
    pub authors: Option<String>,
}

impl LibraryFileIndex {
    /// Opens (creating if needed) `<index_root>/library_files/` — `index_root`
    /// is the `<data_dir>/index` directory, the parent every caller's
    /// `index_cache_dir` (`<data_dir>/index/library`) already shares.
    pub fn open(index_root: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = index_root.as_ref().join("library_files");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn get(&self, hash: &str) -> Option<LibraryFileRecord> {
        let bytes = fs::read(self.path_for(hash)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Atomic write (tmp file + rename), same idiom as
    /// [`build_index_cache`]/`ManualMatchStore::set`.
    pub fn set(
        &self,
        hash: &str,
        filename: &str,
        title: Option<&str>,
        authors: Option<&str>,
    ) -> std::io::Result<()> {
        let path = self.path_for(hash);
        let record = LibraryFileRecord {
            filename: filename.to_string(),
            title: title.map(str::to_string),
            authors: authors.map(str::to_string),
        };
        let json = serde_json::to_vec(&record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn path_for(&self, hash: &str) -> PathBuf {
        self.dir.join(format!("{hash}.json"))
    }

    /// The directory records live in — `ensure_library_file_index`
    /// (api::reading's cache-hit counterpart of the validation's own index
    /// write) scans it by filename before deciding anything needs re-hashing.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

/// SHA-256 hex digest of a PDF's bytes — the content-addressed key both the
/// retrieval-index cache and [`build_index_cache`] use.
pub(crate) fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Reads `/Info` dictionary `Title`/`Author` strings, when present. A thin
/// alias for `pdf::read_info_metadata` (the one implementation since S32,
/// 2026-09-03 — the trailer walk used to be duplicated here). A full
/// `lopdf` parse, so NOT for validation passes: `load_candidates` reads
/// metadata off the (cached) [`PdfDocument`] itself, and this remains only
/// for callers with no parsed document in hand — the S31
/// `ensure_library_file_index` backfill in `api::reading` and
/// `pdf::read_pdf_cached`'s one-time probe of legacy entries (see
/// `pdf::PdfDocument::meta_probed`).
pub(crate) fn read_info_metadata(path: &Path) -> (Option<String>, Option<String>) {
    super::pdf::read_info_metadata(path)
}

/// How strongly one library candidate matches an expected item, for the
/// S27f manual-pairing screen's ranking — coarser than [`IdentityCheck`]
/// (which only ever runs on an already presence-matched candidate); this
/// scores *every* candidate the same way [`check_identity`] scores the one
/// [`find_candidate`] happened to pick first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchConfidence {
    /// Title and at least one author surname both found.
    Strong,
    /// Only the title matched.
    Weak,
}

/// One library PDF that plausibly matches an expected item — the S27f
/// matching screen's raw material. **`filename` only, never a contract**
/// (same caveat as [`LibraryEntry::filename`]): a caller needs to go back
/// through [`LocalPdfSource`] to actually open the file.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateMatch {
    pub filename: String,
    pub confidence: MatchConfidence,
}

/// All plausible candidates for one expected item, not just the first.
/// [`find_candidate`]'s own doc comment flags this as its scope reduction —
/// "real disambiguation between several plausible candidates is the S27f
/// manual-match screen's job, not this engine's." This is that job's data
/// source. Reads the whole library on every call (like [`validate_acervo`]
/// itself) — a caller checking many items at once should use
/// [`match_report`] instead, which reads the library once for the whole
/// batch.
pub(crate) fn candidate_matches(
    library: &LocalPdfSource,
    item: &ExpectedItem,
) -> std::io::Result<Vec<CandidateMatch>> {
    let candidates = load_candidates(library)?;
    Ok(candidate_matches_over(item, &candidates))
}

fn candidate_matches_over(
    item: &ExpectedItem,
    candidates: &[LibraryCandidate],
) -> Vec<CandidateMatch> {
    let target_title = normalize(primary_title(&item.title));
    if target_title.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for cand in candidates {
        let haystack = candidate_haystack(cand);
        if !haystack.contains(&target_title) {
            continue;
        }
        let confidence = if item.authors.iter().any(|a| {
            let surname = normalize(surname_of(a));
            !surname.is_empty() && haystack.contains(&surname)
        }) {
            MatchConfidence::Strong
        } else {
            MatchConfidence::Weak
        };
        out.push(CandidateMatch {
            filename: cand.entry.filename.clone(),
            confidence,
        });
    }
    out
}

/// Library PDFs that don't plausibly match *any* expected item — the S27f
/// matching screen's other ambiguous case ("an unmatched library PDF"): a
/// book the user dropped in that the reading list doesn't know about yet.
/// Like [`candidate_matches`], reads the whole library on every call; prefer
/// [`match_report`] when checking a whole reading list at once.
pub(crate) fn unmatched_library_files(
    library: &LocalPdfSource,
    expected: &[ExpectedItem],
) -> std::io::Result<Vec<String>> {
    let candidates = load_candidates(library)?;
    let matched: std::collections::HashSet<String> = expected
        .iter()
        .flat_map(|item| candidate_matches_over(item, &candidates))
        .map(|m| m.filename)
        .collect();
    Ok(candidates
        .into_iter()
        .map(|c| c.entry.filename)
        .filter(|f| !matched.contains(f))
        .collect())
}

/// One combined pass over the library for the S27f matching screen: every
/// expected item's plausible candidates (parallel to `expected`, one vec per
/// item) plus every library file matched by none of them. Reads the library
/// **once** regardless of how many items are being checked — unlike calling
/// [`candidate_matches`]/[`unmatched_library_files`] once per item, which
/// each re-read and re-parse every PDF in the library from scratch. A caller
/// checking a whole reading list (the matching screen's actual shape) should
/// always prefer this.
pub(crate) fn match_report(
    library: &LocalPdfSource,
    expected: &[ExpectedItem],
) -> std::io::Result<(Vec<Vec<CandidateMatch>>, Vec<String>)> {
    let candidates = load_candidates(library)?;
    let per_item: Vec<Vec<CandidateMatch>> = expected
        .iter()
        .map(|item| candidate_matches_over(item, &candidates))
        .collect();
    let matched: std::collections::HashSet<String> = per_item
        .iter()
        .flatten()
        .map(|m| m.filename.clone())
        .collect();
    let unmatched = candidates
        .into_iter()
        .map(|c| c.entry.filename)
        .filter(|f| !matched.contains(f))
        .collect();
    Ok((per_item, unmatched))
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

    fn toc_dir(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join("data").join("index").join("toc")
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

        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
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
        let report =
            validate_acervo(&lib, &expected, &cache_dir, toc_dir(&tmp), None).expect("validate");
        assert!(matches!(report.items[0].index, IndexCheck::Cached { .. }));
    }

    /// S27k: once a deduction pass has stored a fully-resolved TOC for this
    /// hash, `check_toc` must report it as `Deduced` (not fall through to
    /// the heading heuristic) and `needs_user_confirmation()` must flip to
    /// false — there is nothing left to ask about.
    #[test]
    fn toc_check_reports_deduced_and_needs_no_confirmation_when_fully_resolved() {
        let (mut doc, _pages) = build_document(&["Some body text."], Some("Deduced Book"), None);
        let (tmp, lib) = place_in_library(&mut doc, "deduced.pdf");
        let cache_dir = index_dir(&tmp);
        let toc_confirm_dir = toc_dir(&tmp);
        fs::create_dir_all(&toc_confirm_dir).unwrap();

        let bytes = fs::read(lib.root().join("deduced.pdf")).unwrap();
        let hash = content_hash(&bytes);
        let toc_confirm = TocConfirmStore::open_at(&toc_confirm_dir).unwrap();
        toc_confirm
            .put_deduced(
                &hash,
                &crate::source::toc::TocResolution {
                    resolved: vec![crate::source::toc::ResolvedTocEntry {
                        title: "Chapter One".into(),
                        number: None,
                        page: 1,
                    }],
                    unresolved: Vec::new(),
                },
            )
            .unwrap();

        let expected = vec![ExpectedItem {
            title: "Deduced Book".into(),
            authors: vec![],
            kind: SourceKind::Article,
        }];
        let report =
            validate_acervo(&lib, &expected, &cache_dir, &toc_confirm_dir, None).expect("validate");
        match &report.items[0].toc {
            TocCheck::Deduced {
                resolved,
                unresolved,
            } => {
                assert_eq!(*resolved, 1);
                assert_eq!(*unresolved, 0);
            }
            other => panic!("expected Deduced, got {other:?}"),
        }
        assert!(!report.items[0].toc.needs_user_confirmation());
    }

    /// Same as above but with a leftover unresolved title — `Deduced` still
    /// applies (the deduction pass DID run and DID place entries), but
    /// `needs_user_confirmation()` must stay true: there's still something
    /// for the S27f screen to ask about.
    #[test]
    fn toc_check_deduced_with_unresolved_entries_still_needs_confirmation() {
        let (mut doc, _pages) = build_document(&["Some body text."], Some("Partial Book"), None);
        let (tmp, lib) = place_in_library(&mut doc, "partial.pdf");
        let cache_dir = index_dir(&tmp);
        let toc_confirm_dir = toc_dir(&tmp);
        fs::create_dir_all(&toc_confirm_dir).unwrap();

        let bytes = fs::read(lib.root().join("partial.pdf")).unwrap();
        let hash = content_hash(&bytes);
        let toc_confirm = TocConfirmStore::open_at(&toc_confirm_dir).unwrap();
        toc_confirm
            .put_deduced(
                &hash,
                &crate::source::toc::TocResolution {
                    resolved: vec![crate::source::toc::ResolvedTocEntry {
                        title: "Chapter One".into(),
                        number: None,
                        page: 1,
                    }],
                    unresolved: vec!["Appendix".into()],
                },
            )
            .unwrap();

        let expected = vec![ExpectedItem {
            title: "Partial Book".into(),
            authors: vec![],
            kind: SourceKind::Article,
        }];
        let report =
            validate_acervo(&lib, &expected, &cache_dir, &toc_confirm_dir, None).expect("validate");
        match &report.items[0].toc {
            TocCheck::Deduced {
                resolved,
                unresolved,
            } => {
                assert_eq!(*resolved, 1);
                assert_eq!(*unresolved, 1);
            }
            other => panic!("expected Deduced, got {other:?}"),
        }
        assert!(report.items[0].toc.needs_user_confirmation());
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
        let report =
            validate_acervo(&lib, &expected, &cache_dir, toc_dir(&tmp), None).expect("validate");
        assert_eq!(report.items[0].index, IndexCheck::Cached { path: built });
    }

    /// S27g item 1 (PLAN.md, 2026-08-30): `page_range` must actually exclude
    /// chunks outside it, not just carry the parameter. Uses `Embedder::Mock`
    /// (no model download, unlike the sibling test below) — the mock's
    /// hashed-bag-of-words vectors are enough to prove filtering, since this
    /// test only needs "which pages came back", not "which ranked first".
    #[test]
    fn search_index_cache_excludes_pages_outside_the_range() {
        let (mut doc, _pages) = build_document(
            &[
                "apple banana orchard fruit",
                "car truck engine highway",
                "dog cat kennel leash",
                "sun moon star galaxy",
            ],
            Some("Four Topics"),
            None,
        );
        let (tmp, lib) = place_in_library(&mut doc, "four.pdf");
        let cache_dir = index_dir(&tmp);

        let path = lib.root().join("four.pdf");
        let bytes = fs::read(&path).unwrap();
        let hash = content_hash(&bytes);
        let pdf = read_pdf(&path).unwrap();

        let embedder = Embedder::Mock;
        build_index_cache(&pdf, &hash, &cache_dir, &embedder).expect("build cache");

        // Narrowed to pages 2..=3 ("car truck..." / "dog cat...") — page 1
        // and page 4's chunks must never appear, regardless of query.
        let hits = search_index_cache(
            &cache_dir,
            &hash,
            &embedder,
            "car truck dog cat",
            4,
            Some((2, Some(3))),
        )
        .expect("search");
        assert!(!hits.is_empty(), "expected hits within the narrowed range");
        for (page, _text, _score) in &hits {
            assert!(
                (2..=3).contains(page),
                "page {page} fell outside the requested range"
            );
        }

        // A range that matches nothing in the cache (bad `resolved_page`,
        // e.g. from a stale match) must fall back to the whole book, not to
        // an empty/failed grounding — "no source coverage ⇒ no generation"
        // must never be tripped by a narrowing bug.
        let fallback_hits = search_index_cache(
            &cache_dir,
            &hash,
            &embedder,
            "car truck dog cat",
            4,
            Some((100, Some(200))),
        )
        .expect("search");
        assert!(
            !fallback_hits.is_empty(),
            "an out-of-bounds range must fall back to the whole book, not return nothing"
        );
    }

    /// The grounding-coverage fix (2026-09-04): section text comes back
    /// IN PAGE ORDER from the anchor page, inside the page range, up to the
    /// char budget — with the same whole-book fallback on a bad range as
    /// `search_index_cache`, and an empty cache yielding empty.
    #[test]
    fn pages_text_reads_in_order_from_the_anchor_within_budget() {
        let (mut doc, _pages) = build_document(
            &[
                "apple banana orchard fruit",
                "car truck engine highway",
                "dog cat kennel leash",
                "sun moon star galaxy",
            ],
            Some("Four Topics"),
            None,
        );
        let (tmp, lib) = place_in_library(&mut doc, "four.pdf");
        let cache_dir = index_dir(&tmp);
        let hash = content_hash(&fs::read(lib.root().join("four.pdf")).unwrap());
        let pdf = read_pdf(&lib.root().join("four.pdf")).unwrap();
        build_index_cache(&pdf, &hash, &cache_dir, &Embedder::Mock).expect("build cache");

        // Anchored mid-range (page 3): page 3's text comes first, then page
        // 4 — never page 1/2, which sit before the anchor.
        let pages = pages_text_from_cache(&cache_dir, &hash, Some((1, Some(4))), Some(3), 10_000)
            .expect("read");
        let pages: Vec<usize> = pages.iter().map(|(p, _)| *p).collect();
        assert_eq!(pages, vec![3, 4]);

        // No anchor: the window starts at the range's first page.
        let from_start =
            pages_text_from_cache(&cache_dir, &hash, Some((2, None)), None, 10_000).expect("read");
        assert_eq!(
            from_start.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );

        // Budget cuts the tail: a tiny cap still yields at least one page
        // (never an empty grounding from a nonzero cache), and never more
        // than fit.
        let capped = pages_text_from_cache(&cache_dir, &hash, Some((1, Some(4))), Some(1), 10)
            .expect("read");
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].0, 1);

        // A range matching nothing falls back to the whole book, same as
        // `search_index_cache` — a narrowing bug must never read as "no
        // source coverage".
        let fallback =
            pages_text_from_cache(&cache_dir, &hash, Some((100, Some(200))), None, 10_000)
                .expect("read");
        assert_eq!(
            fallback.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );

        // A missing cache is an Err, not empty — same contract as
        // `search_index_cache` ("errors only on I/O/corruption; an
        // empty/missing cache is the caller's job to have ruled out via
        // `IndexCheck` first"). ground_node's internal-error path, never a
        // silent skip.
        assert!(
            pages_text_from_cache(&cache_dir, "no-such-hash", None, None, 10_000).is_err(),
            "a missing cache file must surface as an error, not empty coverage"
        );
    }

    /// S27m (PLAN.md, 2026-08-29): proves the read side of the gate's
    /// grounding fix — a hit carries the **physical page** it actually came
    /// from (the gap the old `pdf.text`-chunked cache had no way to close),
    /// and a query about one page's topic ranks that page's chunk first
    /// among a book that also discusses something unrelated elsewhere.
    /// Ignored by default (downloads the embedding model); run with
    /// `cargo test -p learnive search_index_cache_scores_the_right_page_first -- --ignored`.
    #[test]
    #[ignore = "downloads the embedding model"]
    fn search_index_cache_scores_the_right_page_first() {
        let (mut doc, _pages) = build_document(
            &[
                "Baking bread requires flour, water, yeast and salt.",
                "A recursive function calls itself with a smaller input until it reaches a base case.",
            ],
            Some("Mixed Topics"),
            None,
        );
        let (tmp, lib) = place_in_library(&mut doc, "mixed.pdf");
        let cache_dir = index_dir(&tmp);

        let path = lib.root().join("mixed.pdf");
        let bytes = fs::read(&path).unwrap();
        let hash = content_hash(&bytes);
        let pdf = read_pdf(&path).unwrap();

        let embedder = Embedder::default_model().expect("load embedder");
        build_index_cache(&pdf, &hash, &cache_dir, &embedder).expect("build cache");

        let hits = search_index_cache(
            &cache_dir,
            &hash,
            &embedder,
            "recursive functions in programming",
            2,
            None,
        )
        .expect("search");
        assert!(!hits.is_empty(), "expected at least one hit");
        let (top_page, top_text, _score) = &hits[0];
        assert_eq!(*top_page, 2, "the recursion page should rank first");
        assert!(top_text.to_lowercase().contains("recursive"));
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
        let report = validate_acervo(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None)
            .expect("validate");
        let item = &report.items[0];
        assert!(item.passes(), "{item:?}");
        assert!(report.all_pass());
        assert!(report.failing_items().is_empty());
    }

    // -- S27f: candidate matching (matching screen) ------------------------

    fn place_two_in_library(
        doc_a: &mut Document,
        filename_a: &str,
        doc_b: &mut Document,
        filename_b: &str,
    ) -> (tempfile::TempDir, LocalPdfSource) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let lib = LocalPdfSource::open(dir.path().join("data")).expect("open library");
        doc_a
            .save(lib.root().join(filename_a))
            .expect("save fixture pdf a");
        doc_b
            .save(lib.root().join(filename_b))
            .expect("save fixture pdf b");
        (dir, lib)
    }

    #[test]
    fn candidate_matches_returns_every_plausible_candidate_not_just_the_first() {
        // Two library PDFs both plausibly titled the expected work — exactly
        // `find_candidate`'s flagged scope reduction ("real disambiguation...
        // is the S27f manual-match screen's job"). One has the right author
        // (strong), one doesn't (weak) — both must come back, not just
        // whichever `find_candidate` would have picked first.
        let (mut doc_a, _) = build_document(
            &["A textbook by Michael Sipser."],
            Some("Introduction to the Theory of Computation"),
            Some("Michael Sipser"),
        );
        let (mut doc_b, _) = build_document(
            &["A different edition, different author."],
            Some("Introduction to the Theory of Computation"),
            Some("Someone Else"),
        );
        let (_tmp, lib) = place_two_in_library(&mut doc_a, "a.pdf", &mut doc_b, "b.pdf");

        let item = ExpectedItem {
            title: "Introduction to the Theory of Computation".into(),
            authors: vec!["Michael Sipser".into()],
            kind: SourceKind::Book,
        };
        let mut matches = candidate_matches(&lib, &item).expect("match");
        matches.sort_by(|a, b| a.filename.cmp(&b.filename));
        assert_eq!(
            matches,
            vec![
                CandidateMatch {
                    filename: "a.pdf".into(),
                    confidence: MatchConfidence::Strong,
                },
                CandidateMatch {
                    filename: "b.pdf".into(),
                    confidence: MatchConfidence::Weak,
                },
            ]
        );
    }

    #[test]
    fn unmatched_library_files_lists_a_pdf_that_matches_no_expected_item() {
        let (mut doc, _) = build_document(
            &["A completely unrelated cookbook, by Jane Chef."],
            Some("The Joy of Baking"),
            Some("Jane Chef"),
        );
        let (_tmp, lib) = place_in_library(&mut doc, "baking.pdf");

        let expected = vec![ExpectedItem {
            title: "Introduction to the Theory of Computation".into(),
            authors: vec!["Michael Sipser".into()],
            kind: SourceKind::Book,
        }];
        let unmatched = unmatched_library_files(&lib, &expected).expect("scan");
        assert_eq!(unmatched, vec!["baking.pdf".to_string()]);
    }

    #[test]
    fn match_report_combines_per_item_candidates_and_unmatched_files_in_one_library_pass() {
        let (mut doc_a, _) = build_document(
            &["A textbook by Michael Sipser."],
            Some("Introduction to the Theory of Computation"),
            Some("Michael Sipser"),
        );
        let (mut doc_b, _) = build_document(
            &["A completely unrelated cookbook, by Jane Chef."],
            Some("The Joy of Baking"),
            Some("Jane Chef"),
        );
        let (_tmp, lib) = place_two_in_library(&mut doc_a, "sipser.pdf", &mut doc_b, "baking.pdf");

        let expected = vec![ExpectedItem {
            title: "Introduction to the Theory of Computation".into(),
            authors: vec!["Michael Sipser".into()],
            kind: SourceKind::Book,
        }];
        let (per_item, unmatched) = match_report(&lib, &expected).expect("match report");
        assert_eq!(
            per_item,
            vec![vec![CandidateMatch {
                filename: "sipser.pdf".into(),
                confidence: MatchConfidence::Strong,
            }]]
        );
        assert_eq!(unmatched, vec!["baking.pdf".to_string()]);
    }

    /// The report screen's per-item phase text is driven by these ticks.
    /// The shared library scan (`load_candidates` — read + hash + parse of
    /// every PDF, before any per-item check runs) is the longest single
    /// stretch of a real validation and used to emit nothing, so the screen
    /// spent its whole duration with every row on the initial "Queued…"
    /// label (bug reported live 2026-09-03). Every item must get a
    /// `Scanning` tick first, in expected order, before any check phase.
    #[test]
    fn progress_starts_with_one_scanning_tick_per_item_before_any_check() {
        let (mut doc, _pages) = build_document(
            &["A textbook by Michael Sipser."],
            Some("Introduction to the Theory of Computation"),
            Some("Michael Sipser"),
        );
        let (tmp, lib) = place_in_library(&mut doc, "sipser.pdf");

        let expected = vec![
            ExpectedItem {
                title: "Introduction to the Theory of Computation".into(),
                authors: vec!["Michael Sipser".into()],
                kind: SourceKind::Book,
            },
            // Deliberately present only as an expected item — its row still
            // needs a Scanning tick (it waits on the same shared scan), and
            // its checks end at Presence since nothing matches it.
            ExpectedItem {
                title: "A Book Nobody Has".into(),
                authors: vec![],
                kind: SourceKind::Book,
            },
        ];

        let mut ticks = Vec::new();
        validate_acervo_with_progress(&lib, &expected, index_dir(&tmp), toc_dir(&tmp), None, |p| {
            ticks.push(p)
        })
        .expect("validate");

        assert_eq!(ticks[0].phase, AcervoPhase::Scanning);
        assert_eq!(ticks[0].title, "Introduction to the Theory of Computation");
        assert_eq!(ticks[1].phase, AcervoPhase::Scanning);
        assert_eq!(ticks[1].title, "A Book Nobody Has");
        // The scan's ticks come first, and the first per-item check phase
        // belongs to the first item, not a second burst of Scanning.
        assert_eq!(ticks[2].phase, AcervoPhase::Presence);
        assert_eq!(ticks[2].title, "Introduction to the Theory of Computation");
        assert!(
            ticks
                .iter()
                .skip(2)
                .all(|t| t.phase != AcervoPhase::Scanning)
        );
    }
}
