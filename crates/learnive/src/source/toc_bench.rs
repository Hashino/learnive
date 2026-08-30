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
                .filter(|(_, t, _)| split_printed_number(t).0.is_some_and(|n| n.contains('.')))
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
                println!(
                    "  !! NO EMBEDDED BOOKMARKS — this book exercises the S27k deduction path"
                );
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

    /// Calls `propose_outline`, retrying through the rate limiting that every
    /// free tier applies. Backoff is deliberately long (60s, 120s, 240s):
    /// Zen's `FreeUsageLimitError` is a shared per-model pool that comes back
    /// in minutes, not seconds, and a fast retry just burns the next slot.
    /// `LEARNIVE_BENCH_PAUSE` (seconds, default 20) additionally paces the
    /// gap between probes.
    async fn propose_with_retry(
        ai: &crate::ai::Ai,
        topic: &str,
        objective: &str,
    ) -> Result<Vec<crate::engine::ProposedOutlineNode>, crate::engine::EngineError> {
        let pause: u64 = std::env::var("LEARNIVE_BENCH_PAUSE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20);
        let mut last = None;
        for (attempt, backoff) in [60u64, 120, 240].into_iter().enumerate() {
            match crate::engine::propose_outline(ai, topic, objective, &[]).await {
                Ok(n) => {
                    tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
                    return Ok(n);
                }
                Err(e) => {
                    let rate_limited = format!("{e:?}").contains("429");
                    println!(
                        "    (attempt {} failed{}: {})",
                        attempt + 1,
                        if rate_limited { ", rate limited" } else { "" },
                        format!("{e:?}").chars().take(90).collect::<String>()
                    );
                    last = Some(e);
                    if !rate_limited {
                        break;
                    }
                    println!("    ...backing off {backoff}s");
                    tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                }
            }
        }
        Err(last.expect("at least one attempt"))
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
            // Free tiers 429 constantly (SPEC §15, measured 2026-08-30: 3 of 6
            // probes died mid-run on the first big-pickle bake-off). Pace and
            // retry, or the bake-off measures rate limits instead of models.
            let nodes = match propose_with_retry(&ai, topic, objective).await {
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
        println!(
            "TOTALS — model={} ",
            std::env::var("LEARNIVE_MODEL_ROBUST").unwrap_or_else(|_| "?".into())
        );
        println!("  chapters proposed against a library book: {total}");
        println!("  ...carrying a model-proposed number:      {numbered_props}");
        println!("  resolved, matcher AS SHIPPED:             {hit_shipped}");
        println!("  resolved, WITH number-split fix:          {hit_split}");
        println!("  proposed works not in the library:        {unmatched_works}");
        // One grep-able line per model, so the bake-off collapses to a table.
        println!(
            "BAKEOFF\t{}\t{total}\t{numbered_props}\t{hit_shipped}\t{hit_split}\t{unmatched_works}",
            std::env::var("LEARNIVE_MODEL_ROBUST").unwrap_or_else(|_| "?".into())
        );
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
        (
            "Stewart",
            "Stewart (CONTROL — 26 real bookmarks to score against)",
        ),
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
            let empty = pdf
                .page_texts
                .iter()
                .filter(|t| t.trim().is_empty())
                .count();
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
            let pages = crate::source::toc::contents_page_chunks(&pdf, range);
            println!(
                "  contents pages: physical {}..={} ({} chars over {} non-empty pages)",
                range.0 + 1,
                range.1 + 1,
                pages.iter().map(|p| p.len()).sum::<usize>(),
                pages.len()
            );

            let entries = match crate::engine::propose_toc(&ai, &pages).await {
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
                println!(
                    "    [{:>7}] {:?} -> p{}",
                    r.number.as_deref().unwrap_or("-"),
                    r.title,
                    r.page
                );
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

/// Diagnostic for the `propose_toc` failures found 2026-08-30: prints the
/// model's RAW response for one contents-page prompt, to separate "the model
/// cannot do the task" from "we cannot read its answer" (the `.env` notes a
/// known gpt-oss failure where the real answer goes to the hidden `reasoning`
/// channel instead of `content`).
#[cfg(test)]
mod raw_probe {
    #[tokio::test]
    #[ignore = "extracts a PDF and hits the real provider; run manually"]
    async fn raw_propose_toc_response() {
        let env_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env");
        crate::load_dotenv(env_path);
        let data_dir =
            std::env::temp_dir().join(format!("learnive-raw-probe-{}", std::process::id()));
        let config = crate::config::AppConfig::load(&data_dir);
        let secret = crate::secret::SecretStore::open(&data_dir);
        let (ai, _policy) = crate::api::build_ai(&config, &secret);

        let dir = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../learnive-data/library"
        ));
        let path = std::fs::read_dir(&dir)
            .expect("library")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.file_name().unwrap().to_string_lossy().contains("Stewart"))
            .expect("stewart");
        let pdf = crate::source::read_pdf(&path).expect("read");
        let range = crate::source::toc::find_contents_pages(&pdf).expect("contents pages");
        let text = crate::source::toc::contents_pages_text(&pdf, range);
        println!("--- prompt input: {} chars ---", text.len());
        println!("{}", &text[..text.len().min(1200)]);

        for (label, tier) in [
            ("FAST tier", crate::ai::Tier::Fast),
            ("ROBUST tier", crate::ai::Tier::Robust),
        ] {
            let messages = crate::engine::prompt::propose_toc(&text);
            match crate::engine::collect(&ai, tier, messages).await {
                Ok(raw) => {
                    println!("\n--- {label}: RAW RESPONSE ({} chars) ---", raw.len());
                    println!("{}", &raw[..raw.len().min(2500)]);
                }
                Err(e) => println!("\n--- {label}: PROVIDER ERROR: {e:?} ---"),
            }
        }
    }
}

/// Regression probe for the K&R text-layer bug (2026-08-30): the file is a
/// scan carrying a real, invisible OCR layer (`3 Tr`, non-embedded WinAnsi
/// Helvetica, hex ASCII) that `pdf_extract` renders as 236 empty pages while
/// poppler reads it fine. Asserts the acervo gate classifies that as
/// `ExtractorFailed` (our bug) and never as `NoText` (go re-buy the book).
#[cfg(test)]
mod kr_text_layer {
    #[test]
    #[ignore = "reads a large library PDF; run manually"]
    fn kr_is_reported_as_extractor_failure_not_as_a_missing_text_layer() {
        let dir = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../learnive-data/library"
        ));
        let Some(path) = std::fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().contains("Kernighan"))
            })
        else {
            eprintln!("K&R not in the library; skipping");
            return;
        };
        {
            let ld = lopdf::Document::load(&path).unwrap();
            let mut ns: Vec<u32> = ld.get_pages().keys().copied().collect();
            ns.sort_unstable();
            for n in ns.into_iter().take(3) {
                let pid = ld.get_pages()[&n];
                let raw = ld.get_page_content(pid);
                match lopdf::content::Content::decode(&raw) {
                    Ok(c) => {
                        let ops: Vec<&str> =
                            c.operations.iter().map(|o| o.operator.as_str()).collect();
                        println!(
                            "page {n}: {} bytes, {} ops, first: {:?}",
                            raw.len(),
                            ops.len(),
                            &ops[..ops.len().min(14)]
                        );
                    }
                    Err(e) => println!("page {n}: DECODE FAILED {e:?} ({} bytes)", raw.len()),
                }
                {
                    let txt = String::from_utf8_lossy(&raw);
                    println!("  raw head: {:?}", &txt[..txt.len().min(160)]);
                    println!(
                        "  has BT: {}  has Tj: {}",
                        txt.contains("BT"),
                        txt.contains("Tj")
                    );
                    // Hypothesis: the leading `%` comment defeats the decoder.
                    let stripped: Vec<u8> = raw
                        .split(|b| *b == b'\n')
                        .filter(|l| !l.starts_with(b"%"))
                        .flat_map(|l| l.iter().copied().chain(std::iter::once(b'\n')))
                        .collect();
                    match lopdf::content::Content::decode(&stripped) {
                        Ok(c) => println!("  AFTER STRIPPING COMMENTS: {} ops", c.operations.len()),
                        Err(e) => println!("  AFTER STRIPPING: still failed {e:?}"),
                    }
                }
            }
        }
        let pdf = crate::source::read_pdf(&path).expect("read");
        let chars = pdf.text.trim().chars().count();
        let empty = pdf
            .page_texts
            .iter()
            .filter(|t| t.trim().is_empty())
            .count();
        println!(
            "K&R: {chars} chars over {} pages ({empty} empty)",
            pdf.page_texts.len()
        );
        assert!(
            chars > 10_000,
            "the OCR text layer must now be readable (was 0 before the \
             comment-stripping patch); got {chars} chars"
        );
        assert!(
            !pdf.text_layer_unreadable,
            "with text extracted, nothing is left to flag"
        );
    }
}

/// Regression: K&R's contents pages must be found despite (a) the OCR
/// heading reading "CONIENTS" and (b) the section numbers extracting onto
/// their own lines, away from the titles. Both defeated the pre-2026-08-30
/// heuristic, and missing the range abandons the whole book.
#[cfg(test)]
mod kr_contents {
    #[test]
    #[ignore = "reads a large library PDF; run manually"]
    fn kr_contents_pages_are_found_despite_ocr_damage() {
        let dir = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../learnive-data/library"
        ));
        let Some(path) = std::fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().contains("Kernighan"))
            })
        else {
            eprintln!("K&R not in the library; skipping");
            return;
        };
        let pdf = crate::source::read_pdf(&path).expect("read");
        let range = crate::source::toc::find_contents_pages(&pdf)
            .expect("K&R's contents pages must be found");
        println!("K&R contents range (0-based): {range:?}");
        // The printed contents occupy physical pages 3..=6 (1-based).
        assert!(range.0 <= 2, "run must start at or before physical page 3");
        assert!(range.1 >= 3, "run must reach physical page 4");
        let text = crate::source::toc::contents_pages_text(&pdf, range);
        assert!(
            text.contains("3.1") && text.to_lowercase().contains("control"),
            "the captured text must actually be the contents"
        );
    }
}

/// Free diagnostic (no API call) for the `resolve_toc` placement rates
/// measured 2026-08-30 (docs §3.5): Stewart lifted to 69% once the offset
/// confirmation read the whole page, but Think Python stayed at 7% and K&R
/// at 8%. The offset prediction can only fire if the *title scan* first
/// anchors enough entries to derive a dominant delta, so the question is
/// what `page_top_matches` actually sees at the top of a body page.
///
/// Prints, it does not assert — the point is to look at the real text
/// rather than keep guessing at it.
#[cfg(test)]
mod page_tops {
    #[test]
    #[ignore = "reads a large library PDF and prints; run manually"]
    fn print_top_of_each_body_page() {
        let fragment =
            std::env::var("LEARNIVE_BENCH_BOOK").unwrap_or_else(|_| "2nd Edition".to_string());
        let dir = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../learnive-data/library"
        ));
        let Some(path) = std::fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().contains(&fragment))
            })
        else {
            eprintln!("no library file matching {fragment:?}; skipping");
            return;
        };
        println!("BOOK: {}", path.file_name().unwrap().to_string_lossy());
        let pdf = crate::source::read_pdf(&path).expect("read");
        let start = crate::source::toc::find_contents_pages(&pdf)
            .map(|(_, end)| end + 1)
            .unwrap_or(0);
        for (i, text) in pdf.page_texts.iter().enumerate().skip(start).take(40) {
            let head: String = text.chars().take(120).collect();
            println!("  p{:<4} {:?}", i + 1, head.replace('\n', " ⏎ "));
        }
    }
}

/// Free diagnostic: what `find_contents_pages` actually captured, per page.
/// 33 KB over 7 pages (Think Python, 2026-08-30) is far more than a printed
/// TOC should be — this shows whether the run over-extended into prose,
/// which would explain the model returning 215 "entries".
#[cfg(test)]
mod contents_dump {
    #[test]
    #[ignore = "reads a large library PDF and prints; run manually"]
    fn print_captured_contents_pages() {
        let fragment =
            std::env::var("LEARNIVE_BENCH_BOOK").unwrap_or_else(|_| "2nd Edition".to_string());
        let dir = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../learnive-data/library"
        ));
        let Some(path) = std::fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().contains(&fragment))
            })
        else {
            eprintln!("no library file matching {fragment:?}; skipping");
            return;
        };
        let pdf = crate::source::read_pdf(&path).expect("read");
        let Some(range) = crate::source::toc::find_contents_pages(&pdf) else {
            println!("no contents range found");
            return;
        };
        println!("range (0-based) = {range:?}");
        for (i, page) in crate::source::toc::contents_page_chunks(&pdf, range)
            .iter()
            .enumerate()
        {
            println!("\n---- captured page {} ({} chars) ----", i + 1, page.len());
            println!("{}", page.chars().take(900).collect::<String>());
        }
    }
}

/// Free, deterministic, no API call: runs `resolve_toc` against Think
/// Python's REAL extracted text with a hand-transcribed entry list taken
/// from its printed contents page (see `contents_dump`). Isolates the
/// resolver from the model entirely — if these fail to place, the bug is in
/// `resolve_toc`, not in what the model returned.
#[cfg(test)]
mod resolver_offline {
    use crate::source::toc::{TocLlmEntry, resolve_toc};

    fn e(number: Option<&str>, title: &str, page: i64) -> TocLlmEntry {
        TocLlmEntry {
            number: number.map(str::to_string),
            title: title.to_string(),
            page: Some(page),
        }
    }

    #[test]
    #[ignore = "reads a large library PDF; run manually"]
    fn think_python_entries_resolve_against_real_text() {
        let dir = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../learnive-data/library"
        ));
        let Some(path) = std::fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().contains("2nd Edition"))
            })
        else {
            eprintln!("Think Python 2e not in the library; skipping");
            return;
        };
        let pdf = crate::source::read_pdf(&path).expect("read");
        let range = crate::source::toc::find_contents_pages(&pdf).expect("contents");

        // Transcribed by hand from the printed contents page.
        let entries = vec![
            e(Some("1"), "The Way of the Program", 1),
            e(None, "What Is a Program?", 1),
            e(None, "Running Python", 2),
            e(None, "The First Program", 3),
            e(None, "Arithmetic Operators", 3),
            e(Some("6"), "Fruitful Functions", 61),
            e(None, "Return Values", 61),
            e(None, "Incremental Development", 62),
            e(None, "Boolean Functions", 65),
            e(Some("10"), "Lists", 107),
            e(None, "A List Is a Sequence", 107),
        ];

        let resolution = resolve_toc(&pdf, &entries, range.1);
        println!("placed {}/{}", resolution.resolved.len(), entries.len());
        for r in &resolution.resolved {
            println!(
                "   OK   [{:>3}] {:?} -> p{}",
                r.number.as_deref().unwrap_or("-"),
                r.title,
                r.page
            );
        }
        for u in &resolution.unresolved {
            println!("   MISS {u:?}");
        }
        // Measured 2026-08-30: all 11 place, including the eight SECTIONS
        // that start mid-page and are only reachable through the offset
        // prediction's whole-page confirmation. This is the line that says
        // the resolver itself is sound — so a low live placement rate is
        // about the entry list it was handed, not about this rule.
        assert_eq!(
            resolution.unresolved,
            Vec::<String>::new(),
            "every hand-transcribed entry must place"
        );
    }
}
