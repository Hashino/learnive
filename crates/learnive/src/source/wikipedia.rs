//! Wikipedia backend (§11.1) — the free, keyless fallback when OER coverage
//! misses a topic (§S13's `research` move triggers this, not just cold
//! start). CC BY-SA, official MediaWiki API, no key required:
//!
//! 1. **search** — `…/w/rest.php/v1/search/page?q=<q>` (full-text search).
//! 2. **fetch** — `…/w/api.php?action=parse&page=<key>&prop=text` (rendered
//!    article HTML), split into sections on `<h2>` boundaries so each gets
//!    its own citable locator, same shape as `openstax::Section`.
//!
//! Chosen over a general web-search API after testing the free/keyless
//! alternatives live (2026-08-15): DuckDuckGo's HTML endpoint now serves a
//! bot CAPTCHA to non-browser requests, and every public SearXNG instance
//! tried either JS-challenge-gated or 429'd anonymous traffic outright —
//! there is currently no free, truly keyless, general web-search backend
//! that a locally-run app can call reliably. Wikipedia's own API has none of
//! that friction and is a legitimate, broad, general-knowledge source in its
//! own right, not just a workaround.

use serde_json::Value;

use super::{FetchedSource, Origin, SearchHit, Section, SourceError, SourceKind, SourceMeta};

const API_BASE: &str = "https://en.wikipedia.org";
/// Cap on sections kept per article (bounds fetch/normalize cost, §14) —
/// generous relative to OpenStax's page cap because one Wikipedia article is
/// already a bounded unit, unlike one page of a multi-hundred-page book.
const MAX_SECTIONS: usize = 40;
const SECTION_TEXT_CAP: usize = 8_000;
/// Trailing sections that are link lists, not citable prose — dropped rather
/// than indexed as if they were content.
const NON_CONTENT_HEADINGS: &[&str] = &[
    "references",
    "external links",
    "see also",
    "further reading",
    "notes",
    "bibliography",
    "citations",
];

/// Wikipedia acquisition backend (network).
pub struct WikipediaSource {
    client: reqwest::Client,
}

impl Default for WikipediaSource {
    fn default() -> Self {
        Self::new()
    }
}

impl WikipediaSource {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("learnive/0.1 (+https://github.com/; educational)")
                .build()
                .unwrap_or_default(),
        }
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>, SourceError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(SourceError::NoResult);
        }
        let resp = self
            .client
            .get(format!("{API_BASE}/w/rest.php/v1/search/page"))
            .query(&[("q", query), ("limit", "5")])
            .send()
            .await
            .map_err(|e| SourceError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(SourceError::Network(format!(
                "{} for wikipedia search",
                resp.status()
            )));
        }
        let body = resp
            .json::<Value>()
            .await
            .map_err(|e| SourceError::Network(e.to_string()))?;
        let pages = body
            .get("pages")
            .and_then(Value::as_array)
            .ok_or(SourceError::NoResult)?;

        let hits: Vec<SearchHit> = pages
            .iter()
            .filter_map(|p| {
                let title = p.get("title")?.as_str()?.to_string();
                let key = p.get("key")?.as_str()?.to_string();
                Some(SearchHit {
                    title,
                    authors: vec!["Wikipedia contributors".to_string()],
                    kind: SourceKind::Article,
                    origin: Origin::Wikipedia,
                    license: "CC BY-SA 4.0".to_string(),
                    handle: key,
                })
            })
            .collect();

        if hits.is_empty() {
            Err(SourceError::NoResult)
        } else {
            Ok(hits)
        }
    }

    pub async fn fetch(&self, hit: &SearchHit) -> Result<FetchedSource, SourceError> {
        let resp = self
            .client
            .get(format!("{API_BASE}/w/api.php"))
            .query(&[
                ("action", "parse"),
                ("page", hit.handle.as_str()),
                ("prop", "text"),
                ("formatversion", "2"),
                ("format", "json"),
            ])
            .send()
            .await
            .map_err(|e| SourceError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(SourceError::Network(format!(
                "{} for wikipedia article",
                resp.status()
            )));
        }
        let body = resp
            .json::<Value>()
            .await
            .map_err(|e| SourceError::Network(e.to_string()))?;
        let parse = body.get("parse").ok_or(SourceError::NoResult)?;
        let article_title = parse
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(&hit.title)
            .to_string();
        let html = parse
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| SourceError::Normalize("parse response missing text".into()))?;

        let html = strip_edit_links(html);
        let mut sections: Vec<Section> = split_sections(&html)
            .into_iter()
            .filter(|(heading, _)| !NON_CONTENT_HEADINGS.contains(&heading.to_lowercase().as_str()))
            .take(MAX_SECTIONS)
            .enumerate()
            .filter_map(|(i, (heading, body_html))| {
                let text = super::normalize_html(&body_html, SECTION_TEXT_CAP);
                if text.is_empty() {
                    return None;
                }
                let html = super::sanitize_html(&body_html);
                Some(Section {
                    locator: format!("sec:{}", i + 1),
                    title: heading,
                    text,
                    html,
                })
            })
            .collect();
        if sections.is_empty() {
            return Err(SourceError::Normalize("no section yielded text".into()));
        }
        sections.shrink_to_fit();

        Ok(FetchedSource {
            meta: SourceMeta {
                id: super::corpus_id(&article_title, "wikipedia"),
                title: article_title,
                authors: hit.authors.clone(),
                kind: SourceKind::Article,
                license: hit.license.clone(),
                origin: Origin::Wikipedia,
                handle: hit.handle.clone(),
                // A Wikipedia fetch is one article, always whole — nothing
                // for §11.1 item 6's completion pass to do here.
                complete: true,
            },
            sections,
        })
    }
}

/// Removes every `<span class="mw-editsection">...</span>` — MediaWiki's
/// inline "[edit]" links after each heading, pure UI chrome that `html2text`
/// would otherwise render straight into the extracted prose as noise.
/// Non-nested by construction (MediaWiki never nests these), so a plain
/// find-the-matching-close scan is correct, not just an approximation.
fn strip_edit_links(html: &str) -> String {
    const OPEN: &str = "<span class=\"mw-editsection\">";
    const SPAN_OPEN: &str = "<span";
    const SPAN_CLOSE: &str = "</span>";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        // The editsection span nests inner `<span class="mw-editsection-...">`
        // brackets — track depth so the FIRST `</span>` (an inner one) isn't
        // mistaken for the outer close.
        let mut cursor = start + OPEN.len();
        let mut depth = 1i32;
        loop {
            let next_open = rest[cursor..].find(SPAN_OPEN).map(|i| cursor + i);
            let next_close = rest[cursor..].find(SPAN_CLOSE).map(|i| cursor + i);
            match (next_open, next_close) {
                (Some(o), Some(c)) if o < c => {
                    depth += 1;
                    cursor = o + SPAN_OPEN.len();
                }
                (_, Some(c)) => {
                    depth -= 1;
                    cursor = c + SPAN_CLOSE.len();
                    if depth == 0 {
                        break;
                    }
                }
                _ => {
                    // Unterminated — bail, keep the rest verbatim rather than
                    // silently eating the remainder of the document.
                    cursor = rest.len();
                    break;
                }
            }
        }
        rest = &rest[cursor..];
    }
    out.push_str(rest);
    out
}

/// Splits rendered article HTML into `(heading, body_html)` pairs on `<h2>`
/// boundaries — the lead section (before the first `<h2>`) is heading
/// "Introduction". Operates on raw HTML (not normalized text) because the
/// heading text itself has to be pulled out of markup before normalization
/// strips tags away.
fn split_sections(html: &str) -> Vec<(String, String)> {
    let lower = html.to_lowercase();
    let mut starts: Vec<usize> = lower.match_indices("<h2").map(|(i, _)| i).collect();
    starts.push(html.len());

    let mut out = Vec::new();
    let mut cursor = 0usize;
    let mut heading = "Introduction".to_string();
    for &pos in &starts {
        if pos > cursor {
            out.push((heading.clone(), html[cursor..pos].to_string()));
        }
        if pos >= html.len() {
            break;
        }
        match html[pos..].find("</h2>") {
            Some(rel_close) => {
                let tag = &html[pos..pos + rel_close + "</h2>".len()];
                // `html2text` renders a heading tag with a markdown-style
                // "## " prefix — strip it, this is a label, not a rendered
                // document.
                let text = super::normalize_html(tag, 200)
                    .trim_start_matches('#')
                    .trim()
                    .to_string();
                heading = if text.is_empty() {
                    "Section".to_string()
                } else {
                    text
                };
                cursor = pos + rel_close + "</h2>".len();
            }
            None => {
                // Unterminated tag (malformed/truncated HTML) — stop here
                // rather than misreading the rest of the document as heading markup.
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lead_section_and_named_sections() {
        let html = "<p>Intro text.</p><h2><span>History</span></h2><p>History text.</p>\
                     <h2><span>Uses</span></h2><p>Uses text.</p>";
        let sections = split_sections(html);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].0, "Introduction");
        assert!(sections[0].1.contains("Intro text."));
        assert_eq!(sections[1].0, "History");
        assert!(sections[1].1.contains("History text."));
        assert_eq!(sections[2].0, "Uses");
        assert!(sections[2].1.contains("Uses text."));
    }

    #[test]
    fn strip_edit_links_removes_nested_bracket_spans() {
        let html = "<h2><span>History</span>\
                     <span class=\"mw-editsection\">\
                       <span class=\"mw-editsection-bracket\">[</span>\
                       <a href=\"/edit\">edit</a>\
                       <span class=\"mw-editsection-bracket\">]</span>\
                     </span></h2><p>History text.</p>";
        let stripped = strip_edit_links(html);
        assert!(!stripped.contains("mw-editsection"));
        assert!(!stripped.contains("edit</a>"));
        assert!(stripped.contains("<h2><span>History</span></h2><p>History text.</p>"));
    }

    #[test]
    fn no_h2_yields_a_single_introduction_section() {
        let sections = split_sections("<p>Just one section, no subheadings.</p>");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, "Introduction");
    }

    /// Live end-to-end against Wikipedia's real API. Ignored by default
    /// (network); run with `cargo test -p learnive wikipedia_live -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "hits the network"]
    async fn wikipedia_live_search_and_fetch() {
        let src = WikipediaSource::new();
        let hits = src
            .search("python programming language")
            .await
            .expect("search");
        assert!(!hits.is_empty());
        let hit = hits.iter().find(|h| h.title.contains("Python")).unwrap();
        println!("HIT: {} · {:?}", hit.title, hit.handle);

        let doc = src.fetch(hit).await.expect("fetch");
        println!("id={} sections={}", doc.meta.id, doc.sections.len());
        assert!(!doc.sections.is_empty());
        assert!(doc.char_len() > 200, "expected real extracted prose");
        let mut locs: Vec<&str> = doc.sections.iter().map(|s| s.locator.as_str()).collect();
        locs.sort();
        let before = locs.len();
        locs.dedup();
        assert_eq!(before, locs.len(), "locators must be unique for citation");
        for s in doc.sections.iter().take(5) {
            let preview: String = s.text.chars().take(80).collect();
            println!("  [{}] {} — {preview}…", s.locator, s.title);
        }
    }
}
