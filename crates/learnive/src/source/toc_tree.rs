//! A book's table of contents as a TREE with exact page ranges (2026-09-27,
//! user decision: "o outline é tão quanto ou mais expressivo que os
//! capítulos do livro … a aplicação apenas divide ainda mais as seções de
//! maior profundidade do livro em nodos, nunca diminui/muda a estrutura
//! representativa"). Before this, every TOC consumer flattened the book's
//! bookmarks into one list of "chapters" — Sipser's Part → Chapter →
//! section tree became 100 sibling rows, and a section's page range was
//! guessed from "the next sibling chapter".
//!
//! Every node carries its own `[page, end_page]` computed from the book's
//! real outline: an entry ends right before the next entry at the same or a
//! shallower depth (pre-order), so a chapter spans all its sections and a
//! section spans exactly its own pages. Front matter (cover, contents,
//! prefaces, indexes…) is dropped from the tree but still bounds its
//! neighbours' ranges. Exercise-type sections are kept (the structure is
//! never reduced) but flagged `default_skip`, which the picker uses as the
//! row's initial "skip" (user decision, same day).

use super::pdf::OutlineEntry;
use super::toc_confirm::{ConfirmedTocEntry, split_printed_number};

/// One entry of a book's table of contents, with its sub-entries.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TocNode {
    /// Printed number split off the title (`"1.1"`, `"2"`) when present.
    pub number: Option<String>,
    pub title: String,
    /// The book's own wording for the entry ("Part 1: Automata and
    /// Languages", "1.1 Finite Automata") — what the reading list shows.
    /// `number`/`title` are the split form used for matching.
    pub label: String,
    /// 1-based physical first page, when known.
    pub page: Option<usize>,
    /// 1-based physical last page (inclusive), when known.
    pub end_page: Option<usize>,
    /// An "Exercises"/"Problems"/"Selected Solutions"-type section: kept,
    /// but offered as "skip" by default.
    pub default_skip: bool,
    pub children: Vec<TocNode>,
}

/// Builds the tree from a PDF's embedded bookmarks. `page_count` bounds the
/// last entry's range.
pub fn tree_from_outline(outline: &[OutlineEntry], page_count: usize) -> Vec<TocNode> {
    // Pre-order with depths over EVERY entry (front matter included, so an
    // index or back-matter entry still bounds the last real chapter).
    let mut flat: Vec<(usize, &OutlineEntry)> = Vec::new();
    fn walk<'a>(es: &'a [OutlineEntry], depth: usize, out: &mut Vec<(usize, &'a OutlineEntry)>) {
        for e in es {
            out.push((depth, e));
            walk(&e.children, depth + 1, out);
        }
    }
    walk(outline, 0, &mut flat);
    let end_of = |i: usize| -> Option<usize> {
        let (depth, e) = flat[i];
        let next = flat[i + 1..]
            .iter()
            .find(|(d, _)| *d <= depth)
            .map(|(_, n)| n.page);
        match next {
            // Two entries on the same page ("Introduction" and "1.1" both on
            // p.55): the earlier one is that one page.
            Some(n) => Some(n.saturating_sub(1).max(e.page)),
            None => (page_count >= e.page).then_some(page_count),
        }
    };
    let mut index = 0usize;
    build(outline, &end_of, &mut index)
}

fn build(
    entries: &[OutlineEntry],
    end_of: &dyn Fn(usize) -> Option<usize>,
    index: &mut usize,
) -> Vec<TocNode> {
    let mut out = Vec::new();
    for e in entries {
        let my = *index;
        *index += 1;
        let children = build(&e.children, end_of, index);
        if is_front_matter(&e.title) {
            // Dropped, but whatever real content it holds is promoted.
            out.extend(children);
            continue;
        }
        let (number, title) = split_printed_number(&e.title);
        out.push(TocNode {
            label: e.title.split_whitespace().collect::<Vec<_>>().join(" "),
            default_skip: is_exercise_section(&title),
            number,
            title,
            page: Some(e.page),
            end_page: end_of(my),
            children,
        });
    }
    out
}

/// Builds a one-level tree from a flat list (a confirmed/deduced TOC, or
/// chapter openers derived from the text): each entry ends before the next.
pub fn tree_from_flat(entries: &[ConfirmedTocEntry], page_count: usize) -> Vec<TocNode> {
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let next = entries[i + 1..].iter().find_map(|n| n.page);
            let end_page = match (e.page, next) {
                (Some(p), Some(n)) => Some(n.saturating_sub(1).max(p)),
                (Some(p), None) => (page_count >= p).then_some(page_count),
                (None, _) => None,
            };
            TocNode {
                number: e.number.clone(),
                title: e.title.clone(),
                label: match &e.number {
                    Some(n) => format!("{n} {}", e.title),
                    None => e.title.clone(),
                },
                page: e.page,
                end_page,
                default_skip: is_exercise_section(&e.title),
                children: Vec::new(),
            }
        })
        .collect()
}

/// Every node, depth-first, parents before children.
pub fn flatten(tree: &[TocNode]) -> Vec<&TocNode> {
    let mut out = Vec::new();
    fn walk<'a>(ns: &'a [TocNode], out: &mut Vec<&'a TocNode>) {
        for n in ns {
            out.push(n);
            walk(&n.children, out);
        }
    }
    walk(tree, &mut out);
    out
}

/// Front matter a real book carries in its outline but nobody studies as a
/// chapter (moved here from `api::reading` with the tree). Zero-token and
/// deliberately conservative: an exact match on the normalized title, or a
/// prefix from a short lead list — a miss costs one noisy row, a wrong drop
/// costs a real chapter. Observed live: Axler's "About the Author",
/// "Contents", "Acknowledgments", "Photo Credits", "Symbol Index";
/// Stewart's "Front matter", "To the student"; Sipser's "Cover",
/// "Statement", "Dedication", "Preface to the … Edition".
pub fn is_front_matter(title: &str) -> bool {
    let t = title.trim().to_lowercase();
    let t = t.trim_end_matches(':');
    matches!(
        t,
        "contents"
            | "table of contents"
            | "acknowledgments"
            | "acknowledgements"
            | "index"
            | "cover"
            | "title page"
            | "copyright"
            | "statement"
            | "dedication"
            | "front matter"
            | "to the student"
            | "answers"
            | "answers to odd-numbered exercises"
            | "answers to selected exercises"
            | "answer key"
            | "photo credits"
            | "credits"
            | "symbol index"
            | "subject index"
            | "name index"
    ) || t.starts_with("preface")
        || t.starts_with("foreword")
        || t.starts_with("about the")
        || t.starts_with("about this")
        || t.starts_with("colophon")
        || t.starts_with("index of")
}

/// A section that holds exercises/problems/solutions rather than exposition
/// — kept in the tree (the book's structure is never reduced), offered as
/// "skip" by default. Matches the normalized title exactly, with or without
/// a printed number already split off.
pub fn is_exercise_section(title: &str) -> bool {
    let t = title.trim().to_lowercase();
    let t = t.trim_end_matches([':', '.']);
    matches!(
        t,
        "exercises"
            | "problems"
            | "exercises and problems"
            | "problems and exercises"
            | "selected solutions"
            | "solutions"
            | "solutions to selected exercises"
            | "review exercises"
            | "review questions"
            | "problem set"
            | "problem sets"
            | "exercícios"
            | "problemas"
            | "soluções"
            | "exercícios resolvidos"
            | "lista de exercícios"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(title: &str, page: usize, children: Vec<OutlineEntry>) -> OutlineEntry {
        OutlineEntry {
            title: title.to_string(),
            page,
            children,
        }
    }

    /// Sipser's real shape: front matter, a chapter with sections, a Part
    /// holding chapters holding sections.
    fn sipser() -> Vec<OutlineEntry> {
        vec![
            e("Cover", 1, vec![]),
            e("Contents", 7, vec![]),
            e("Preface to the First Edition", 13, vec![]),
            e(
                "Ch 0: Introduction",
                25,
                vec![
                    e("0.1 Automata, Computability, and Complexity", 25, vec![]),
                    e("0.2 Mathematical Notions and Terminology", 27, vec![]),
                    e("Exercises", 49, vec![]),
                ],
            ),
            e(
                "Part 1: Automata and Languages",
                53,
                vec![
                    e(
                        "Ch 1: Regular Languages",
                        55,
                        vec![
                            e("Introduction", 55, vec![]),
                            e("1.1 Finite Automata", 55, vec![]),
                            e("1.2 Nondeterminism", 71, vec![]),
                            e("Selected Solutions", 118, vec![]),
                        ],
                    ),
                    e("Ch 2: Context-Free Languages", 125, vec![]),
                ],
            ),
            e("Index", 480, vec![]),
        ]
    }

    #[test]
    fn keeps_the_hierarchy_and_drops_front_matter() {
        let tree = tree_from_outline(&sipser(), 500);
        let titles: Vec<&str> = tree.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles.len(), 2, "{titles:?}");
        assert!(titles[0].contains("Introduction"));
        assert!(titles[1].contains("Automata and Languages"));
        let part = &tree[1];
        assert_eq!(part.children.len(), 2);
        assert_eq!(part.children[0].children.len(), 4, "sections under Ch 1");
    }

    #[test]
    fn ranges_nest_exactly() {
        let tree = tree_from_outline(&sipser(), 500);
        let part = &tree[1];
        let ch1 = &part.children[0];
        assert_eq!(
            (part.page, part.end_page),
            (Some(53), Some(479)),
            "part ends before Index"
        );
        assert_eq!(
            (ch1.page, ch1.end_page),
            (Some(55), Some(124)),
            "chapter spans its sections"
        );
        let secs = &ch1.children;
        assert_eq!(
            (secs[0].page, secs[0].end_page),
            (Some(55), Some(55)),
            "same-page intro"
        );
        assert_eq!((secs[1].page, secs[1].end_page), (Some(55), Some(70)));
        assert_eq!((secs[2].page, secs[2].end_page), (Some(71), Some(117)));
        assert_eq!(
            (secs[3].page, secs[3].end_page),
            (Some(118), Some(124)),
            "last section ends where the next chapter starts"
        );
        let ch0 = &tree[0];
        assert_eq!(
            ch0.end_page,
            Some(52),
            "front matter/next part still bound ranges"
        );
    }

    #[test]
    fn exercise_sections_are_kept_but_default_to_skip() {
        let tree = tree_from_outline(&sipser(), 500);
        let ch0 = &tree[0];
        let ex = ch0.children.last().unwrap();
        assert_eq!(ex.title, "Exercises");
        assert!(ex.default_skip);
        assert!(!ch0.children[0].default_skip);
        let sol = tree[1].children[0].children.last().unwrap();
        assert!(sol.default_skip, "Selected Solutions");
    }

    #[test]
    fn printed_numbers_are_split_off() {
        let tree = tree_from_outline(&sipser(), 500);
        let s = &tree[1].children[0].children[1];
        assert_eq!(s.number.as_deref(), Some("1.1"));
        assert_eq!(s.title, "Finite Automata");
    }

    #[test]
    fn flat_entries_end_before_the_next() {
        let flat = vec![
            ConfirmedTocEntry {
                title: "One".into(),
                number: Some("1".into()),
                page: Some(10),
                inferred: true,
            },
            ConfirmedTocEntry {
                title: "Two".into(),
                number: Some("2".into()),
                page: Some(30),
                inferred: true,
            },
        ];
        let tree = tree_from_flat(&flat, 50);
        assert_eq!(tree[0].end_page, Some(29));
        assert_eq!(tree[1].end_page, Some(50));
    }
}
