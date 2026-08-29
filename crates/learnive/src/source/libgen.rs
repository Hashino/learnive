//! LibGen (Library Genesis) acquisition backend (§11.1).
//!
//! LibGen mirrors don't expose one stable JSON endpoint that carries both the
//! book title and the download `md5` across every mirror generation, so the
//! search scrapes the results **HTML table**:
//!
//! ```text
//! GET {mirror}/index.php?req=<q>&res=25&view=detailed&phrase=1&column=title&lg_topic=libgen
//! ```
//!
//! `q` is expected to be a real book/article TITLE, not a topic phrase
//! (`column=title` — changed 2026-08-29 from the all-fields `def` column,
//! which is what let a topic phrase like "discrete math" match and download
//! an unrelated Android/automata paper; see `engine::propose_source_title`).
//!
//! Each result row links to `/ads.php?md5=<md5>`; `md5` plus the row's title
//! and author are scraped out. A few generations (notably libgen.is) instead
//! answer the `out=json` query — that path is kept as a fallback.
//!
//! Download is a two-hop flow: `/ads.php?md5=` returns an HTML page containing
//! the real `/get.php?md5=<md5>&key=<key>` link (the `key` is a per-request
//! token); fetching that 302-redirects to the file host and yields the PDF.
//! We verify the bytes are a real `%PDF` before normalizing.
//!
//! Domains rotate frequently, so the backend accepts one or more
//! comma-separated mirror roots and tries each (and each script name) in order
//! until one answers.

use std::time::Duration;

use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;
use url::Url;

use super::{FetchedSource, Origin, SearchHit, SourceError, SourceKind, fetched_from_pdf};

/// Browser-like UA: libgen.li answers real content to ordinary browser UAs and
/// may serve a default/blocked page to unknown agents.
const CHROME_UA: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// A LibGen mirror (or a list of candidate mirrors) to try in order.
pub struct LibGenSource {
    client: reqwest::Client,
    bases: Vec<String>,
}

impl LibGenSource {
    /// `base_url` is one or more mirror roots, comma-separated (e.g.
    /// `"https://libgen.li,https://libgen.is"`). Each is tried in order until
    /// one answers, and within a mirror both `search.php` and `index.php` are
    /// probed (different generations expose search under different names). A
    /// trailing slash is tolerated on every entry.
    pub fn new(base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(CHROME_UA)
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        let bases = base_url
            .split(',')
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Self { client, bases }
    }

    fn net(e: impl std::fmt::Display) -> SourceError {
        SourceError::Network(e.to_string())
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>, SourceError> {
        let q = query.trim();
        if q.is_empty() {
            return Err(SourceError::NoResult);
        }
        let mut last_err: Option<SourceError> = None;
        for base in &self.bases {
            let Ok(base_url) = Url::parse(base) else {
                continue;
            };
            // libgen.is/rs/st use `search.php`; libgen.li/im use `index.php`.
            for script in ["index.php", "search.php"] {
                match self.search_on(&base_url, script, q).await {
                    Ok(hits) if !hits.is_empty() => return Ok(hits),
                    Ok(_) => continue,
                    Err(e) => {
                        last_err = Some(e);
                        continue;
                    }
                }
            }
        }
        Err(last_err.unwrap_or(SourceError::NoResult))
    }

    async fn search_on(
        &self,
        base: &Url,
        script: &str,
        q: &str,
    ) -> Result<Vec<SearchHit>, SourceError> {
        let url = base.join(script).map_err(Self::net)?;
        // 1) HTML results table (libgen.li / libgen.im and friends).
        if let Ok(resp) = self
            .client
            .get(url.clone())
            .query(&[
                ("req", q),
                ("res", "25"),
                ("view", "detailed"),
                ("phrase", "1"),
                ("column", "title"),
                ("lg_topic", "libgen"),
            ])
            .send()
            .await
            && resp.status().is_success()
                && let Ok(bytes) = resp.bytes().await {
                    let hits = parse_html_results(base, &bytes);
                    if !hits.is_empty() {
                        return Ok(hits);
                    }
                }
        // 2) `out=json` endpoint (libgen.is-style mirrors).
        if let Ok(resp) = self
            .client
            .get(url)
            .query(&[
                ("req", q),
                ("res", "25"),
                ("view", "detailed"),
                ("phrase", "1"),
                ("out", "json"),
                ("column", "title"),
                ("lg_topic", "libgen"),
            ])
            .send()
            .await
            && resp.status().is_success()
                && let Ok(bytes) = resp.bytes().await
                    && let Ok(books) = serde_json::from_slice::<Vec<LibGenBook>>(&bytes) {
                        let hits = books_to_hits(base, &books);
                        if !hits.is_empty() {
                            return Ok(hits);
                        }
                    }
        Err(SourceError::NoResult)
    }

    /// Resolve a search hit to the final PDF URL without downloading the file:
    /// follow `ads.php?md5=` → `get.php?md5=&key=`, returning the URL that
    /// actually serves the PDF bytes.
    async fn resolve_pdf(&self, hit: &SearchHit) -> Result<Url, SourceError> {
        let base_url = Url::parse(&hit.handle).ok();
        let resp = self
            .client
            .get(&hit.handle)
            .send()
            .await
            .map_err(Self::net)?;
        if !resp.status().is_success() {
            return Err(SourceError::Network(format!(
                "libgen ads.php HTTP {}",
                resp.status()
            )));
        }
        let bytes = resp.bytes().await.map_err(Self::net)?;
        if bytes.starts_with(b"%PDF") {
            return Url::parse(&hit.handle).map_err(Self::net);
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| SourceError::Normalize("ads.php not HTML".into()))?;
        let dl = extract_get_link(text, base_url.as_ref())
            .ok_or_else(|| SourceError::Normalize("no get.php link in ads.php".into()))?;
        // Some intermediaries nest another HTML page with the real link; detect
        // that via content-type on a HEAD and descend one level if needed.
        if let Ok(head) = self.client.head(dl.clone()).send().await
            && let Some(ct) = head.headers().get(reqwest::header::CONTENT_TYPE)
                && ct.to_str().unwrap_or("").contains("html")
                    && let Ok(r) = self.client.get(dl.clone()).send().await
                        && let Ok(b) = r.bytes().await
                            && let Ok(t) = std::str::from_utf8(&b)
                                && let Some(dl2) = extract_get_link(t, Some(&dl)) {
                                    return Ok(dl2);
                                }
        Ok(dl)
    }

    pub async fn fetch(&self, hit: &SearchHit) -> Result<FetchedSource, SourceError> {
        let pdf_url = self.resolve_pdf(hit).await?;
        // LibGen mirrors intermittently reset the connection mid-download
        // ("error decoding response body"); retry a few times before giving
        // up so a real textbook actually lands instead of silently failing.
        // Definitive failures (non-success status, empty/non-PDF body) are
        // returned immediately and not retried.
        let mut last_err: Option<reqwest::Error> = None;
        for attempt in 1..=3 {
            match self.client.get(pdf_url.clone()).send().await {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        return Err(SourceError::Network(format!(
                            "libgen download HTTP {}",
                            resp.status()
                        )));
                    }
                    match resp.bytes().await {
                        Ok(bytes) => {
                            if bytes.is_empty() {
                                return Err(SourceError::Normalize(
                                    "libgen returned an empty file".into(),
                                ));
                            }
                            if !bytes.starts_with(b"%PDF") {
                                return Err(SourceError::Normalize(
                                    "libgen download was not a PDF (mirror likely returned an error page)"
                                        .into(),
                                ));
                            }
                            return fetched_from_pdf(hit, &bytes);
                        }
                        Err(e) => last_err = Some(e),
                    }
                }
                Err(e) => last_err = Some(e),
            }
            tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
        }
        Err(Self::net(
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "libgen download failed after retries".into()),
        ))
    }
}

/// Extract the 32-hex `md5` from a LibGen href (`ads.php?md5=…`).
fn extract_md5(href: &str) -> Option<String> {
    let idx = href.find("md5=")?;
    let rest = &href[idx + 4..];
    // The md5 may be the last segment of the href (no trailing char), so treat
    // "no more hex" as end-of-string rather than a parse failure.
    let end = rest
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(rest.len());
    let hex = &rest[..end];
    if hex.len() == 32 {
        Some(hex.to_ascii_lowercase())
    } else {
        None
    }
}

/// Find the first `get.php?md5=…` link in an HTML page, resolving relative
/// hrefs against `base`. We match on the raw `href` (rather than a CSS
/// attribute selector) for robustness across scraper's selector grammar.
fn extract_get_link(html: &str, base: Option<&Url>) -> Option<Url> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("a").ok()?;
    for a in doc.select(&sel) {
        let href = a.value().attr("href")?;
        if !href.contains("get.php?md5=") {
            continue;
        }
        let url = if href.starts_with("http://") || href.starts_with("https://") {
            Url::parse(href).ok()
        } else {
            base.and_then(|b| b.join(href).ok())
        }?;
        return Some(url);
    }
    None
}

/// Scrape LibGen's search-results HTML into search hits. Each result row
/// carries the title (first `<td>`'s `<b>`), the author (second `<td>`), and a
/// `md5` link used to build the `ads.php` download handle. Selectors are kept
/// to plain element types (no `:nth-child` / attribute-substring) for
/// robustness across scraper's selector grammar.
fn parse_html_results(base: &Url, html: &[u8]) -> Vec<SearchHit> {
    let Ok(text) = std::str::from_utf8(html) else {
        return Vec::new();
    };
    let doc = Html::parse_document(text);
    let Ok(a_sel) = Selector::parse("a") else {
        return Vec::new();
    };
    let Ok(td_sel) = Selector::parse("td") else {
        return Vec::new();
    };
    let Ok(b_sel) = Selector::parse("b") else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for a in doc.select(&a_sel) {
        let Some(href) = a.value().attr("href") else {
            continue;
        };
        if !href.contains("md5=") {
            continue;
        }
        let Some(md5) = extract_md5(href) else {
            continue;
        };
        if !seen.insert(md5.clone()) {
            continue;
        }
        let Some(tr) = a
            .ancestors()
            .find(|n| {
                n.value()
                    .as_element()
                    .map(|el| &*el.name.local == "tr")
                    .unwrap_or(false)
            })
            .and_then(ElementRef::wrap)
        else {
            continue;
        };
        let tds: Vec<ElementRef> = tr.select(&td_sel).collect();
        let title = tds
            .first()
            .map(|td| {
                td.select(&b_sel)
                    .next()
                    .map(|b| b.text().collect::<Vec<_>>().join("").trim().to_string())
                    .filter(|t| !t.is_empty())
                    .or_else(|| {
                        td.select(&a_sel)
                            .next()
                            .map(|x| x.text().collect::<Vec<_>>().join("").trim().to_string())
                    })
                    .unwrap_or_else(|| td.text().collect::<Vec<_>>().join("").trim().to_string())
            })
            .unwrap_or_default();
        let author = tds
            .get(1)
            .map(|td| td.text().collect::<Vec<_>>().join(" ").trim().to_string())
            .unwrap_or_default();
        let handle = match base.join(&format!("ads.php?md5={md5}")) {
            Ok(u) => u.to_string(),
            Err(_) => format!("{base}ads.php?md5={md5}"),
        };
        // Scan the row's cells for the size ("5.2 MB") and pages ("524" /
        // "277—279") columns so a real textbook can be ranked above a 3-page
        // journal excerpt even when LibGen lists the paper first.
        let mut size_bytes = None;
        let mut pages = None;
        for td in &tds {
            let txt = td.text().collect::<Vec<_>>().join(" ").trim().to_string();
            if size_bytes.is_none() {
                size_bytes = parse_size_bytes(&txt);
            }
            if pages.is_none() {
                pages = parse_pages(&txt);
            }
        }
        let kind = if looks_like_article(&title) {
            SourceKind::Article
        } else {
            SourceKind::Book
        };
        hits.push(SearchHit {
            title: if title.is_empty() {
                "(untitled)".into()
            } else {
                title
            },
            authors: if author.is_empty() {
                Vec::new()
            } else {
                vec![author]
            },
            kind,
            origin: Origin::LibGen,
            license: "Copyright — verify before redistribution".into(),
            handle,
            pages,
            size_bytes,
        });
    }
    hits
}

/// A journal article (not a textbook) carries a volume/issue/page citation in
/// its LibGen title — e.g. the live hit that broke grounding was
/// "...1990-jul vol. 136 iss. none pp.277—279". Real textbooks sometimes use
/// "vol." for a book *series* (e.g. "Lecture Notes in Computer Science, vol.
/// 1234") but those lack "iss./pp.", so they stay classified as books.
fn looks_like_article(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    let journal_citation = t.contains("vol.")
        && (t.contains("iss.") || t.contains("pp.") || t.contains("no."));
    let named_serial = t.contains("journal of")
        || t.contains("proceedings of")
        || t.contains("conference on")
        || t.contains("doi:");
    journal_citation || named_serial
}

/// Parses a LibGen size cell ("5.2 MB", "524 kB", "1.3 GB") into bytes. A unit
/// is required so a bare page count is never mistaken for a byte count.
fn parse_size_bytes(text: &str) -> Option<u64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // LibGen renders size as "<number> <unit>" (e.g. "5.2 MB", "524 kB").
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let num: f64 = parts.first()?.parse().ok()?;
    let mult = match parts.get(1) {
        Some(u) => match u.to_ascii_lowercase().as_str() {
            "kb" | "k" => 1_000.0,
            "mb" => 1_000_000.0,
            "gb" => 1_000_000_000.0,
            // A bare number (no unit) is ambiguous — an ID or page count,
            // not a size — so refuse rather than misread it as bytes.
            _ => return None,
        },
        None => return None,
    };
    Some((num * mult) as u64)
}

/// Parses a LibGen pages cell ("524" or a range "277—279") into a page count.
fn parse_pages(text: &str) -> Option<u32> {
    let has_range = text.contains('-') || text.contains('\u{2014}') || text.contains('\u{2013}');
    if has_range {
        let nums: Vec<u32> = text
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        if let (Some(first), Some(last)) = (nums.first(), nums.last()) {
            return Some(last - first + 1);
        }
    }
    let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Book record as returned by LibGen's `out=json` search (libgen.is-style).
/// Only the fields we need are modeled; mirrors vary the exact key set, so
/// everything is `#[serde(default)]` and `author`/`authors` are aliased.
#[derive(Debug, Deserialize)]
struct LibGenBook {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default, alias = "author")]
    authors: String,
    #[serde(default)]
    md5: String,
    #[serde(default)]
    extension: String,
    #[serde(default)]
    year: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    pages: String,
    #[serde(default)]
    publisher: String,
    #[serde(default)]
    filesize: String,
}

/// Map `out=json` book records into search hits. The download handle is the
/// mirror's `/ads.php?md5=<md5>` URL (the `get.php?md5=&key=` hop is resolved
/// at fetch time).
fn books_to_hits(base: &Url, books: &[LibGenBook]) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    for b in books {
        if b.md5.is_empty() || b.title.is_empty() {
            continue;
        }
        let authors = b
            .authors
            .split(',')
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect();
        let download = match base.join(&format!("ads.php?md5={}", b.md5)) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let pages = b.pages.parse::<u32>().ok();
        let size_bytes = b.filesize.parse::<u64>().ok();
        let kind = if looks_like_article(&b.title) {
            SourceKind::Article
        } else {
            SourceKind::Book
        };
        hits.push(SearchHit {
            title: b.title.clone(),
            authors,
            kind,
            origin: Origin::LibGen,
            license: "Copyright — verify before redistribution".into(),
            handle: download.to_string(),
            pages,
            size_bytes,
        });
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    /// Driven against the mock acquisition backend (see
    /// `/tmp/opencode/mock_libgen.py`): proves the HTML-search → `ads.php` →
    /// `get.php` download path actually produces a stored PDF. Skipped unless
    /// `LEARNIVE_MOCK_URL` points at a running mock.
    #[tokio::test]
    async fn libgen_out_json_search_and_fetch_against_mock() {
        let base = match std::env::var("LEARNIVE_MOCK_URL") {
            Ok(b) if !b.is_empty() => b,
            _ => return,
        };
        let src = LibGenSource::new(base);
        let hits = src
            .search("machine learning")
            .await
            .expect("src.search(\"machine learning\")");
        assert!(!hits.is_empty(), "search should return hits");
        assert_eq!(hits[0].origin, Origin::LibGen);
        let fetched = src.fetch(&hits[0]).await.expect("fetch should succeed");
        let pdf = fetched.pdf.expect("fetched source should carry the PDF");
        assert!(
            pdf.windows(b"MOCKLIBGENMARKER".len())
                .any(|w| w == b"MOCKLIBGENMARKER"),
            "downloaded artifact should be the mock PDF"
        );
    }

    /// Real-network validation: resolves a book query through the live
    /// `libgen.li` mirror and confirms the download hop serves a genuine PDF
    /// (without pulling the whole — possibly huge — file). Ignored by default
    /// (hits the network); run with `cargo test -- --ignored`. Fails from
    /// networks where LibGen is blocked.
    #[tokio::test]
    #[ignore]
    async fn libgen_real_download_via_libgen_li() {
        let src = LibGenSource::new("https://libgen.li".to_string());
        let hits = src
            .search("calculus")
            .await
            .expect("libgen.li should answer a book query");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].origin, Origin::LibGen);
        let pdf_url = src
            .resolve_pdf(&hits[0])
            .await
            .expect("should resolve a PDF URL");
        // Read only the first chunk to confirm it's a real PDF.
        let resp = src
            .client
            .get(pdf_url)
            .header("Range", "bytes=0-2047")
            .send()
            .await
            .expect("GET pdf");
        let mut stream = resp.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.expect("chunk"));
            if buf.len() >= 64 {
                break;
            }
        }
        assert!(
            buf.starts_with(b"%PDF"),
            "expected a real PDF, got: {:?}",
            &buf[..buf.len().min(20)]
        );
    }

    #[test]
    fn looks_like_article_flags_journal_citations() {
        // The live hit that broke grounding: a 3-page paper, not a textbook.
        assert!(looks_like_article(
            "Linear Algebra and its Applications    1990-jul vol. 136 iss. none pp.277—279"
        ));
        assert!(looks_like_article("Journal of Fluid Mechanics vol. 12 iss. 3 pp. 45-67"));
        assert!(looks_like_article("Proceedings of the International Conference on ML"));
        // Real textbooks must stay classified as books.
        assert!(!looks_like_article("Introduction to Linear Algebra"));
        assert!(!looks_like_article("Linear Algebra Done Right"));
        // A book *series* uses "vol." but no issue/pages — must stay a book.
        assert!(!looks_like_article("Lecture Notes in Computer Science, vol. 1234"));
    }

    #[test]
    fn parse_size_bytes_handles_units() {
        assert_eq!(parse_size_bytes("5.2 MB"), Some(5_200_000));
        assert_eq!(parse_size_bytes("524 kB"), Some(524_000));
        assert_eq!(parse_size_bytes("1.3 GB"), Some(1_300_000_000));
        assert_eq!(parse_size_bytes("8123456"), None, "bare number is not a size");
        assert_eq!(parse_size_bytes("524"), None);
    }

    #[test]
    fn parse_pages_handles_count_and_range() {
        assert_eq!(parse_pages("524"), Some(524));
        assert_eq!(parse_pages("277—279"), Some(3));
        assert_eq!(parse_pages("100-150"), Some(51));
        assert_eq!(parse_pages("n/a"), None);
    }

    /// Real-network validation of the textbook-vs-article selection: a
    /// textbook-titled query through the live `libgen.li` mirror must resolve
    /// to a `Book` hit via `pick_best_hit`, never the 3-page journal article
    /// that broke grounding before this fix. Ignored by default (hits the
    /// network); run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn libgen_real_prefers_book_for_textbook_query() {
        let src = LibGenSource::new("https://libgen.li".to_string());
        let hits = src
            .search("Linear Algebra")
            .await
            .expect("libgen.li should answer a textbook query");
        assert!(!hits.is_empty());
        let best = crate::source::pick_best_hit(&hits).expect("should pick a hit");
        assert_eq!(
            best.kind,
            crate::source::SourceKind::Book,
            "acquisition should prefer a textbook, got article: {}",
            best.title
        );
        eprintln!(
            "chose textbook: {} ({} bytes)",
            best.title,
            best.size_bytes.unwrap_or(0)
        );
    }

    /// Real-network end-to-end: search a textbook query through `libgen.li`,
    /// pick the best hit (must be a `Book`, never the 3-page article that
    /// broke grounding), then perform the FULL download the acquisition path
    /// does. Validates both the textbook-vs-article selection AND that the
    /// chosen book actually downloads within the client timeout (the 35 MB
    /// "Manga guide" class of source needs the 600s budget, not the old 180s).
    /// Ignored by default (hits the network); run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn libgen_real_full_book_download() {
        let src = LibGenSource::new("https://libgen.li".to_string());
        let hits = src
            .search("Linear Algebra")
            .await
            .expect("libgen.li should answer a textbook query");
        let best = crate::source::pick_best_hit(&hits).expect("should pick a hit");
        assert_eq!(best.kind, crate::source::SourceKind::Book, "{}", best.title);
        let doc = src
            .fetch(&best)
            .await
            .expect("full download of the chosen textbook should succeed");
        let pdf = doc.pdf.expect("fetched source should carry the PDF");
        assert!(pdf.starts_with(b"%PDF"), "downloaded artifact should be a PDF");
        // A real textbook, not a stub — insist on a non-trivial size.
        assert!(pdf.len() > 1_000_000, "textbook PDF too small: {} bytes", pdf.len());
        eprintln!("downloaded textbook '{}' ({} bytes)", best.title, pdf.len());
    }
}
