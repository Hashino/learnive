//! **Measurement harness, not production code** (S27g, 2026-08-30).
//!
//! Answers a question the S27g live run could only guess at from `n=3` in a
//! single book: how often does a model-proposed chapter actually resolve
//! onto a real book's table of contents, which tier of the cascade resolves
//! it, and what would the proposed *split the printed number out of the
//! bookmark title* fix change? Every conclusion drawn from this lands in
//! PLAN.md's S27g section; nothing here ships.
//!
//! Two independent `#[ignore]`d entry points:
//! - [`tests::toc_shape_of_every_library_book`] — free, local, no API call.
//!   Dumps each library PDF's embedded outline and classifies its numbering
//!   style/depth. Reads bookmarks via `lopdf` ONLY (`read_outline_for_test`),
//!   deliberately skipping `read_pdf`'s full-text extraction, which is the
//!   slow part (222s for Stewart alone).
//! - [`tests::live_match_rate_across_the_library`] — spends real API budget:
//!   one `propose_outline` call per probe, then matches every proposed
//!   chapter against the real TOC of whichever library book the proposed
//!   work corresponds to, scoring the current matcher against the
//!   number-split variant.

#[cfg(test)]
mod tests {
    use crate::source::matching::normalize;
    use crate::source::{ConfirmedTocEntry, OutlineEntry, match_chapter};

    fn library_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../learnive-data/library"
        ))
    }

    fn library_pdfs() -> Vec<(String, std::path::PathBuf)> {
        let mut out: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(library_dir())
            .expect("library dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("pdf"))
            .map(|p| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    p.clone(),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Splits a leading printed chapter number off a TOC title — the fix
    /// under evaluation. `"4 - Applications of differentiation "` becomes
    /// `(Some("4"), "Applications of differentiation")`. Handles a bare
    /// `"4.10 Recursion"`, a `"Chapter 4: ..."` prefix, and the assorted
    /// dash/colon/dot separators real books use. Returns `(None, trimmed)`
    /// when there's no leading number to split.
    fn split_printed_number(title: &str) -> (Option<String>, String) {
        // Real books put NON-BREAKING spaces inside the numbering (Think
        // Python's bookmarks are literally "Chapter\u{a0}1.\u{a0}The Way of
        // the Program") — measured 2026-08-30, and it silently defeated a
        // first version of this splitter on all 270 of that book's entries.
        // Fold every unicode space to a plain one before anything else.
        let folded: String = title
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .collect();
        let t = folded.trim();
        let lower = t.to_lowercase();
        let rest = if let Some(stripped) = lower.strip_prefix("chapter ") {
            &t[t.len() - stripped.len()..]
        } else if let Some(stripped) = lower.strip_prefix("part ") {
            &t[t.len() - stripped.len()..]
        } else {
            t
        };

        let digits_end = rest
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_digit() || *c == '.')
            .map(|(i, c)| i + c.len_utf8())
            .last()
            .unwrap_or(0);
        if digits_end == 0 {
            return (None, rest.trim().to_string());
        }
        let number = rest[..digits_end].trim_matches('.').to_string();
        if number.is_empty() {
            return (None, rest.trim().to_string());
        }
        let tail = rest[digits_end..]
            .trim_start_matches([' ', '\t', '-', '\u{2013}', '\u{2014}', ':', '.', ')'])
            .trim();
        // A "number" that ate the whole title (a TOC entry that is literally
        // just "1") leaves nothing to match on by name — keep the original.
        if tail.is_empty() {
            return (None, rest.trim().to_string());
        }
        (Some(number), tail.to_string())
    }

    fn flatten(entries: &[OutlineEntry], depth: usize, out: &mut Vec<(usize, String, usize)>) {
        for e in entries {
            out.push((depth, e.title.clone(), e.page));
            flatten(&e.children, depth + 1, out);
        }
    }

    /// The current, shipped shape: number is always `None` on the
    /// embedded-bookmark path (nothing populates it), title is raw.
    fn as_shipped(flat: &[(usize, String, usize)]) -> Vec<ConfirmedTocEntry> {
        flat.iter()
            .map(|(_, title, page)| ConfirmedTocEntry {
                title: title.clone(),
                number: None,
                page: Some(*page),
                inferred: true,
            })
            .collect()
    }

    /// The proposed fix: printed number split out of the title into
    /// `number`, title cleaned.
    fn as_split(flat: &[(usize, String, usize)]) -> Vec<ConfirmedTocEntry> {
        flat.iter()
            .map(|(_, title, page)| {
                let (number, clean) = split_printed_number(title);
                ConfirmedTocEntry {
                    title: clean,
                    number,
                    page: Some(*page),
                    inferred: true,
                }
            })
            .collect()
    }

    /// Free, local, no API call. Run with:
    /// `cargo test -p learnive --bin learnive source::toc_bench::tests::toc_shape_of_every_library_book -- --ignored --nocapture`
    #[test]
    #[ignore = "reads every library PDF; diagnostic only"]
    fn toc_shape_of_every_library_book() {
        for (name, path) in library_pdfs() {
            let outline = crate::source::pdf::read_outline_for_test(&path);
            let mut flat = Vec::new();
            flatten(&outline, 0, &mut flat);

            let max_depth = flat.iter().map(|(d, _, _)| *d).max().unwrap_or(0);
            let numbered = flat
                .iter()
                .filter(|(_, t, _)| split_printed_number(t).0.is_some())
                .count();
            let deep_numbered = flat
                .iter()
                .filter(|(_, t, _)| {
                    split_printed_number(t)
                        .0
                        .is_some_and(|n| n.contains('.'))
                })
                .count();

            println!("\n================================================================");
            println!("BOOK: {name}");
            println!(
                "  bookmarks: {}   max nesting depth: {}   numbered titles: {}/{}   with sub-numbering (N.M): {}",
                flat.len(),
                max_depth,
                numbered,
                flat.len(),
                deep_numbered
            );
            if flat.is_empty() {
                println!("  !! NO EMBEDDED BOOKMARKS — this book exercises the S27k deduction path");
                continue;
            }
            println!("  --- first 30 entries (depth | raw title | split) ---");
            for (depth, title, page) in flat.iter().take(30) {
                let (num, clean) = split_printed_number(title);
                println!(
                    "  {}{:?} p{}  ->  number={:?} title={:?}",
                    "  ".repeat(*depth),
                    title.trim_end(),
                    page,
                    num,
                    clean
                );
            }
        }
    }

    /// One probe per book: a topic/objective chosen so the target book is
    /// overwhelmingly likely to appear in the proposed reading list.
    /// `(label, filename fragment identifying the library book, topic, objective)`.
    ///
    /// The filename fragment is explicit rather than derived by matching the
    /// model's proposed work title against the PDF's filename — a first
    /// version did the latter and silently scored SICP as "not in library"
    /// because the file is named
    /// `Harold_Abelson_Gerald_Sussman_Julie_Sussman-SICP-EN.pdf`, which
    /// shares no containment with "Structure and Interpretation of Computer
    /// Programs". That's a harness artifact, not a finding about the matcher.
    fn probes() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
        vec![
            (
                "The C Programming Language",
                "Kernighan",
                "C programming",
                "Understand C well enough to write and reason about recursive functions, \
                 pointers, and structs.",
            ),
            (
                "Structure and Interpretation of Computer Programs",
                "SICP",
                "Structure and Interpretation of Computer Programs",
                "Understand higher-order procedures, data abstraction, and how an interpreter \
                 for a language can be written in that same language.",
            ),
            (
                "Think Python (1st ed, 2012)",
                "Think Python (2012",
                "Python programming for beginners",
                "Learn Python well enough to write functions, use dictionaries and lists, and \
                 read and write files.",
            ),
            (
                "Think Python (2nd ed, 2015)",
                "2nd Edition",
                "Python programming for beginners",
                "Learn Python well enough to write functions, use dictionaries and lists, and \
                 read and write files.",
            ),
            (
                "Pro Git",
                "Pro Git",
                "Git version control",
                "Understand Git well enough to branch, merge, rebase, and reason about how \
                 commits and refs are stored.",
            ),
            (
                "Calculus: Early Transcendentals",
                "Stewart",
                "Calculus: Early Transcendentals",
                "Understand derivatives, integrals, and the fundamental theorem of calculus \
                 well enough to apply them to related-rates and optimization problems.",
            ),
        ]
    }

    /// Spends real API budget: one `propose_outline` per probe against the
    /// configured provider. Run with:
    /// `cargo test -p learnive --bin learnive source::toc_bench::tests::live_match_rate_across_the_library -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "hits the real configured AI provider once per probe; run manually"]
    async fn live_match_rate_across_the_library() {
        let env_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env");
        crate::load_dotenv(env_path);
        let data_dir =
            std::env::temp_dir().join(format!("learnive-toc-bench-{}", std::process::id()));
        let config = crate::config::AppConfig::load(&data_dir);
        let secret = crate::secret::SecretStore::open(&data_dir);
        let (ai, _policy) = crate::api::build_ai(&config, &secret);

        // Preload every library book's TOC once (local, no API).
        let mut tocs: Vec<(String, Vec<(usize, String, usize)>)> = Vec::new();
        for (name, path) in library_pdfs() {
            let outline = crate::source::pdf::read_outline_for_test(&path);
            let mut flat = Vec::new();
            flatten(&outline, 0, &mut flat);
            tocs.push((name, flat));
        }

        let (mut total, mut hit_shipped, mut hit_split, mut numbered_props) = (0, 0, 0, 0);
        let mut unmatched_works = 0;

        for (label, file_fragment, topic, objective) in probes() {
            println!("\n================================================================");
            println!("PROBE: {label}   (library file matching {file_fragment:?})");
            let target = tocs.iter().find(|(name, _)| name.contains(file_fragment));
            match target {
                Some((name, flat)) if flat.is_empty() => println!(
                    "  NOTE: {name} has ZERO bookmarks — nothing to match against; \
                     this book can only be served by the S27k deduction path."
                ),
                Some((name, flat)) => {
                    println!("  target TOC: {} ({} entries)", name, flat.len())
                }
                None => println!("  !! no library file matches {file_fragment:?}"),
            }
            let nodes = match crate::engine::propose_outline(&ai, topic, objective, &[]).await {
                Ok(n) => n,
                Err(e) => {
                    println!("  !! propose_outline FAILED: {e:?}");
                    continue;
                }
            };
            for work in &nodes {
                let chapters: Vec<_> = work
                    .children
                    .iter()
                    .filter(|c| c.item_type == crate::engine::OutlineItemType::Chapter)
                    .collect();
                // Score a proposed work against the probe's declared target
                // book when the two plausibly refer to the same work.
                let w = normalize(&work.title);
                let probe_title = normalize(topic);
                let same_work = !w.is_empty()
                    && (probe_title.contains(&w)
                        || w.contains(&probe_title)
                        || normalize(label).contains(&w)
                        || w.contains(&normalize(label)));
                let Some((name, flat)) = target.filter(|_| same_work) else {
                    if !chapters.is_empty() {
                        unmatched_works += 1;
                    }
                    println!(
                        "  proposed work (not the probe target, unscored): {:?} ({} chapters)",
                        work.title,
                        chapters.len()
                    );
                    continue;
                };
                if flat.is_empty() {
                    println!(
                        "  proposed work {:?} -> {} but it has NO TOC; {} chapters unscorable",
                        work.title,
                        name,
                        chapters.len()
                    );
                    continue;
                }
                println!("  proposed work {:?}  ->  library: {}", work.title, name);
                let shipped = as_shipped(flat);
                let split = as_split(flat);
                for c in chapters {
                    total += 1;
                    if c.chapter_number.is_some() {
                        numbered_props += 1;
                    }
                    let a = match_chapter(&shipped, c.chapter_number.as_deref(), &c.title);
                    let b = match_chapter(&split, c.chapter_number.as_deref(), &c.title);
                    if a.is_some() {
                        hit_shipped += 1;
                    }
                    if b.is_some() {
                        hit_split += 1;
                    }
                    println!(
                        "    [{:>6}] {:?}\n        shipped -> {:?}\n        split   -> {:?}",
                        c.chapter_number.as_deref().unwrap_or("-"),
                        c.title,
                        a.map(|e| (e.title.as_str(), e.page)),
                        b.map(|e| (e.title.as_str(), e.page)),
                    );
                }
            }
        }

        println!("\n================================================================");
        println!("TOTALS across all probes");
        println!("  chapters proposed against a library book: {total}");
        println!("  ...carrying a model-proposed number:      {numbered_props}");
        println!("  resolved, matcher AS SHIPPED:             {hit_shipped}");
        println!("  resolved, WITH number-split fix:          {hit_split}");
        println!("  proposed works not in the library:        {unmatched_works}");
    }
}

/// S27k deduction-path benchmark — the ~33%-of-the-corpus path that had no
/// measurement at all until 2026-08-30 (see
/// `docs/S27g-chapter-matching-measurements.md` §3).
#[cfg(test)]
mod deduction {
    use crate::source::toc::{
        contents_pages_text, find_contents_pages, is_resolution_acceptable, resolve_toc,
    };

    /// Books to run the deduction cascade against. The two bookmark-less
    /// ones are the real target; Stewart is a **control** — its 26 embedded
    /// bookmarks are known-good ground truth, so deduction output can be
    /// scored against them rather than merely described.
    const TARGETS: [(&str, &str); 3] = [
        ("Kernighan", "K&R (1978 scan, OCR noise, no bookmarks)"),
        ("2nd Edition", "Think Python 2e (no bookmarks)"),
        ("Stewart", "Stewart (CONTROL — 26 real bookmarks to score against)"),
    ];

    /// Spends real API budget: one `propose_toc` (fast tier) per book. Run:
    /// `cargo test -p learnive --bin learnive source::toc_bench::deduction::live_deduction_cascade -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "full-text-extracts several large PDFs and hits the real provider; run manually"]
    async fn live_deduction_cascade() {
        let env_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env");
        crate::load_dotenv(env_path);
        let data_dir =
            std::env::temp_dir().join(format!("learnive-dedup-bench-{}", std::process::id()));
        let config = crate::config::AppConfig::load(&data_dir);
        let secret = crate::secret::SecretStore::open(&data_dir);
        let (ai, _policy) = crate::api::build_ai(&config, &secret);

        let dir = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../learnive-data/library"
        ));
        for (fragment, label) in TARGETS {
            println!("\n================================================================");
            println!("DEDUCTION: {label}");
            let Some(path) = std::fs::read_dir(&dir)
                .expect("library")
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| p.file_name().unwrap().to_string_lossy().contains(fragment))
            else {
                println!("  !! no library file matching {fragment:?}");
                continue;
            };

            let t0 = std::time::Instant::now();
            let pdf = match crate::source::read_pdf(&path) {
                Ok(p) => p,
                Err(e) => {
                    println!("  !! read_pdf failed: {e}");
                    continue;
                }
            };
            let empty = pdf.page_texts.iter().filter(|t| t.trim().is_empty()).count();
            println!(
                "  extracted {} pages in {:?}  ({} empty / no text layer)",
                pdf.page_texts.len(),
                t0.elapsed(),
                empty
            );

            let Some(range) = find_contents_pages(&pdf) else {
                println!("  !! find_contents_pages found NOTHING — cascade cannot start here");
                continue;
            };
            let text = contents_pages_text(&pdf, range);
            println!(
                "  contents pages: physical {}..={} ({} chars of text)",
                range.0 + 1,
                range.1 + 1,
                text.len()
            );

            let entries = match crate::engine::propose_toc(&ai, &text).await {
                Ok(e) => e,
                Err(e) => {
                    println!("  !! propose_toc FAILED: {e:?}");
                    continue;
                }
            };
            let numbered = entries.iter().filter(|e| e.number.is_some()).count();
            let sub_numbered = entries
                .iter()
                .filter(|e| e.number.as_deref().is_some_and(|n| n.contains('.')))
                .count();
            println!(
                "  propose_toc returned {} entries: {} numbered, {} SUB-numbered (N.M)",
                entries.len(),
                numbered,
                sub_numbered
            );

            let resolution = resolve_toc(&pdf, &entries, range.1);
            let total = resolution.resolved.len() + resolution.unresolved.len();
            println!(
                "  resolve_toc placed {}/{}  ({:.0}%)   acceptable={}",
                resolution.resolved.len(),
                total,
                100.0 * resolution.resolved.len() as f64 / total.max(1) as f64,
                is_resolution_acceptable(&resolution)
            );
            for r in resolution.resolved.iter().take(25) {
                println!("    [{:>7}] {:?} -> p{}", r.number.as_deref().unwrap_or("-"), r.title, r.page);
            }
            if !resolution.unresolved.is_empty() {
                println!("    UNRESOLVED ({}):", resolution.unresolved.len());
                for u in resolution.unresolved.iter().take(15) {
                    println!("      {u:?}");
                }
            }

            // Control scoring: compare against the book's real bookmarks.
            let truth = crate::source::pdf::read_outline_for_test(&path);
            if !truth.is_empty() {
                let mut exact = 0usize;
                let mut near = 0usize;
                for r in &resolution.resolved {
                    let n = crate::source::matching::normalize(&r.title);
                    if let Some(hit) = truth.iter().find(|t| {
                        let tn = crate::source::matching::normalize(&t.title);
                        !n.is_empty() && (tn.contains(&n) || n.contains(&tn))
                    }) {
                        if hit.page == r.page {
                            exact += 1;
                        } else if hit.page.abs_diff(r.page) <= 2 {
                            near += 1;
                        } else {
                            println!(
                                "    PAGE MISMATCH: {:?} deduced p{} but bookmark says p{}",
                                r.title, r.page, hit.page
                            );
                        }
                    }
                }
                println!(
                    "  CONTROL vs real bookmarks: {exact} exact page, {near} within 2 pages, \
                     out of {} resolved",
                    resolution.resolved.len()
                );
            }
        }
    }
}
