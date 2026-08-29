//! Sci-Hub acquisition backend (§11.1).
//!
//! Sci-Hub is a DOI/PMID resolver, not a free-text search engine, so a non-DOI
//! query yields `NoResult` here and the acquisition chain falls through to
//! other backends (e.g. LibGen for books). The PDF itself is downloaded
//! directly from the resolved URL.
//!
//! Sci-Hub domains rotate and are frequently behind anti-bot challenges
//! (Cloudflare-style "Checking your browser" interstitials); one or more
//! comma-separated mirror roots are tried in order. When every mirror is
//! challenged, we surface `NoResult` rather than failing the whole
//! acquisition — same graceful degradation as the other backends.
//!
//! We resolve the paper page and extract the real PDF URL ourselves rather than
//! relying on a DOM-scraping crate: the live mirrors embed the PDF either in an
//! `<iframe src="…pdf">` or in a `#buttons` element whose `onclick` opens
//! `location.href='…pdf'`. The *first* `#buttons [onclick]` is frequently a
//! donate popup (e.g. `sci-hub.kvnp.top`), so we specifically look for a
//! `.pdf`-bearing URL instead of taking the first match.

use url::Url;

use super::{FetchedSource, Origin, SearchHit, SourceError, SourceKind, fetched_from_pdf};

/// A Sci-Hub mirror (or a list of candidate mirrors) to try in order.
pub struct SciHubSource {
    client: reqwest::Client,
    bases: Vec<String>,
}

impl SciHubSource {
    /// `base_url` is one or more mirror roots, comma-separated (e.g.
    /// `"https://sci-hub.ee,https://sci-hub.se"`). Each is tried in order until
    /// one answers. A trailing slash is tolerated on every entry.
    pub fn new(base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("learnive/0.1 (+https://github.com/; educational)")
            .timeout(std::time::Duration::from_secs(120))
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

    /// A DOI looks like `10.<registry>/<suffix>`; Sci-Hub resolves those.
    fn is_doi(q: &str) -> bool {
        let q = q.trim();
        let Some(rest) = q.strip_prefix("10.") else {
            return false;
        };
        let Some(first) = rest.chars().next() else {
            return false;
        };
        first.is_ascii_digit() && rest.contains('/')
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>, SourceError> {
        let q = query.trim();
        if !Self::is_doi(q) {
            return Err(SourceError::NoResult);
        }
        // Sci-Hub serves the paper at `<mirror>/<doi>`; try each mirror in order.
        let mut last_err: Option<SourceError> = None;
        for base in &self.bases {
            let Some(base_url) = Url::parse(base).ok() else {
                continue;
            };
            let Ok(url) = base_url.join(q) else {
                continue;
            };
            let resp = match self.client.get(url.clone()).send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(Self::net(e));
                    continue;
                }
            };
            if !resp.status().is_success() {
                last_err = None;
                continue;
            }
            let html = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    last_err = Some(Self::net(e));
                    continue;
                }
            };
            // Confirm it's a Sci-Hub paper page (title carries "<paper> | <doi>").
            let Some(title) = Self::extract_title(&html) else {
                continue;
            };
            let Some(pdf) = Self::extract_pdf_url(&html, &url) else {
                continue;
            };
            let Ok(pdf_url) = Url::parse(&pdf) else {
                continue;
            };
            let hit = SearchHit {
                title,
                authors: Vec::new(),
                kind: SourceKind::Article,
                origin: Origin::SciHub,
                license: "Accessed via Sci-Hub — verify copyright before redistribution".into(),
                handle: pdf_url.to_string(),
                pages: None,
                size_bytes: None,
            };
            return Ok(vec![hit]);
        }
        Err(last_err.unwrap_or(SourceError::NoResult))
    }

    pub async fn fetch(&self, hit: &SearchHit) -> Result<FetchedSource, SourceError> {
        // The handle is an absolute (or protocol-relative) PDF URL resolved in
        // `search`.
        let handle = if let Some(rest) = hit.handle.strip_prefix("//") {
            format!("https:{rest}")
        } else {
            hit.handle.clone()
        };
        let url = Url::parse(&handle).map_err(Self::net)?;
        let resp = self.client.get(url).send().await.map_err(Self::net)?;
        if !resp.status().is_success() {
            return Err(SourceError::Network(format!(
                "scihub download HTTP {}",
                resp.status()
            )));
        }
        let bytes = resp.bytes().await.map_err(Self::net)?;
        if bytes.is_empty() {
            return Err(SourceError::Normalize(
                "scihub returned an empty file".into(),
            ));
        }
        fetched_from_pdf(hit, &bytes)
    }

    /// Parse the paper title out of a Sci-Hub page `<title>`, which is shaped
    /// `"Sci-Hub | <paper title> | <doi>"` (some mirrors drop the leading
    /// "Sci-Hub"). We return the middle segment.
    fn extract_title(html: &str) -> Option<String> {
        let tag = html.find("<title>")?;
        let start = tag + 7;
        let end = html[start..].find("</title>")? + start;
        let title = &html[start..end];
        let parts: Vec<&str> = title.split('|').collect();
        if parts.len() >= 2 {
            let mid = parts[1].trim();
            if !mid.is_empty() {
                return Some(mid.to_string());
            }
        }
        let trimmed = title.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// Pull the real PDF URL out of a Sci-Hub page. We specifically look for a
    /// `.pdf`-bearing URL (in an `<iframe src>` or an `onclick` `location.href`)
    /// rather than blindly taking the first `#buttons [onclick]`, which is
    /// frequently a donate popup on live mirrors.
    fn extract_pdf_url(html: &str, base: &Url) -> Option<String> {
        // 1) <iframe src="..."> — the PDF viewer.
        for tag in html.split("<iframe") {
            if let Some(src) = Self::attr(tag, "src") {
                let lower = src.to_ascii_lowercase();
                if lower.contains(".pdf") || lower.contains("/pdf/") {
                    return Some(Self::resolve_relative(&src, base));
                }
            }
        }
        // 2) any onclick="..." whose inner URL is .pdf-bearing.
        let mut fallback: Option<String> = None;
        for frag in html.split("onclick=") {
            let Some(attr_val) = Self::quoted(frag) else {
                continue;
            };
            let Some(inner) = Self::inner_url(&attr_val) else {
                continue;
            };
            let lower = inner.to_ascii_lowercase();
            if lower.contains(".pdf") || lower.contains("/pdf/") {
                return Some(Self::resolve_relative(&inner, base));
            }
            if fallback.is_none() {
                fallback = Some(Self::resolve_relative(&inner, base));
            }
        }
        fallback
    }

    /// Within an `onclick` attribute value such as
    /// `location.href='https://x/y.pdf'`, extract the inner quoted URL.
    fn inner_url(s: &str) -> Option<String> {
        let q = s.chars().find(|c| *c == '\'' || *c == '"')?;
        let start = s.find(q)?.checked_add(1)?;
        let end = s[start..].find(q)? + start;
        Some(Self::sanitize_url(&s[start..end]))
    }

    /// Extract an attribute value (`name="..."` / `name='...'`) from the start
    /// of a tag fragment (text immediately following the tag name).
    fn attr(frag: &str, name: &str) -> Option<String> {
        let pat = format!("{name}=");
        let pos = frag.find(&pat)? + pat.len();
        let rest = &frag[pos..];
        let q = rest.chars().next()?;
        if q != '\'' && q != '"' {
            return None;
        }
        let end = rest[1..].find(q)? + 1;
        Some(Self::sanitize_url(&rest[1..end]))
    }

    /// Extract the first quoted string from the start of a fragment (the value
    /// right after `onclick=`).
    fn quoted(frag: &str) -> Option<String> {
        let q = frag.chars().next()?;
        if q != '\'' && q != '"' {
            return None;
        }
        let end = frag[1..].find(q)? + 1;
        Some(Self::sanitize_url(&frag[1..end]))
    }

    /// Sci-Hub sometimes emits backslash-escaped slashes inside `onclick`
    /// (`https:\/\/host\/x.pdf`); strip them so the URL parses.
    fn sanitize_url(u: &str) -> String {
        u.replace('\\', "")
    }

    /// Resolve a possibly-relative / protocol-relative URL against the page base.
    fn resolve_relative(u: &str, base: &Url) -> String {
        if u.starts_with("//") {
            return format!("{}:{}", base.scheme(), u);
        }
        if let Ok(mut abs) = Url::parse(u) {
            abs.set_fragment(None);
            return abs.to_string();
        }
        if let Ok(rel) = base.join(u) {
            return rel.to_string();
        }
        u.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Driven against the mock acquisition backend: proves a DOI resolves to a
    /// PDF through our own page scrape + extraction. Skipped unless
    /// `LEARNIVE_MOCK_URL` points at a running mock.
    #[tokio::test]
    async fn scihub_resolves_doi_against_mock() {
        let base = match std::env::var("LEARNIVE_MOCK_URL") {
            Ok(b) if !b.is_empty() => b,
            _ => return,
        };
        let src = SciHubSource::new(base);
        let hits = src
            .search("10.1234/abc")
            .await
            .expect("did not find a source");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].origin, Origin::SciHub);
        let fetched = src.fetch(&hits[0]).await.expect("fetch should succeed");
        let pdf = fetched.pdf.expect("fetched source should carry the PDF");
        assert!(
            pdf.windows(b"MOCKLIBGENMARKER".len())
                .any(|w| w == b"MOCKLIBGENMARKER"),
            "downloaded artifact should be the mock PDF"
        );
    }

    #[tokio::test]
    async fn scihub_rejects_non_doi_query() {
        let base = match std::env::var("LEARNIVE_MOCK_URL") {
            Ok(b) if !b.is_empty() => b,
            _ => return,
        };
        let src = SciHubSource::new(base);
        // Sci-Hub is a DOI resolver, not a free-text search: a topic query must
        // yield NoResult (and let the acquisition chain fall through).
        assert!(src.search("machine learning").await.is_err());
    }

    /// Real-network validation: resolves a genuine DOI through the live
    /// `sci-hub.ee` mirror (no Cloudflare interstitial) and downloads the actual
    /// PDF via our own extraction. Ignored by default (hits the network); run
    /// with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn scihub_real_download_via_scihub_ee() {
        let src = SciHubSource::new("https://sci-hub.ee".to_string());
        let hits = src
            .search("10.1038/s41586-020-2649-2")
            .await
            .expect("real DOI should resolve on sci-hub.ee");
        assert_eq!(hits[0].origin, Origin::SciHub);
        let fetched = src.fetch(&hits[0]).await.expect("real PDF download");
        let pdf = fetched.pdf.expect("fetched source should carry the PDF");
        assert!(
            pdf.starts_with(b"%PDF"),
            "expected a real PDF, got: {:?}",
            &pdf[..pdf.len().min(20)]
        );
    }
}
