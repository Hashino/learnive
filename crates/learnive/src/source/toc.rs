//! S27k: the app deduces a PDF's table of contents instead of asking the
//! user to type it chapter by chapter (PLAN.md, decided 2026-08-29 after a
//! user objection to S27f's blank-form fallback: *"a aplicação deveria
//! tornar aprender coisas complexas sem esforço"*). Pure title-matching and
//! resolution logic only — the LLM call that produces [`TocLlmEntry`] lives
//! in `engine::propose_toc` (this module stays free of `Ai`/tokio, matching
//! the rest of `source`'s discipline).
//!
//! Cascade (SPEC §11.1's original three steps, with a new middle step):
//! embedded bookmarks (handled entirely by `acervo::check_toc`/
//! `pdf::OutlineEntry`, before this module is ever consulted) → **a printed
//! contents/sumário page, read by the model** ([`find_contents_pages`] +
//! [`resolve_toc`], this module) → the heading-line heuristic
//! (`acervo::heuristic_toc`, unchanged, tried only when this module gives up
//! too) → a per-chapter question to the user (`toc_confirm.rs`'s store,
//! extended to hold only what this module couldn't resolve).
//!
//! **Why a printed page number is never used as a physical offset directly**
//! (the bug the user's own objection identified in the old design): printed
//! and physical pagination diverge (front matter, plates, blank pages) — see
//! [`resolve_toc`]'s doc for the actual resolution rule.

use std::collections::HashMap;

use serde::Deserialize;

use super::matching::normalize;
use super::pdf::PdfDocument;

/// One entry the model read off a printed contents/sumário page: a title and
/// the page number as PRINTED there, if the line had one. Never used as a
/// physical page offset directly — see the module doc.
///
/// `number` (S27g, 2026-08-30): the entry's own printed chapter/section
/// number verbatim (e.g. `"4"`, `"4.10"`, `"2.2.1"`), `None` when the line
/// carried no numbering at all. Kept as a plain string, never parsed into
/// integers — a book's own numbering scheme is whatever it prints, and this
/// only ever needs to be compared back against another string
/// (`toc_confirm::match_chapter`), never computed with. This reverses an
/// earlier version of this prompt/struct that dropped numbering entirely
/// ("drop leading numbering like '1.'") on the theory that a chapter's
/// title was the only stable handle; the user later asked for numbers too,
/// specifically so `propose_outline`'s proposed chapters can carry BOTH and
/// match on whichever the real book's TOC actually resolves.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TocLlmEntry {
    pub title: String,
    #[serde(default)]
    pub number: Option<String>,
    #[serde(default)]
    pub page: Option<i64>,
}

/// One chapter [`resolve_toc`] placed on a real physical page.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTocEntry {
    pub title: String,
    /// Carried through verbatim from [`TocLlmEntry::number`] — see its doc.
    pub number: Option<String>,
    /// 1-based physical page — matches `PdfDocument::page_texts`/`PageMap`'s
    /// convention everywhere else in `source`.
    pub page: usize,
}

/// The outcome of one deduction pass: what got placed, and what didn't.
/// [`is_resolution_acceptable`] decides whether this is worth keeping at
/// all; a caller that keeps it only ever needs to ask the user about
/// `unresolved` — this is the entire point of S27k over the old blank form.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TocResolution {
    pub resolved: Vec<ResolvedTocEntry>,
    pub unresolved: Vec<String>,
}

/// How many leading physical pages count as "front matter" to search for a
/// contents page — matches `acervo::toc_page_heuristic`'s existing scope.
const CONTENTS_SEARCH_WINDOW: usize = 40;

/// A page counts as "contents-like" once at least this fraction of its
/// non-empty lines end in a short number — the same signal
/// `acervo::toc_page_heuristic` uses per-line, aggregated here so a
/// multi-page contents run can be detected as a contiguous block instead of
/// only ever the first matching page.
const NUMBERED_LINE_RATIO_THRESHOLD: f64 = 0.4;

/// Below this fraction resolved, the whole deduction is discarded in favor
/// of the heading heuristic (PLAN.md: "abaixo de um piso de resolução (ordem
/// de metade dos capítulos) ... joga-se fora a dedução inteira"). Exact
/// value is a placeholder pending S27k's own calibration test (PLAN.md
/// names this explicitly as something "decidimos com base em testes", not
/// on paper) — not yet run, so this is a reasonable prior, not a settled
/// number.
const MIN_RESOLUTION_FRACTION: f64 = 0.5;

/// A chapter title, when a book prints it at all, is always within this
/// opening span of a page's extracted text — keeping the window small
/// avoids a title coincidentally appearing deep in a chapter's own body
/// (e.g. a cross-reference) matching before the chapter's real start.
const TOP_OF_PAGE_CHARS: usize = 200;

/// Locates a contiguous run of contents/sumário-like pages within the first
/// [`CONTENTS_SEARCH_WINDOW`] physical pages — the input `engine::propose_toc`
/// should be given, and the point after which [`resolve_toc`] is allowed to
/// search (SPEC: exclude the pre-textual range, "senão todo título casa com
/// a própria página de sumário"). Returns 0-based inclusive
/// `(first_page, last_page)` physical-page indices, or `None` if nothing in
/// the window looks like one — the caller falls straight through to the
/// heading heuristic without ever calling the model.
pub fn find_contents_pages(pdf: &PdfDocument) -> Option<(usize, usize)> {
    let mut start = None;
    let mut end = None;
    for (i, page) in pdf
        .page_texts
        .iter()
        .take(CONTENTS_SEARCH_WINDOW)
        .enumerate()
    {
        let titled = has_contents_heading(page);
        let looks_like_contents =
            titled || numbered_line_ratio(page) >= NUMBERED_LINE_RATIO_THRESHOLD;
        if looks_like_contents {
            start.get_or_insert(i);
            end = Some(i);
        } else if start.is_some() {
            break; // contiguous run only
        }
    }
    start.zip(end)
}

/// Does one of this page's opening lines read as a table-of-contents
/// heading?
///
/// **Fuzzy, not an exact substring** (fixed 2026-08-30): the heading on a
/// scanned book is OCR output, and OCR confuses letter shapes. K&R's contents
/// page comes out as `"CONIENTS"` (T read as I), which an exact
/// `contains("contents")` misses — and missing the heading was enough to
/// abandon the whole book, since nothing downstream can run without a
/// contents range.
///
/// **Matched against a whole LINE, not against any word on the page**, which
/// is the guard that makes fuzziness safe here: `"content"` and `"context"`
/// are ordinary English words scoring 0.98 and 0.94 against `"contents"`, so
/// a word-level check fires on any prose page that happens to use one (caught
/// by `find_contents_pages_ignores_ordinary_prose` while writing this). A
/// real heading stands alone on its line; a prose sentence never does.
fn has_contents_heading(page: &str) -> bool {
    const KEYWORDS: [&str; 5] = ["contents", "sumário", "sumario", "índice", "indice"];
    page.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(4)
        .any(|line| {
            let l: String = line
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase();
            l.len() >= 5
                && l.len() <= 12
                && KEYWORDS
                    .iter()
                    .any(|k| strsim::jaro_winkler(&l, k) >= CONTENTS_HEADING_SIMILARITY)
        })
}

/// How close an OCR'd word must be to "contents" (or a locale equivalent) to
/// count as the heading. `"conients"` — K&R's actual OCR output — scores
/// 0.94 against `"contents"`, so this leaves real room while staying well
/// clear of ordinary prose words.
const CONTENTS_HEADING_SIMILARITY: f64 = 0.88;

/// Fraction of a page's non-empty lines that look like table-of-contents
/// rows. Three signals, because real contents pages extract in more than one
/// shape (all three measured against the library on 2026-08-30):
///
/// 1. the line ENDS in a bare page number — the classic `Title .... 42` row;
/// 2. the line IS a bare section number — K&R's scan puts `1.1`, `1.2`, …
///    on their own lines, separated from the titles they belong to, so
///    signal 1 finds nothing at all on a page that is obviously a contents
///    page to a human;
/// 3. the line STARTS with a section number — `3.1 Statements and Blocks`,
///    the shape the same book uses two pages later.
///
/// Before signals 2 and 3 existed, K&R scored ~0 here and, with its OCR-
/// damaged heading also missed, the cascade could not start on it at all.
fn numbered_line_ratio(page: &str) -> f64 {
    let lines: Vec<&str> = page
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return 0.0;
    }
    let numbered = lines
        .iter()
        .filter(|l| {
            let ends_in_page_number = l.split_whitespace().next_back().is_some_and(|tok| {
                !tok.is_empty() && tok.len() <= 4 && tok.chars().all(|c| c.is_ascii_digit())
            });
            let first = l.split_whitespace().next().unwrap_or("");
            let is_section_number = is_section_number(first);
            // A line that is ONLY a section number, or one that opens with
            // one and then names something.
            // `first` is the line's first whitespace token, so equal lengths
            // mean the line is nothing but that section number.
            ends_in_page_number || (is_section_number && l.len() >= first.len())
        })
        .count();
    numbered as f64 / lines.len() as f64
}

/// `"1"`, `"1.1"`, `"2.10"`, `"4.10."` — a hierarchical section number and
/// nothing else. Bounded in length so a stray decimal in body prose (a
/// measurement, a version) can't masquerade as one.
fn is_section_number(tok: &str) -> bool {
    let t = tok.trim_end_matches('.');
    !t.is_empty()
        && t.len() <= 8
        && t.starts_with(|c: char| c.is_ascii_digit())
        && t.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Joins the text of a contents-page run ([`find_contents_pages`]'s output)
/// into the single string `engine::propose_toc` feeds the model — a few KB
/// of pre-textual, never the book's body (PLAN.md's cost argument for why
/// this call is affordable at all).
pub fn contents_pages_text(pdf: &PdfDocument, range: (usize, usize)) -> String {
    contents_page_chunks(pdf, range).join("\n")
}

/// The same run, kept **one string per physical page** — the unit
/// `engine::propose_toc` actually sends.
///
/// Joining the run into a single prompt is what broke the whole S27k
/// cascade (measured 2026-08-30): Think Python's contents run is 33 KB, and
/// a reasoning model asked to transcribe it narrates every entry in its
/// reasoning channel and hits the provider's token ceiling before writing
/// the first character of JSON — `finish_reason: "length"`, which surfaced
/// four layers up as `Parse("no JSON")`. A printed contents page is a
/// self-contained list, so the split costs nothing in comprehension, bounds
/// each response to something a free-tier budget can hold, and makes a
/// failure lose one page instead of the book.
pub fn contents_page_chunks(pdf: &PdfDocument, range: (usize, usize)) -> Vec<String> {
    let (start, end) = range;
    pdf.page_texts
        .get(start..=end.min(pdf.page_texts.len().saturating_sub(1)))
        .map(|pages| {
            pages
                .iter()
                .filter(|p| !p.trim().is_empty())
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Resolves the model's read of a contents page against real physical pages,
/// by text — never by printed-number-as-offset (module doc). `entries` is
/// read in order (SPEC: reading order carries the sequence, same convention
/// `engine::propose_outline` already uses); `contents_end_page0` is
/// [`find_contents_pages`]'s own `end` (0-based, inclusive) — the search
/// space starts strictly after it, excluding the pre-textual range.
///
/// The rule (PLAN.md §11.1/S27k, decision delegated to the implementer,
/// 2026-08-29):
/// 1. **Candidates by title.** Normalize (shared [`normalize`]) and look for
///    the title at the TOP of each physical page's text ([`TOP_OF_PAGE_CHARS`]),
///    which also naturally **collapses repeated running headers** to their
///    first occurrence (a later page carrying the same running header is
///    simply never reached — search for the NEXT entry starts strictly past
///    whichever page satisfied this one).
/// 2. **Monotonicity as a hard constraint.** Each entry's placement must
///    land on a physical page strictly after the previous entry's — a
///    failed placement doesn't block the entries after it (the search
///    window for the next entry doesn't advance).
/// 3. **Offset derivation, zero extra token cost.** For every entry that
///    both resolved by title AND carried a printed page number, compute
///    `delta = physical − printed`; if a delta value dominates the set,
///    apply it to unresolved entries that have a printed number and accept
///    the prediction only if that exact predicted page's title match
///    confirms it (never blind arithmetic alone). Confirmation reads the
///    **whole** predicted page, not just its top ([`page_contains`]) — a
///    subsection starts mid-page, and the pinned page makes the wider read
///    safe.
/// 4. Whatever is left is `unresolved` — the only thing worth asking the
///    user about now.
pub fn resolve_toc(
    pdf: &PdfDocument,
    entries: &[TocLlmEntry],
    contents_end_page0: usize,
) -> TocResolution {
    if entries.is_empty() {
        return TocResolution::default();
    }

    let mut last_page = contents_end_page0 + 1; // 1-based page of the last contents page
    let mut placements: Vec<Option<usize>> = Vec::with_capacity(entries.len());

    for entry in entries {
        let needle = normalize(&entry.title);
        if needle.is_empty() {
            placements.push(None);
            continue;
        }
        let found = ((last_page + 1)..=pdf.page_texts.len())
            .find(|&physical| page_top_matches(pdf, physical, &needle));
        if let Some(page) = found {
            last_page = page;
        }
        placements.push(found);
    }

    if let Some(delta) = dominant_offset(entries, &placements) {
        for (entry, placement) in entries.iter().zip(placements.iter_mut()) {
            if placement.is_some() {
                continue;
            }
            let Some(printed) = entry.page else { continue };
            let predicted = printed + delta;
            if predicted < 1 {
                continue;
            }
            let predicted = predicted as usize;
            if predicted > pdf.page_texts.len() {
                continue;
            }
            let needle = normalize(&entry.title);
            if !needle.is_empty() && page_contains(pdf, predicted, &needle) {
                *placement = Some(predicted);
            }
        }
    }

    let mut resolution = TocResolution::default();
    for (entry, placement) in entries.iter().zip(placements) {
        match placement {
            Some(page) => resolution.resolved.push(ResolvedTocEntry {
                title: entry.title.clone(),
                number: entry.number.clone(),
                page,
            }),
            None => resolution.unresolved.push(entry.title.clone()),
        }
    }
    resolution
}

fn page_top_matches(pdf: &PdfDocument, physical_page: usize, needle: &str) -> bool {
    let Some(page_text) = pdf.page_texts.get(physical_page - 1) else {
        return false;
    };
    let head: String = page_text.chars().take(TOP_OF_PAGE_CHARS).collect();
    normalize(&head).contains(needle)
}

/// Whole-page variant of [`page_top_matches`].
///
/// Only ever used to confirm an *arithmetic* prediction, never to search.
/// The top-of-page rule exists because a free forward scan over full page
/// text would happily stop on a cross-reference or a running header; that
/// risk is gone when the page is already pinned by the dominant offset —
/// there is exactly one page to check, and the question is just "is this
/// entry anywhere on it".
///
/// Measured 2026-08-30 (docs §3.5): the top-of-page rule was written when
/// entries were chapters, and a chapter starts on a fresh page. The
/// deduction cascade now returns mostly **subsections**, which start
/// mid-page and therefore could not match by construction — Stewart placed
/// 97 of 234.
fn page_contains(pdf: &PdfDocument, physical_page: usize, needle: &str) -> bool {
    pdf.page_texts
        .get(physical_page - 1)
        .is_some_and(|text| normalize(text).contains(needle))
}

/// The dominant `physical − printed` delta across entries resolved by title
/// AND carrying a printed number — `None` when no entry offers both, or
/// when there's nothing to derive from.
fn dominant_offset(entries: &[TocLlmEntry], placements: &[Option<usize>]) -> Option<i64> {
    let deltas: Vec<i64> = entries
        .iter()
        .zip(placements)
        .filter_map(|(entry, placement)| match (entry.page, placement) {
            (Some(printed), Some(physical)) => Some(*physical as i64 - printed),
            _ => None,
        })
        .collect();
    mode(&deltas)
}

fn mode(values: &[i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for &v in values {
        *counts.entry(v).or_insert(0) += 1;
    }
    counts.into_iter().max_by_key(|&(_, c)| c).map(|(v, _)| v)
}

/// Whether a resolution is good enough to keep, or should be discarded
/// entirely in favor of the heading heuristic — PLAN.md's "descarte do
/// parse inteiro, não conserto capítulo a capítulo": a low resolution rate
/// means the model likely hallucinated the chapter list, and asking the
/// user to fix it title-by-title in that case is the worst outcome, not a
/// helpful safety net.
pub fn is_resolution_acceptable(resolution: &TocResolution) -> bool {
    let total = resolution.resolved.len() + resolution.unresolved.len();
    total > 0 && (resolution.resolved.len() as f64 / total as f64) >= MIN_RESOLUTION_FRACTION
}

#[cfg(test)]
mod tests {
    use super::super::pdf::PageMap;
    use super::*;

    fn doc(pages: &[&str]) -> PdfDocument {
        let page_texts: Vec<String> = pages.iter().map(|s| s.to_string()).collect();
        PdfDocument {
            text_layer_unreadable: false,
            text: page_texts.join("\n"),
            page_texts,
            outline: Vec::new(),
            pages: PageMap::default(),
            meta_title: None,
            meta_author: None,
            meta_probed: true,
        }
    }

    fn entry(title: &str, page: Option<i64>) -> TocLlmEntry {
        TocLlmEntry {
            title: title.to_string(),
            number: None,
            page,
        }
    }

    /// K&R's scan renders the heading as "CONIENTS" (T misread as I). An
    /// exact substring check missed it, and with the section numbers also
    /// extracting onto their own lines the page scored ~0 — enough to
    /// abandon the entire book. Both signals are exercised here.
    #[test]
    fn find_contents_pages_survives_an_ocr_damaged_heading() {
        let pdf = doc(&[
            "Cover",
            "CONIENTS\nPreface\nChapter 0\n1.1\n1.2\n1.3",
            "Introduction\nSome real body text starts here and runs on.",
        ]);
        assert_eq!(find_contents_pages(&pdf), Some((1, 1)));
    }

    /// Signal 2: the page is a column of bare section numbers, with the
    /// titles extracted somewhere else entirely — no line ends in a page
    /// number, so the classic `Title .... 42` signal finds nothing.
    #[test]
    fn find_contents_pages_detects_a_column_of_bare_section_numbers() {
        let pdf = doc(&[
            "Cover",
            "1.1\n1.2\n1.3\n2.1\n2.2\nChapter 2",
            "Body text with no numbers at all in it whatsoever.",
        ]);
        assert_eq!(find_contents_pages(&pdf), Some((1, 1)));
    }

    /// Signal 3: lines that OPEN with a section number and then name it.
    #[test]
    fn find_contents_pages_detects_leading_section_numbers() {
        let pdf = doc(&[
            "Cover",
            "3.1 Statements and Blocks\n3.2 If-Else\n3.3 Else-If\n3.4 Switch",
            "Body prose that carries none of that shape.",
        ]);
        assert_eq!(find_contents_pages(&pdf), Some((1, 1)));
    }

    /// The fuzzy heading must not fire on ordinary prose — "contents" is
    /// close to nothing common, but the guard is worth pinning.
    #[test]
    fn find_contents_pages_ignores_ordinary_prose() {
        let pdf = doc(&[
            "Cover",
            "The context of this content is consistent with earlier claims.",
            "More prose.",
        ]);
        assert_eq!(find_contents_pages(&pdf), None);
    }

    #[test]
    fn find_contents_pages_detects_a_titled_contents_page() {
        let pdf = doc(&[
            "Cover",
            "Contents\nIntroduction .......... 1\nChapter One .......... 5",
            "Introduction\nSome real text starts here.",
        ]);
        assert_eq!(find_contents_pages(&pdf), Some((1, 1)));
    }

    #[test]
    fn find_contents_pages_extends_over_a_multi_page_run() {
        let pdf = doc(&[
            "Cover",
            "Sumário\nCap 1 .......... 1",
            "Cap 2 .......... 40\nCap 3 .......... 80",
            "Introduction\nReal body text with no trailing numbers at all here.",
        ]);
        assert_eq!(find_contents_pages(&pdf), Some((1, 2)));
    }

    #[test]
    fn find_contents_pages_is_none_without_any_signal() {
        let pdf = doc(&["Cover", "Just some prose, no numbers, no title."]);
        assert_eq!(find_contents_pages(&pdf), None);
    }

    #[test]
    fn resolve_toc_places_entries_by_title_in_monotonic_order() {
        // page 0: contents; page 1: "Introduction"; page 2: filler; page 3: "Chapter One"
        let padding = "x".repeat(TOP_OF_PAGE_CHARS);
        let filler_page = format!(
            "{padding}\nmentions Chapter One deep in the body, past the top-of-page window"
        );
        let pdf = doc(&[
            "Contents",
            "Introduction\nBody text.",
            filler_page.as_str(),
            "Chapter One\nReal chapter body.",
        ]);
        let entries = vec![
            entry("Introduction", Some(1)),
            entry("Chapter One", Some(5)),
        ];
        let resolution = resolve_toc(&pdf, &entries, 0);
        assert_eq!(resolution.unresolved, Vec::<String>::new());
        assert_eq!(
            resolution.resolved[0],
            ResolvedTocEntry {
                title: "Introduction".into(),
                number: None,
                page: 2
            }
        );
        assert_eq!(
            resolution.resolved[1],
            ResolvedTocEntry {
                title: "Chapter One".into(),
                number: None,
                page: 4
            }
        );
    }

    /// A chapter starts on a fresh page; a subsection starts mid-page. The
    /// forward scan (top-of-page only) can never see the subsection, so it
    /// has to come in through the offset prediction, whose confirmation
    /// reads the whole page. Measured 2026-08-30, docs §3.5: without this,
    /// Stewart placed 97 of 234 entries because most of them were `N.M`.
    #[test]
    fn resolve_toc_places_a_subsection_that_starts_mid_page() {
        let padding = "x".repeat(TOP_OF_PAGE_CHARS);
        // Physical 3 = printed 1, so delta = 2 for every entry below.
        let mid_page = format!("{padding}\n1.1 Four Ways to Represent a Function\nBody.");
        let pdf = doc(&[
            "Contents",
            "front matter",
            "Functions and Models\nChapter opener body.",
            mid_page.as_str(),
        ]);
        let entries = vec![
            entry("Functions and Models", Some(1)),
            entry("Four Ways to Represent a Function", Some(2)),
        ];

        let resolution = resolve_toc(&pdf, &entries, 0);

        assert_eq!(resolution.unresolved, Vec::<String>::new());
        // The chapter anchors the offset by landing at the top of physical 3.
        assert_eq!(resolution.resolved[0].page, 3);
        // The subsection is only reachable through that offset + a
        // whole-page read of the page it predicts.
        assert_eq!(resolution.resolved[1].page, 4);
    }

    #[test]
    fn resolve_toc_does_not_let_a_failed_entry_block_the_next_one() {
        let pdf = doc(&[
            "Contents",
            "Some page with neither title on it.",
            "Chapter Two\nReal body.",
        ]);
        let entries = vec![entry("Chapter One", None), entry("Chapter Two", None)];
        let resolution = resolve_toc(&pdf, &entries, 0);
        assert_eq!(resolution.unresolved, vec!["Chapter One".to_string()]);
        assert_eq!(
            resolution.resolved,
            vec![ResolvedTocEntry {
                title: "Chapter Two".into(),
                number: None,
                page: 3
            }]
        );
    }

    #[test]
    fn resolve_toc_never_places_an_entry_before_the_previous_one() {
        // "Appendix" title text physically appears (in passing) on an EARLIER
        // page than "Chapter One" — monotonicity must reject placing it there
        // once Chapter One has already claimed a later page in reading order.
        let pdf = doc(&[
            "Contents",
            "Appendix mentioned here just in passing, not as a real heading.",
            "Chapter One\nReal body.",
            "Appendix\nReal appendix body.",
        ]);
        let entries = vec![entry("Chapter One", None), entry("Appendix", None)];
        let resolution = resolve_toc(&pdf, &entries, 0);
        assert_eq!(
            resolution.resolved,
            vec![
                ResolvedTocEntry {
                    title: "Chapter One".into(),
                    number: None,
                    page: 3
                },
                ResolvedTocEntry {
                    title: "Appendix".into(),
                    number: None,
                    page: 4
                },
            ]
        );
    }

    #[test]
    fn resolve_toc_derives_an_offset_and_recovers_an_otherwise_unresolved_entry() {
        // Chapter One resolves directly (printed 1 -> physical 3, delta +2).
        // Chapter Two's own title text never appears verbatim (say, OCR noise
        // put a stray character in it) so direct matching alone would fail —
        // but the derived +2 offset (printed 10 -> predicted physical 12)
        // lands on a page whose top DOES match "Chapter Two" once tried, so
        // it should still resolve.
        let mut pages = vec!["Contents".to_string(), "Filler".to_string()];
        pages.push("Chapter One\nBody.".to_string()); // physical 3, printed 1 (delta 2)
        for i in 0..8 {
            pages.push(format!("Filler page {i}"));
        }
        pages.push("Chapter Two\nBody.".to_string()); // physical 12, printed 10 (delta 2)
        let pdf = doc(&pages.iter().map(String::as_str).collect::<Vec<_>>());

        let entries = vec![
            entry("Chapter One", Some(1)),
            entry("Chapter Two", Some(10)),
        ];
        let resolution = resolve_toc(&pdf, &entries, 0);
        assert_eq!(resolution.unresolved, Vec::<String>::new());
        assert_eq!(
            resolution.resolved,
            vec![
                ResolvedTocEntry {
                    title: "Chapter One".into(),
                    number: None,
                    page: 3
                },
                ResolvedTocEntry {
                    title: "Chapter Two".into(),
                    number: None,
                    page: 12
                },
            ]
        );
    }

    #[test]
    fn is_resolution_acceptable_rejects_a_mostly_failed_parse() {
        let mostly_failed = TocResolution {
            resolved: vec![ResolvedTocEntry {
                title: "A".into(),
                number: None,
                page: 1,
            }],
            unresolved: vec!["B".into(), "C".into(), "D".into()],
        };
        assert!(!is_resolution_acceptable(&mostly_failed));

        let mostly_resolved = TocResolution {
            resolved: vec![
                ResolvedTocEntry {
                    title: "A".into(),
                    number: None,
                    page: 1,
                },
                ResolvedTocEntry {
                    title: "B".into(),
                    number: None,
                    page: 2,
                },
                ResolvedTocEntry {
                    title: "C".into(),
                    number: None,
                    page: 3,
                },
            ],
            unresolved: vec!["D".into()],
        };
        assert!(is_resolution_acceptable(&mostly_resolved));
    }

    #[test]
    fn is_resolution_acceptable_rejects_an_empty_result() {
        assert!(!is_resolution_acceptable(&TocResolution::default()));
    }
}
