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
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TocLlmEntry {
    pub title: String,
    #[serde(default)]
    pub page: Option<i64>,
}

/// One chapter [`resolve_toc`] placed on a real physical page.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTocEntry {
    pub title: String,
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
        let head: String = page.chars().take(60).collect::<String>().to_lowercase();
        let titled = head.contains("contents")
            || head.contains("sumário")
            || head.contains("sumario")
            || head.contains("índice")
            || head.contains("indice");
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
            l.split_whitespace().next_back().is_some_and(|tok| {
                !tok.is_empty() && tok.len() <= 4 && tok.chars().all(|c| c.is_ascii_digit())
            })
        })
        .count();
    numbered as f64 / lines.len() as f64
}

/// Joins the text of a contents-page run ([`find_contents_pages`]'s output)
/// into the single string `engine::propose_toc` feeds the model — a few KB
/// of pre-textual, never the book's body (PLAN.md's cost argument for why
/// this call is affordable at all).
pub fn contents_pages_text(pdf: &PdfDocument, range: (usize, usize)) -> String {
    let (start, end) = range;
    pdf.page_texts
        .get(start..=end.min(pdf.page_texts.len().saturating_sub(1)))
        .map(|pages| pages.join("\n"))
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
///    confirms it (never blind arithmetic alone).
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
            if !needle.is_empty() && page_top_matches(pdf, predicted, &needle) {
                *placement = Some(predicted);
            }
        }
    }

    let mut resolution = TocResolution::default();
    for (entry, placement) in entries.iter().zip(placements) {
        match placement {
            Some(page) => resolution.resolved.push(ResolvedTocEntry {
                title: entry.title.clone(),
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
            text: page_texts.join("\n"),
            page_texts,
            outline: Vec::new(),
            pages: PageMap::default(),
        }
    }

    fn entry(title: &str, page: Option<i64>) -> TocLlmEntry {
        TocLlmEntry {
            title: title.to_string(),
            page,
        }
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
                page: 2
            }
        );
        assert_eq!(
            resolution.resolved[1],
            ResolvedTocEntry {
                title: "Chapter One".into(),
                page: 4
            }
        );
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
                    page: 3
                },
                ResolvedTocEntry {
                    title: "Appendix".into(),
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
                    page: 3
                },
                ResolvedTocEntry {
                    title: "Chapter Two".into(),
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
                page: 1,
            }],
            unresolved: vec!["B".into(), "C".into(), "D".into()],
        };
        assert!(!is_resolution_acceptable(&mostly_failed));

        let mostly_resolved = TocResolution {
            resolved: vec![
                ResolvedTocEntry {
                    title: "A".into(),
                    page: 1,
                },
                ResolvedTocEntry {
                    title: "B".into(),
                    page: 2,
                },
                ResolvedTocEntry {
                    title: "C".into(),
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
