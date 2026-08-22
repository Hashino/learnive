//! Source acquisition (§11, §11.1) — **legal open sources only**.
//!
//! Node content is grounded in real sources cited by book+chapter or article
//! (§11). Acquisition is agent-driven and behind ONE swappable facade so the
//! backend is an implementation detail (matters for the §15 hosting endgame):
//! LibGen is deliberately **not** part of this — the default chain is Open
//! Educational Resources (OpenStax, LibreTexts) + open-access books/papers +
//! public domain. See the project memory `acquisition-oer-not-libgen`.
//!
//! Swap seam: [`Source`] is an enum facade (same idiom as `ai::Provider`); a new
//! backend — including a hosted one — is a new variant, no call-site changes.
//! Everything a backend returns is **normalized** to one internal representation
//! ([`FetchedSource`]: extracted text + the app HTML dialect), so source format
//! (EPUB/PDF/HTML) is an acquisition detail invisible downstream (§11.1).
//!
//! Not fully consumed at runtime yet (grounding is Phase B); hence the temporary
//! `allow`s (mirrors `ai::mod`).
#![allow(dead_code, unused_imports)]

pub mod corpus;
pub mod mock;
pub mod openstax;
pub mod wikipedia;

pub use corpus::{Corpus, CorpusError};
pub use mock::MockSource;
pub use openstax::OpenStaxSource;
pub use wikipedia::WikipediaSource;

/// MathML tags a source may deliver (OpenStax; LibreTexts's LaTeX is
/// converted to MathML separately by `learnive_core::math` at freeze time) —
/// not in `ammonia`'s prose-oriented default whitelist, so listed explicitly.
const MATHML_TAGS: &[&str] = &[
    "math",
    "mi",
    "mn",
    "mo",
    "mrow",
    "mfrac",
    "msup",
    "msub",
    "msubsup",
    "msqrt",
    "mroot",
    "mtable",
    "mtr",
    "mtd",
    "mtext",
    "mspace",
    "mstyle",
    "menclose",
    "mpadded",
    "mphantom",
    "mfenced",
    "munder",
    "mover",
    "munderover",
    "semantics",
    "annotation",
    "mmultiscripts",
    "mprescripts",
    "none",
];

/// Sanitizes acquired source HTML **once, at ingestion** (§11.1 item 2) —
/// before it is ever stored in the corpus or reaches a browser. Beyond
/// `ammonia`'s default whitelist (already covers headings, lists, tables,
/// `figure`/`figcaption`, `code`/`pre`, `sub`/`sup`, and `img[src]`) this adds
/// the `<math>` subtree so OpenStax's MathML survives. `img[src]` is kept
/// pointed at the remote host here — [`localize_images`] downloads figures
/// into the corpus and rewrites `src` as a separate pass over the *already
/// sanitized* HTML (§11.1 item 5), so a caller that skips it (tests, `mock.rs`)
/// still gets a source where the figure IS the content usable in the reader.
pub(crate) fn sanitize_html(html: &str) -> String {
    ammonia::Builder::default()
        .add_tags(MATHML_TAGS)
        .add_generic_attributes(["mathvariant", "display", "xmlns", "columnalign", "rowalign"])
        .clean(html)
        .to_string()
}

/// Concurrent image downloads per acquisition — polite to the image host,
/// mirrors `openstax.rs`'s `FETCH_CONCURRENCY` for the same reason.
const IMAGE_FETCH_CONCURRENCY: usize = 6;

/// A `reqwest::Client` for [`localize_images`], identified the same way the
/// backends identify themselves to OpenStax/Wikipedia — a courtesy to
/// whatever host is actually serving the figure, which is almost never the
/// backend's own host.
pub(crate) fn image_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("learnive/0.1 (+https://github.com/; educational)")
        .build()
        .unwrap_or_default()
}

/// Raster image content-types this app will store and serve back (§11.1 item
/// 5). Deliberately **excludes `image/svg+xml`**: unlike a raster format, an
/// SVG can carry an embedded `<script>` that a browser executes if the asset
/// URL is ever opened as a top-level navigation (not just `<img>`-embedded,
/// where SVG script execution is already suppressed) — sanitizing untrusted
/// SVG bytes is its own project, and every OER backend here already delivers
/// figures as raster images, so the safer move is to just not serve SVG from
/// this app's own origin.
fn image_extension(content_type: &str) -> Option<&'static str> {
    match content_type {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// Rejects loopback/link-local/private-range hosts (§3.1's spirit extended to
/// outbound fetches, cheap SSRF hardening): the `<img src>` values here come
/// from third-party page content (an OpenStax page, a Wikipedia article) that
/// this app doesn't otherwise vet host-by-host, so a malicious or compromised
/// page embedding e.g. `http://169.254.169.254/...` shouldn't get an
/// authenticated-context server to fetch it on its behalf. Prefix-based, not
/// full CIDR parsing — proportionate to what a source backend's HTML can
/// actually smuggle in an `<img src>`, not a general-purpose SSRF filter.
fn is_safe_image_host(url: &reqwest::Url) -> bool {
    if url.scheme() != "http" && url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host == "localhost" || host == "::1" {
        return false;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        match ip {
            std::net::IpAddr::V4(v4) => {
                if v4.is_loopback() || v4.is_link_local() || v4.is_private() {
                    return false;
                }
            }
            std::net::IpAddr::V6(v6) => {
                if v6.is_loopback() || (v6.segments()[0] & 0xffc0) == 0xfe80 {
                    return false;
                }
            }
        }
    }
    true
}

/// Downloads every remotely-hosted `<img src>` across a source's sections
/// into the corpus (§11.1 item 5) and rewrites `src` to the locally-served
/// `/api/sources/{id}/assets/{hash}.{ext}` — otherwise the reader can't work
/// offline, and every panel open would tell the source host what page the
/// learner is on. Two passes over the HTML with `lol_html` (streaming
/// attribute rewriting, not a hand-rolled parser): first collect every
/// distinct URL, then download concurrently, then rewrite. Best-effort per
/// image — a download/network failure just leaves that `<img>` pointed at
/// the remote host (normalized to `https:` if it was protocol-relative, see
/// `rewrite_image_srcs`) rather than failing the whole acquisition over one
/// broken figure.
pub(crate) async fn localize_images(
    client: &reqwest::Client,
    corpus: &Corpus,
    source_id: &str,
    sections: &mut [Section],
) {
    let mut urls = std::collections::HashSet::new();
    for section in sections.iter() {
        collect_image_urls(&section.html, &mut urls);
    }
    if urls.is_empty() {
        return;
    }

    use futures_util::StreamExt;
    let rewrites: std::collections::HashMap<String, String> = futures_util::stream::iter(urls)
        .map(|url| {
            let client = client.clone();
            async move {
                let local = download_and_store(&client, corpus, source_id, &url).await;
                local.map(|path| (url, path))
            }
        })
        .buffer_unordered(IMAGE_FETCH_CONCURRENCY)
        .filter_map(|x| async move { x })
        .collect()
        .await;

    // Always run the rewrite pass, even with zero successful downloads:
    // `rewrite_image_srcs` also normalizes any *still-remote* protocol-
    // relative `src` to an explicit `https:` (see its doc comment) — this
    // app is served over plain HTTP (§3), so leaving `//host/path` behind
    // after a failed download would resolve to `http://` in the browser and
    // get silently blocked by the CSP's `img-src ... https:`. Skipping this
    // pass when nothing downloaded would un-do exactly that safety net.
    for section in sections.iter_mut() {
        section.html = rewrite_image_srcs(&section.html, &rewrites);
    }
}

/// Pass 1 of [`localize_images`]: every remotely-hosted `<img src>` in
/// `html`, deduplicated. Matches `http(s)://` and also **protocol-relative**
/// `//host/path` — Wikipedia (found live, verifying §11.1 item 7) emits
/// exactly that form, and skipping it silently would defeat item 5's whole
/// point for that backend: the browser resolves `//...` against the page's
/// own scheme and loads it directly from the remote host regardless,
/// offline-reading and privacy rationale included. Kept as the *original*
/// string (not normalized) so pass 2's exact-match rewrite still finds it;
/// [`download_and_store`] is what actually normalizes it into a fetchable
/// absolute URL. Ignores parse errors (malformed fragments just yield fewer
/// URLs, never a hard failure — sanitized source HTML, not a security
/// boundary at this point).
fn collect_image_urls(html: &str, urls: &mut std::collections::HashSet<String>) {
    let mut found = Vec::new();
    let _ = lol_html::rewrite_str(
        html,
        lol_html::RewriteStrSettings::new().append_element_content_handler(lol_html::element!(
            "img[src]",
            |el| {
                if let Some(src) = el.get_attribute("src")
                    && (src.starts_with("http://")
                        || src.starts_with("https://")
                        || src.starts_with("//"))
                {
                    found.push(src);
                }
                Ok(())
            }
        )),
    );
    urls.extend(found);
}

/// Pass 2 of [`localize_images`]: rewrites every `<img src>` present in
/// `rewrites` to its local path. Anything not in the map (a failed
/// download) is left pointing at the remote host — **except** a still
/// protocol-relative `src` (`//host/path`), which gets normalized to an
/// explicit `https:` rather than left as-is: this app is served over plain
/// HTTP (§3's local server), so a `//`-relative URL resolves to `http://`
/// in the browser and the CSP's `img-src 'self' data: https:` then blocks
/// it outright — found live (§11.1 item 7's verification) against a real
/// Wikipedia image whose download failed. Forcing `https:` at least gives
/// the browser a URL the CSP allows, matching what the download attempt
/// itself already assumed.
fn rewrite_image_srcs(html: &str, rewrites: &std::collections::HashMap<String, String>) -> String {
    lol_html::rewrite_str(
        html,
        lol_html::RewriteStrSettings::new().append_element_content_handler(lol_html::element!(
            "img[src]",
            |el| {
                if let Some(src) = el.get_attribute("src") {
                    if let Some(local) = rewrites.get(&src) {
                        el.set_attribute("src", local)?;
                    } else if let Some(rest) = src.strip_prefix("//") {
                        el.set_attribute("src", &format!("https://{rest}"))?;
                    }
                }
                Ok(())
            }
        )),
    )
    .unwrap_or_else(|_| html.to_string())
}

/// Resolves every **relative** `<img src>` in `html` (no scheme, not
/// protocol-relative) against `base`, turning it into an absolute URL —
/// found live (§11.1 item 7's verification) against real OpenStax content:
/// its page HTML links figures as `../resources/<hash>`, relative to the
/// page's own archive URL, which `collect_image_urls` (matching only
/// `http(s)://`/`//`) silently never saw, so no OpenStax figure was ever
/// being localized despite item 5 nominally covering every backend. Must run
/// before [`localize_images`], since that function only recognizes absolute
/// or protocol-relative URLs; a backend with server-relative image links
/// (only OpenStax today) resolves them itself, in its own module, because
/// only it knows the right base URL. Malformed fragments are left as-is
/// (same best-effort spirit as [`collect_image_urls`]).
pub(crate) fn absolutize_relative_image_srcs(html: &str, base: &str) -> String {
    let Ok(base_url) = reqwest::Url::parse(base) else {
        return html.to_string();
    };
    lol_html::rewrite_str(
        html,
        lol_html::RewriteStrSettings::new().append_element_content_handler(lol_html::element!(
            "img[src]",
            |el| {
                if let Some(src) = el.get_attribute("src")
                    && !src.starts_with("http://")
                    && !src.starts_with("https://")
                    && !src.starts_with("//")
                    && !src.starts_with("data:")
                    && !src.starts_with('#')
                    && let Ok(resolved) = base_url.join(&src)
                {
                    el.set_attribute("src", resolved.as_str())?;
                }
                Ok(())
            }
        )),
    )
    .unwrap_or_else(|_| html.to_string())
}

/// Downloads one image and stores it in the corpus under a content hash
/// (§11.1 item 5); `None` on any failure (network, non-2xx, non-raster
/// content-type) or on an unsafe host ([`is_safe_image_host`]) — every
/// failure here is recoverable by leaving that image at its remote `src`.
async fn download_and_store(
    client: &reqwest::Client,
    corpus: &Corpus,
    source_id: &str,
    url: &str,
) -> Option<String> {
    // Protocol-relative (`//host/path`, see `collect_image_urls`) has no
    // scheme for `Url::parse` to work with — the browser would resolve it
    // against the page's own (https) scheme, so do the same here.
    let absolute = if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        url.to_string()
    };
    let parsed = reqwest::Url::parse(&absolute).ok()?;
    if !is_safe_image_host(&parsed) {
        return None;
    }
    let resp = client.get(parsed).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let ext = image_extension(&content_type)?;
    let bytes = resp.bytes().await.ok()?;
    if bytes.is_empty() {
        return None;
    }
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(&bytes);
    let filename = format!(
        "{}.{ext}",
        hash.iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    corpus.store_asset(source_id, &filename, &bytes).ok()?;
    Some(format!("/api/sources/{source_id}/assets/{filename}"))
}

/// HTML → plain text via the `html2text` crate, whitespace collapsed to
/// single spaces (rendered line-wrapping is irrelevant for retrieval) and
/// truncated to `cap` chars so the corpus stays lean (retrieval chunks it
/// further, §10) — shared by every backend that normalizes HTML sections
/// (`openstax`, `wikipedia`).
pub(crate) fn normalize_html(html: &str, cap: usize) -> String {
    let rendered = html2text::config::plain()
        .string_from_read(html.as_bytes(), 100)
        .unwrap_or_default();
    let mut text = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.len() > cap {
        let mut end = cap;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// What kind of thing a source is — steers how a locator is read (§4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// A book: locator is `chap:N;sec:M;p:K` style.
    Book,
    /// An article/paper: locator is `sec:N;p:K` or a paragraph index.
    Article,
    /// A web page (fallback grounding): attributed inline + tracked in SOURCES.md.
    Web,
}

/// Where a source came from — kept for attribution and for the §16 note that the
/// backend must stay swappable. `Web` carries its URL for inline attribution.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "backend")]
pub enum Origin {
    OpenStax,
    /// Wikipedia (§11.1's "internet search" fallback tier) — free, keyless,
    /// CC BY-SA. See `wikipedia` module docs for why this backend and not a
    /// general search API.
    Wikipedia,
    LibreTexts,
    /// Open-access book registries (DOAB/OAPEN) — future backend.
    OpenAccessBook,
    /// arXiv / PMC / OpenAlex — future backend.
    OpenAccessPaper,
    /// Public domain (Project Gutenberg / Internet Archive) — future backend.
    PublicDomain,
    /// Web-search fallback (§11.1), attributed inline ("segundo o site X ...").
    Web {
        url: String,
    },
    /// Canned content for demo/tests (no network).
    Mock,
}

/// A candidate returned by [`Source::search`], before the (heavier) fetch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    pub title: String,
    pub authors: Vec<String>,
    pub kind: SourceKind,
    pub origin: Origin,
    /// Human-readable license (e.g. `CC BY 4.0`, `Public Domain`).
    pub license: String,
    /// Backend-specific opaque handle used to [`fetch`](Source::fetch) this hit.
    pub handle: String,
}

/// Immutable metadata for a source that lives in the corpus (§4). Stable across
/// reuse; the `id` is what a `<cite data-source-id=...>` points at (§4.3).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceMeta {
    /// Stable corpus id (slug + short hash) — the citation target.
    pub id: String,
    pub title: String,
    pub authors: Vec<String>,
    pub kind: SourceKind,
    pub license: String,
    pub origin: Origin,
    /// The backend-specific handle ([`SearchHit::handle`]) this source was
    /// fetched from — kept so §11.1 item 6's background completion pass can
    /// rebuild a [`SearchHit`] and re-fetch without a fresh search.
    /// `#[serde(default)]`: sources acquired before this field existed
    /// deserialize with an empty string; [`complete_source`] treats an empty
    /// handle as nothing-to-re-fetch-against, same non-destructive stance as
    /// the `html` backfill (item 1) — they stay as-is until a fresh
    /// acquisition replaces them.
    #[serde(default)]
    pub handle: String,
    /// Whether this acquisition got the whole source or was capped by
    /// first-fetch latency bounds (§14) — §11.1 item 6. `#[serde(default)]`
    /// deliberately defaults to `true`: sources acquired before this field
    /// existed have no recorded truncation, and there is no way to tell
    /// after the fact whether they were capped, so treating them as
    /// "nothing known to be pending" is honest — a background completion
    /// job with no evidence of truncation would just be silent extra network
    /// traffic against sources that may already be whole.
    #[serde(default = "default_true")]
    pub complete: bool,
}

fn default_true() -> bool {
    true
}

/// One addressable piece of a source — the unit a `data-locator` names (§4.3)
/// and the unit that gets chunked/embedded for retrieval (§10, Phase B).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Section {
    /// Locator string, e.g. `chap:3;sec:2` — goes verbatim into
    /// `<cite data-locator="...">` so a citation resolves back here (§4.3).
    pub locator: String,
    pub title: String,
    /// Extracted plain text — the normalization target used for retrieval (§10).
    pub text: String,
    /// Sanitized HTML (§11.1 item 1/2, S19) — what the real reader (item 7)
    /// and passage deep-linking (item 8, `learnive_core::anchor::resolve_quote`
    /// against this field) will render/resolve against, instead of `text`'s
    /// flattened prose. `#[serde(default)]`: every source already in the
    /// corpus before this field existed has no `html` in its `source.json` —
    /// they deserialize with an empty string here rather than failing to
    /// load, and stay readable (via `text`) until a completion/re-ingest pass
    /// (§11.1 item 6) backfills them.
    #[serde(default)]
    pub html: String,
}

/// A source after acquisition, **normalized** (§11.1): metadata + addressable
/// sections of extracted text. Whatever the on-disk format was (EPUB/PDF/HTML),
/// downstream only ever sees this.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FetchedSource {
    pub meta: SourceMeta,
    pub sections: Vec<Section>,
}

/// A section's locator+title, without its body — what a table of contents is
/// made of (§11.1 item 4, S19).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SectionSummary {
    pub locator: String,
    pub title: String,
}

/// A source's metadata + table of contents, without any section body (§11.1
/// item 4) — the cheap thing `GET /api/sources/{id}` returns instead of the
/// whole [`FetchedSource`], so opening the source panel or reading the
/// sumário doesn't ship a whole book. A section's body comes separately,
/// addressed by locator (`Corpus::load_section`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceIndex {
    pub meta: SourceMeta,
    pub toc: Vec<SectionSummary>,
}

impl FetchedSource {
    /// Total extracted characters — a cheap proxy for "did we get real content".
    pub fn char_len(&self) -> usize {
        self.sections.iter().map(|s| s.text.len()).sum()
    }
}

/// Acquisition errors. Network/parse failures are recoverable — the caller falls
/// back down the §11.1 chain (OER → OA → web search).
#[derive(Debug)]
pub enum SourceError {
    /// Backend reached but returned nothing usable for the query.
    NoResult,
    /// Network/transport failure.
    Network(String),
    /// Fetched but could not be normalized to text.
    Normalize(String),
    /// Persisting to / reading from the corpus failed.
    Corpus(CorpusError),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::NoResult => write!(f, "no adequate source found"),
            SourceError::Network(e) => write!(f, "acquisition network error: {e}"),
            SourceError::Normalize(e) => write!(f, "could not normalize source: {e}"),
            SourceError::Corpus(e) => write!(f, "corpus error: {e}"),
        }
    }
}

impl std::error::Error for SourceError {}

impl From<CorpusError> for SourceError {
    fn from(e: CorpusError) -> Self {
        SourceError::Corpus(e)
    }
}

/// Swappable acquisition facade (§11.1). Each variant is one backend; the app
/// asks by intent (search a topic, fetch a hit) without knowing which backend
/// serves it. Add a hosted/registry backend as a new variant.
pub enum Source {
    /// Canned content — demo mode and tests, no network.
    Mock(MockSource),
    /// OpenStax OER textbooks (network) — the default legal source.
    OpenStax(OpenStaxSource),
    /// Wikipedia (network) — free/keyless fallback, §11.1's "internet search" tier.
    Wikipedia(WikipediaSource),
    // Future, behind the same facade (kept as doc so the seam is explicit):
    //   LibreTexts(LibreTextsSource),
    //   OpenAccess(OpenAccessSource),// DOAB/OAPEN, arXiv/PMC
}

impl Source {
    /// The default runtime backend: real OER acquisition (OpenStax). Demo/tests
    /// construct `Source::Mock` explicitly.
    pub fn openstax() -> Self {
        Source::OpenStax(OpenStaxSource::new())
    }

    /// The free/keyless fallback backend (§11.1) — see `wikipedia` module docs.
    pub fn wikipedia() -> Self {
        Source::Wikipedia(WikipediaSource::new())
    }

    /// Searches the backend for sources relevant to `query`, cheaply (no fetch).
    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>, SourceError> {
        match self {
            Source::Mock(m) => m.search(query).await,
            Source::OpenStax(o) => o.search(query).await,
            Source::Wikipedia(w) => w.search(query).await,
        }
    }

    /// Fetches and **normalizes** a hit to the internal representation (§11.1).
    /// The caller then stores it once in the immutable [`Corpus`] and reuses it.
    pub async fn fetch(&self, hit: &SearchHit) -> Result<FetchedSource, SourceError> {
        match self {
            Source::Mock(m) => m.fetch(hit).await,
            Source::OpenStax(o) => o.fetch(hit).await,
            Source::Wikipedia(w) => w.fetch(hit).await,
        }
    }

    /// Re-fetches ignoring whatever first-acquisition cap `fetch` applies
    /// (§11.1 item 6's background completion pass). Backends that never cap
    /// (`Mock`, `Wikipedia` — a whole article is already one fetch) just
    /// delegate to `fetch`; `OpenStax` ignores its configured `max_pages` in
    /// favor of a much larger, still-bounded ceiling.
    pub async fn fetch_complete(&self, hit: &SearchHit) -> Result<FetchedSource, SourceError> {
        match self {
            Source::Mock(m) => m.fetch(hit).await,
            Source::OpenStax(o) => o.fetch_all(hit).await,
            Source::Wikipedia(w) => w.fetch(hit).await,
        }
    }
}

/// Derives a stable corpus id from a title (slug) + a short content hash, so the
/// same source fetched twice lands on the same id and is reused, not duplicated.
pub(crate) fn corpus_id(title: &str, disambiguator: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut slug = String::with_capacity(title.len());
    let mut dash = false;
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
            dash = false;
        } else if !dash && !slug.is_empty() {
            slug.push('-');
            dash = true;
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    hasher.update(b"\0");
    hasher.update(disambiguator.as_bytes());
    let hash = hasher.finalize();
    format!(
        "{slug}-{:x}{:x}{:x}{:x}",
        hash[0], hash[1], hash[2], hash[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_image_urls_finds_remote_srcs_and_ignores_data_uris() {
        let html = r#"<figure><img src="https://openstax.org/fig1.png" alt="a"></figure>
            <img src="http://example.org/fig2.jpg">
            <img src="data:image/png;base64,AAAA">
            <img src="/already/local.png">
            <img src="https://openstax.org/fig1.png">
            <img src="//upload.wikimedia.org/commons/fig3.jpg">"#;
        let mut urls = std::collections::HashSet::new();
        collect_image_urls(html, &mut urls);
        assert_eq!(
            urls,
            std::collections::HashSet::from([
                "https://openstax.org/fig1.png".to_string(),
                "http://example.org/fig2.jpg".to_string(),
                // Protocol-relative (Wikipedia's own form, found live) must
                // be collected too — see `collect_image_urls`'s doc comment.
                "//upload.wikimedia.org/commons/fig3.jpg".to_string(),
            ]),
            "dedupes, keeps http(s) and protocol-relative, ignores data: and already-local src"
        );
    }

    #[test]
    fn rewrite_image_srcs_only_touches_mapped_urls() {
        let html = r#"<figure><img src="https://openstax.org/fig1.png" alt="a"><figcaption>Fig 1</figcaption></figure>
            <img src="https://openstax.org/fig2.png">"#;
        let mut rewrites = std::collections::HashMap::new();
        rewrites.insert(
            "https://openstax.org/fig1.png".to_string(),
            "/api/sources/s1/assets/deadbeef.png".to_string(),
        );
        let rewritten = rewrite_image_srcs(html, &rewrites);
        assert!(rewritten.contains(r#"src="/api/sources/s1/assets/deadbeef.png""#));
        assert!(
            rewritten.contains(r#"src="https://openstax.org/fig2.png""#),
            "an image with no successful download is left pointed at the remote host: {rewritten}"
        );
        assert!(
            rewritten.contains("<figcaption>Fig 1</figcaption>"),
            "surrounding markup survives"
        );
    }

    /// Found live (§11.1 item 7's verification): a protocol-relative `src`
    /// whose download failed (or was never attempted) must NOT survive
    /// as-is — this app is served over plain HTTP (§3), so `//host/path`
    /// resolves to `http://` in the browser and the CSP's
    /// `img-src 'self' data: https:` silently blocks it. Must normalize to
    /// `https:` even with an empty `rewrites` map.
    #[test]
    fn rewrite_image_srcs_normalizes_unresolved_protocol_relative_to_https() {
        let html = r#"<img src="//upload.wikimedia.org/commons/fig.jpg">"#;
        let rewritten = rewrite_image_srcs(html, &std::collections::HashMap::new());
        assert!(
            rewritten.contains(r#"src="https://upload.wikimedia.org/commons/fig.jpg""#),
            "expected an explicit https src: {rewritten}"
        );
    }

    /// Found live (§11.1 item 7's verification) against real OpenStax
    /// content: page HTML links figures as `../resources/<hash>`, relative
    /// to the page's own archive URL — a form `collect_image_urls` never
    /// recognized, so no OpenStax figure was ever localized. This must run
    /// before `collect_image_urls` sees the HTML.
    #[test]
    fn absolutize_relative_image_srcs_resolves_against_page_base() {
        let html = r#"<img src="../resources/deadbeef" alt="fig">
            <img src="https://openstax.org/already/absolute.png">
            <img src="//upload.wikimedia.org/commons/fig.jpg">
            <img src="data:image/png;base64,AAAA">"#;
        let base = "https://openstax.org/apps/archive/20231213.123456/contents/BOOK@1:PAGE.json";
        let out = absolutize_relative_image_srcs(html, base);
        assert!(
            out.contains(
                r#"src="https://openstax.org/apps/archive/20231213.123456/resources/deadbeef""#
            ),
            "relative src resolved against the page's archive base: {out}"
        );
        assert!(out.contains(r#"src="https://openstax.org/already/absolute.png""#));
        assert!(out.contains(r#"src="//upload.wikimedia.org/commons/fig.jpg""#));
        assert!(out.contains(r#"src="data:image/png;base64,AAAA""#));
    }

    #[test]
    fn image_extension_accepts_raster_rejects_svg_and_unknown() {
        assert_eq!(image_extension("image/png"), Some("png"));
        assert_eq!(image_extension("image/jpeg"), Some("jpg"));
        assert_eq!(image_extension("image/gif"), Some("gif"));
        assert_eq!(image_extension("image/webp"), Some("webp"));
        assert_eq!(
            image_extension("image/svg+xml"),
            None,
            "SVG must never be accepted — it can carry an executable <script>"
        );
        assert_eq!(image_extension("text/html"), None);
        assert_eq!(image_extension(""), None);
    }

    #[test]
    fn is_safe_image_host_rejects_loopback_link_local_and_private() {
        let unsafe_urls = [
            "http://localhost/x.png",
            "http://127.0.0.1/x.png",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.5/x.png",
            "http://192.168.1.1/x.png",
            "http://172.16.0.1/x.png",
            "http://[::1]/x.png",
            "ftp://openstax.org/x.png",
            "file:///etc/passwd",
        ];
        for u in unsafe_urls {
            let parsed = reqwest::Url::parse(u).unwrap();
            assert!(!is_safe_image_host(&parsed), "must reject {u}");
        }
        let safe = reqwest::Url::parse("https://openstax.org/fig.png").unwrap();
        assert!(is_safe_image_host(&safe));
    }

    #[tokio::test]
    async fn localize_images_rewrites_only_successfully_downloaded_images() {
        let dir =
            std::env::temp_dir().join(format!("learnive-localize-images-{}", engine_test_id()));
        std::fs::remove_dir_all(&dir).ok();
        let corpus = Corpus::open(&dir).unwrap();
        let mut sections = vec![Section {
            locator: "sec:1".into(),
            title: "Intro".into(),
            text: "text".into(),
            html: r#"<img src="http://127.0.0.1:1/unreachable.png">"#.into(),
        }];
        // A loopback URL is rejected by `is_safe_image_host` before any
        // network attempt, so this exercises the "leave the remote src
        // untouched" path end-to-end without needing a live image host.
        localize_images(&image_client(), &corpus, "s1", &mut sections).await;
        assert!(
            sections[0]
                .html
                .contains("http://127.0.0.1:1/unreachable.png"),
            "unsafe/unreachable image must be left at its original src: {}",
            sections[0].html
        );
        assert!(
            corpus.load_asset("s1", "anything.png").is_err(),
            "nothing should have been stored"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn engine_test_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    /// Live end-to-end against a real, stable image host. Ignored by default
    /// (network); run with `cargo test -p learnive image_localize_live --
    /// --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "hits the network"]
    async fn image_localize_live_downloads_and_rewrites() {
        let dir = std::env::temp_dir().join(format!(
            "learnive-localize-images-live-{}",
            engine_test_id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        let corpus = Corpus::open(&dir).unwrap();
        // A small, stable Wikimedia Commons thumbnail — same host family the
        // `wikipedia` backend already fetches article HTML from.
        // Second image deliberately protocol-relative (`//host/path`) --
        // exactly the form Wikipedia emits (found live, §11.1 item 7's
        // verification), which `collect_image_urls`/`download_and_store`
        // used to silently skip since it isn't `http(s)://`.
        let mut sections = vec![Section {
            locator: "sec:1".into(),
            title: "Intro".into(),
            text: "text".into(),
            html: r#"<p>See <img src="https://upload.wikimedia.org/wikipedia/commons/thumb/4/47/PNG_transparency_demonstration_1.png/120px-PNG_transparency_demonstration_1.png" alt="demo">
                and <img src="//upload.wikimedia.org/wikipedia/commons/thumb/4/47/PNG_transparency_demonstration_1.png/120px-PNG_transparency_demonstration_1.png" alt="demo2"></p>"#.into(),
        }];
        localize_images(&image_client(), &corpus, "live-s1", &mut sections).await;
        println!("rewritten html: {}", sections[0].html);
        assert_eq!(
            sections[0]
                .html
                .matches("/api/sources/live-s1/assets/")
                .count(),
            2,
            "both the absolute and the protocol-relative src must localize: {}",
            sections[0].html
        );
        assert!(!sections[0].html.contains("upload.wikimedia.org"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Advisor-flagged risk: `xmlns` was added as a *generic* attribute, so
    /// it's allowed on every whitelisted tag, not just `<math>`. A foreign
    /// namespace declared on an ordinary HTML element is the classic
    /// content-confusion sanitizer bypass shape — check it doesn't actually
    /// let a script/handler through, and that `annotation-xml` (excluded on
    /// purpose, see `MATHML_TAGS`'s doc comment) stays excluded so nobody
    /// "completes" the MathML tag list later and reopens it.
    #[test]
    fn sanitize_html_xmlns_does_not_smuggle_scripts() {
        let via_div = sanitize_html(
            r#"<div xmlns="http://www.w3.org/2000/svg"><style><img src=1 onerror=alert(1)></style></div>"#,
        );
        assert!(
            !via_div.contains("onerror"),
            "handler smuggled via div/svg xmlns: {via_div}"
        );
        assert!(
            !via_div.to_lowercase().contains("<style"),
            "style tag smuggled: {via_div}"
        );

        let via_annotation = sanitize_html(
            r#"<math><annotation-xml encoding="text/html"><img src=1 onerror=alert(1)></annotation-xml></math>"#,
        );
        assert!(
            !via_annotation.contains("annotation-xml"),
            "annotation-xml must stay outside the whitelist: {via_annotation}"
        );
        assert!(
            !via_annotation.contains("onerror"),
            "handler smuggled via annotation-xml: {via_annotation}"
        );
    }

    #[test]
    fn sanitize_html_drops_script_but_keeps_structural_whitelist() {
        let html = r#"<script>alert('xss')</script>
            <h2>Limits</h2>
            <p onclick="evil()">A <strong>limit</strong> describes a value.</p>
            <ul><li>First</li><li>Second</li></ul>
            <table><tr><td>1</td></tr></table>
            <figure><img src="https://openstax.org/fig.png" alt="graph"><figcaption>Fig 1</figcaption></figure>
            <code>x + 1</code>
            <sub>n</sub><sup>2</sup>"#;
        let clean = sanitize_html(html);
        assert!(
            !clean.contains("<script"),
            "script tag must be removed: {clean}"
        );
        assert!(
            !clean.contains("alert"),
            "script content must be removed: {clean}"
        );
        assert!(
            !clean.contains("onclick"),
            "event handler attribute must be stripped: {clean}"
        );
        assert!(clean.contains("<h2>Limits</h2>"));
        assert!(clean.contains("<strong>limit</strong>"));
        assert!(clean.contains("<li>First</li>"));
        assert!(clean.contains("<table>"));
        assert!(clean.contains("<figure>") && clean.contains("<figcaption>Fig 1</figcaption>"));
        assert!(
            clean.contains(r#"src="https://openstax.org/fig.png""#),
            "img src kept until asset download lands: {clean}"
        );
        assert!(clean.contains("<code>x + 1</code>"));
        assert!(clean.contains("<sub>n</sub>") && clean.contains("<sup>2</sup>"));
    }

    #[test]
    fn sanitize_html_keeps_mathml_subtree() {
        let html = r#"<p>The area is <math xmlns="http://www.w3.org/1998/Math/MathML">
            <mrow><mi>A</mi><mo>=</mo><msup><mi>r</mi><mn>2</mn></msup></mrow>
            </math>.</p>"#;
        let clean = sanitize_html(html);
        assert!(clean.contains("<math"));
        assert!(clean.contains("<msup>"));
        assert!(clean.contains("<mi>r</mi>"));
        assert!(clean.contains("<mn>2</mn>"));
    }

    #[test]
    fn corpus_id_is_slug_plus_stable_hash() {
        let a = corpus_id("Calculus, Volume 1", "openstax");
        let b = corpus_id("Calculus, Volume 1", "openstax");
        assert_eq!(a, b, "same input → same id (reuse, not duplicate)");
        assert!(a.starts_with("calculus-volume-1-"));
        let c = corpus_id("Calculus, Volume 1", "libretexts");
        assert_ne!(a, c, "disambiguator changes the id");
    }

    #[test]
    fn corpus_id_handles_unicode_and_symbols() {
        let id = corpus_id("Naïve Café Résumé & Exposé!!!", "x");
        assert!(!id.starts_with('-') && !id.contains("--"));
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
