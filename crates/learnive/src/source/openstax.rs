//! OpenStax backend (§11.1) — free, openly-licensed (CC BY) college textbooks.
//!
//! The default legal source; behind the [`Source`](super::Source) facade like
//! every other backend. Talks to OpenStax's public content API (the same one
//! their reader uses):
//!
//! 1. **catalog** — `…/apps/cms/api/v2/pages/?type=books.Book&search=<q>` lists
//!    matching books with their `cnx_id` (book UUID), authors and license.
//! 2. **release** — `…/rex/release.json` maps a book UUID → the pinned archive
//!    version and gives the archive base URL.
//! 3. **tree** — `…/apps/archive/<ver>/contents/<uuid>@<v>.json` is the table of
//!    contents (chapters → pages).
//! 4. **page** — `…:<page-uuid>.json` carries the page HTML, which we
//!    **normalize** to extracted text (§11.1) after stripping style/script.
//!
//! First acquisition is bounded ([`max_pages`](OpenStaxSource::with_max_pages))
//! so it stays responsive; acquiring an entire book in the background is a §14
//! follow-up. Content is stored once in the immutable [`Corpus`](super::Corpus).

use futures_util::stream::{self, StreamExt};
use serde_json::Value;

use super::{
    FetchedSource, Origin, SearchHit, Section, SourceError, SourceKind, SourceMeta, corpus_id,
};

const BASE: &str = "https://openstax.org";
/// Cap on pages fetched per acquisition (bounds first-fetch latency, §14).
const DEFAULT_MAX_PAGES: usize = 24;
/// Concurrent page fetches — polite to the host, still fast.
const FETCH_CONCURRENCY: usize = 6;
/// Per-section text cap so the corpus stays lean (retrieval chunks it, §10).
const SECTION_TEXT_CAP: usize = 8_000;

/// OpenStax acquisition backend (network).
pub struct OpenStaxSource {
    client: reqwest::Client,
    max_pages: usize,
}

impl Default for OpenStaxSource {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenStaxSource {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("learnive/0.1 (+https://github.com/; educational)")
                .build()
                .unwrap_or_default(),
            max_pages: DEFAULT_MAX_PAGES,
        }
    }

    /// Overrides the first-fetch page cap (e.g. a background full-book acquire).
    pub fn with_max_pages(mut self, n: usize) -> Self {
        self.max_pages = n.max(1);
        self
    }

    async fn get_json(&self, url: &str) -> Result<Value, SourceError> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| SourceError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(SourceError::Network(format!("{} for {url}", resp.status())));
        }
        resp.json::<Value>()
            .await
            .map_err(|e| SourceError::Network(e.to_string()))
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>, SourceError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(SourceError::NoResult);
        }
        let resp = self
            .client
            .get(format!("{BASE}/apps/cms/api/v2/pages/"))
            .query(&[
                ("type", "books.Book"),
                (
                    "fields",
                    "title,cnx_id,authors,license_name,license_version",
                ),
                ("limit", "8"),
                ("search", query),
            ])
            .send()
            .await
            .map_err(|e| SourceError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(SourceError::Network(format!(
                "{} for catalog search",
                resp.status()
            )));
        }
        let body = resp
            .json::<Value>()
            .await
            .map_err(|e| SourceError::Network(e.to_string()))?;
        let items = body
            .get("items")
            .and_then(Value::as_array)
            .ok_or(SourceError::NoResult)?;

        let hits: Vec<SearchHit> = items
            .iter()
            .filter_map(|it| {
                let title = it.get("title")?.as_str()?.to_string();
                let cnx_id = it.get("cnx_id")?.as_str()?.to_string();
                let authors = it
                    .get("authors")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|au| {
                                au.get("value")?.get("name")?.as_str().map(str::to_string)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let lname = it
                    .get("license_name")
                    .and_then(Value::as_str)
                    .unwrap_or("CC BY");
                let lver = it
                    .get("license_version")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let license = if lver.is_empty() {
                    lname.to_string()
                } else {
                    format!("{lname} {lver}")
                };
                Some(SearchHit {
                    title,
                    authors,
                    kind: SourceKind::Book,
                    origin: Origin::OpenStax,
                    license,
                    handle: cnx_id,
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
        let book_uuid = hit.handle.clone();

        // 1. Resolve the pinned archive version for this book.
        let release = self.get_json(&format!("{BASE}/rex/release.json")).await?;
        let archive_url = release
            .get("archiveUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| SourceError::Normalize("release.json missing archiveUrl".into()))?;
        let version = release
            .get("books")
            .and_then(|b| b.get(&book_uuid))
            .and_then(|b| b.get("defaultVersion"))
            .and_then(Value::as_str)
            .ok_or_else(|| SourceError::Normalize("book not in release.json".into()))?
            .to_string();

        // 2. Book tree (table of contents).
        let tree_url = format!("{BASE}{archive_url}/contents/{book_uuid}@{version}.json");
        let tree = self.get_json(&tree_url).await?;
        let book_title = tree
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(&hit.title)
            .to_string();

        // 3. Flatten the tree into ordered leaf pages with a locator each.
        let mut pages: Vec<PageRef> = Vec::new();
        if let Some(root) = tree.get("tree") {
            let mut chapter = 0usize;
            let mut ordinal = 0usize;
            walk_tree(root, 0, &mut chapter, &mut ordinal, &mut pages);
        }
        pages.truncate(self.max_pages);
        if pages.is_empty() {
            return Err(SourceError::NoResult);
        }

        // 4. Fetch page bodies concurrently, then normalize to text.
        let base_page = format!("{BASE}{archive_url}/contents/{book_uuid}@{version}:");
        let fetched: Vec<(usize, Section)> = stream::iter(pages.into_iter().enumerate())
            .map(|(idx, p)| {
                let url = format!("{base_page}{}.json", p.uuid);
                async move {
                    let body = self.get_json(&url).await.ok()?;
                    let html = body.get("content").and_then(Value::as_str)?;
                    let text = normalize_html(html);
                    if text.is_empty() {
                        return None;
                    }
                    Some((
                        idx,
                        Section {
                            locator: p.locator,
                            title: p.title,
                            text,
                        },
                    ))
                }
            })
            .buffer_unordered(FETCH_CONCURRENCY)
            .filter_map(|x| async move { x })
            .collect()
            .await;

        let mut fetched = fetched;
        fetched.sort_by_key(|(idx, _)| *idx);
        let sections: Vec<Section> = fetched.into_iter().map(|(_, s)| s).collect();
        if sections.is_empty() {
            return Err(SourceError::Normalize("no page yielded text".into()));
        }

        Ok(FetchedSource {
            meta: SourceMeta {
                id: corpus_id(&book_title, &format!("openstax:{book_uuid}")),
                title: book_title,
                authors: hit.authors.clone(),
                kind: SourceKind::Book,
                license: hit.license.clone(),
                origin: Origin::OpenStax,
            },
            sections,
        })
    }
}

/// A leaf page located within the book tree.
struct PageRef {
    uuid: String,
    title: String,
    locator: String,
}

/// Depth-first walk of the OpenStax tree, assigning a **unique** locator to each
/// leaf. Top-level `contents` entries increment the chapter. When a title carries
/// an `os-number` (e.g. `2.1`) we use `sec:<n>` (already unique within a book and
/// human-meaningful for citation); otherwise — front matter, chapter intros — we
/// fall back to `chap:<c>;p:<ordinal>` where `ordinal` is a global leaf counter,
/// so two untitled pages never collide (which would break `<cite>` resolution).
fn walk_tree(
    node: &Value,
    depth: usize,
    chapter: &mut usize,
    ordinal: &mut usize,
    out: &mut Vec<PageRef>,
) {
    let Some(contents) = node.get("contents").and_then(Value::as_array) else {
        return;
    };
    for child in contents {
        if child.get("contents").is_some() {
            if depth == 0 {
                *chapter += 1;
            }
            walk_tree(child, depth + 1, chapter, ordinal, out);
        } else {
            let raw_id = child.get("id").and_then(Value::as_str).unwrap_or("");
            let uuid = raw_id.split('@').next().unwrap_or(raw_id).to_string();
            if uuid.is_empty() {
                continue;
            }
            let raw_title = child.get("title").and_then(Value::as_str).unwrap_or("");
            let title = strip_tags(raw_title);
            *ordinal += 1;
            let locator = match section_number(raw_title) {
                Some(n) => format!("sec:{n}"),
                None => format!("chap:{};p:{}", (*chapter).max(1), *ordinal),
            };
            out.push(PageRef {
                uuid,
                title,
                locator,
            });
        }
    }
}

/// Extracts an OpenStax `os-number` (e.g. `2.1`) from a title's HTML, if present.
fn section_number(title_html: &str) -> Option<String> {
    let marker = "os-number";
    let start = title_html.find(marker)?;
    let after = &title_html[start..];
    let gt = after.find('>')? + 1;
    let rest = &after[gt..];
    let end = rest.find('<')?;
    let num = rest[..end].trim();
    if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit() || c == '.') {
        Some(num.to_string())
    } else {
        None
    }
}

/// Strips HTML from a short string (a page/section title), collapsing whitespace.
fn strip_tags(s: &str) -> String {
    html_to_text(s)
}

/// Normalizes acquired page HTML to extracted text (§11.1) via `html2text`
/// (which renders visible text and skips `<style>`/`<script>`), collapsed and
/// capped so the corpus stays lean (retrieval chunks it further, §10).
fn normalize_html(html: &str) -> String {
    let mut text = html_to_text(html);
    if text.len() > SECTION_TEXT_CAP {
        let mut end = SECTION_TEXT_CAP;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// HTML → plain text via the `html2text` crate, with whitespace collapsed to
/// single spaces (the rendered line-wrapping is irrelevant for retrieval).
fn html_to_text(html: &str) -> String {
    let rendered = html2text::config::plain()
        .string_from_read(html.as_bytes(), 100)
        .unwrap_or_default();
    rendered.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_drops_css_keeps_prose() {
        let html = "<style>/* STYLING */ :target{background:#ffc}</style>\
                    <section><p>A limit describes the value a function approaches.</p></section>";
        let text = normalize_html(html);
        assert!(!text.contains("STYLING"));
        assert!(!text.contains("background"));
        assert!(text.contains("A limit describes the value a function approaches."));
    }

    #[test]
    fn section_number_parses_os_number() {
        let t = r#"<span class="os-number">2.1</span><span class="os-text">Limits</span>"#;
        assert_eq!(section_number(t).as_deref(), Some("2.1"));
        assert_eq!(section_number("<span>Preface</span>"), None);
    }

    /// Live end-to-end against the OpenStax API. Ignored by default (network);
    /// run with `cargo test -p learnive openstax_live -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "hits the network"]
    async fn openstax_live_search_and_fetch() {
        let src = OpenStaxSource::new().with_max_pages(3);
        let hits = src.search("calculus").await.expect("search");
        assert!(!hits.is_empty());
        let hit = hits.iter().find(|h| h.title.contains("Calculus")).unwrap();
        println!("HIT: {} · {} · {:?}", hit.title, hit.license, hit.authors);
        assert!(hit.license.to_lowercase().contains("creative commons"));

        let doc = src.fetch(hit).await.expect("fetch");
        println!("id={} sections={}", doc.meta.id, doc.sections.len());
        assert!(!doc.sections.is_empty());
        assert!(doc.char_len() > 200, "expected real extracted prose");
        let mut locs: Vec<&str> = doc.sections.iter().map(|s| s.locator.as_str()).collect();
        locs.sort();
        let before = locs.len();
        locs.dedup();
        assert_eq!(before, locs.len(), "locators must be unique for citation");
        for s in doc.sections.iter().take(3) {
            println!(
                "  [{}] {} — {}…",
                s.locator,
                s.title,
                &s.text[..s.text.len().min(80)]
            );
        }
    }
}
