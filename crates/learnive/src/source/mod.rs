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
/// pointed at the remote host for now — downloading figures into the corpus
/// is a separate follow-up (§11.1 item 5); dropping the attribute today would
/// make any source where the figure IS the content (physics, geometry)
/// useless in the reader before that lands.
pub(crate) fn sanitize_html(html: &str) -> String {
    ammonia::Builder::default()
        .add_tags(MATHML_TAGS)
        .add_generic_attributes(["mathvariant", "display", "xmlns", "columnalign", "rowalign"])
        .clean(html)
        .to_string()
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
