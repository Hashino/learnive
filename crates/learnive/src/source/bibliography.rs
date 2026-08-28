//! Online bibliographic existence verification (§11.1, PLAN.md S27d).
//!
//! Before a proposed reading-list item is worth acting on at all, this
//! module answers one narrow question against public bibliographic catalogs:
//! **does something like this exist?** Not "is this exact edition correct" —
//! that stronger question is [`super::acervo::validate_acervo`]'s job (S27c,
//! already built), which runs against a *real file* the user actually has.
//! PLAN.md's own words for the split: *"o portão do acervo é um segundo
//! filtro, muito mais forte"* — this check only has to be lenient because a
//! hallucinated or poorly-matched proposal still has to survive the acervo
//! gate before it can ground anything.
//!
//! **Calibration is the decision that matters.** The error asymmetry points
//! one way: a false positive (passing a slightly-wrong record — wrong
//! edition, translated title) is cheap, caught downstream by the acervo
//! gate. A false negative is expensive — it silently drops a good, real
//! textbook because it's poorly indexed (subtitle variance, an old edition,
//! a translation) and replaces it with a worse substitute. So matching here
//! uses the same normalized comparison
//! ([`super::matching::normalize`]/[`super::matching::primary_title`]/
//! [`super::matching::surname_of`], shared with the acervo gate) at a
//! deliberately loose bar: title **and** at least one author's surname,
//! either direction of containment, and **multiple plausible results all
//! count as verified** — a catalog's top-ranked hit is not trusted blindly
//! (a live OpenAlex query for a well-known paper's title returned a
//! completely unrelated work as its #1 "relevance" hit during development;
//! see the module's live tests).
//!
//! **The identifier is never the search key.** [`ProposedItem::identifier`]
//! (ISBN/DOI/arXiv id) is model-emitted alongside title/authors and can be
//! hallucinated exactly like the rest — worse, a hallucinated identifier that
//! happens to *resolve* to a different real work is the silent-failure case,
//! not the loud one. So even the identifier-routed lookups (DOI → Crossref
//! `/works/{doi}`, arXiv id → the arXiv API) still run the same normalized
//! title+author comparison against whatever record the identifier resolves
//! to, rather than treating "the identifier resolved" as verification by
//! itself.
//!
//! **Routing** ([`verification_plan`], pure, no I/O — unit-tested directly):
//! book → OpenLibrary, then Google Books as a second attempt; article with a
//! DOI → Crossref `/works/{doi}`; article with an arXiv id → the arXiv API;
//! article with neither → Crossref bibliographic search, then OpenAlex.
//!
//! **A catalog being down does not block anything.** [`BibliographyClient::verify`]
//! walks the plan, tries every catalog even after one fails, and only
//! returns [`VerificationOutcome::Unavailable`] when *none* of them could be
//! reached at all — degrading to "unverified", never blocking, exactly
//! because the acervo gate is the real, structural safety net (a
//! hallucinated proposal can't become content without a real PDF, regardless
//! of what this check said).
//!
//! **Caching** ([`BibliographyCache`]) lives alongside the acervo gate's own
//! index cache (`<data>/index/library/`, content-hash-keyed) under its own
//! subpath, `<data>/index/bibliography/` — same `<data>/index/` root, so the
//! two never collide. Keyed by a hash of the *proposed item's* normalized
//! title+authors+kind (there is no real file yet at this point, unlike the
//! acervo cache's content hash). Only [`VerificationOutcome::Verified`]/
//! [`VerificationOutcome::NotFound`] are cached — `Unavailable` is a
//! transient network condition, not a durable fact about the work, so
//! caching it would freeze a real book into "unverified" forever the first
//! time a catalog happened to be down.
//!
//! **Privacy**: Crossref/OpenAlex's "polite pool" (better rate limits) is
//! requested via a static [`USER_AGENT`] identifying the app, never a
//! per-user email — see that constant's doc comment.
//!
//! No caller yet (same pattern as S27a/b/c): the LLM call site that produces
//! a [`ProposedItem`] is S27e, and wiring this into cold start is S27f/g+.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::SourceKind;
use super::matching::{normalize, primary_title, surname_of};

/// Identifies the app to Crossref/OpenAlex's "polite pool" (better rate
/// limits in exchange for *an* identifying contact — their docs suggest an
/// email). **Deliberately not an email**: sending the user's own address to
/// a third party without their consent isn't worth a little rate-limit
/// headroom (user decision, PLAN.md S27d) — a static string identifying the
/// app is the whole ask, and the "common pool" (no identification at all) is
/// an acceptable fallback if a catalog ever rejects this.
const USER_AGENT: &str = "learnive/0.1 (local self-hosted learning app)";

/// A cross-check identifier the model may emit alongside title/authors.
/// **Never the search key** — see the module doc's "identifier is never the
/// search key" section. Kept as a tagged enum (not three `Option<String>`
/// fields) so a caller can't accidentally populate more than one at once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Identifier {
    Isbn(String),
    Doi(String),
    Arxiv(String),
}

/// The model-emitted shape this module consumes: structured fields, not
/// prose (SPEC's own instruction — "o modelo emite campos estruturados").
/// Produced by a later slice's LLM call site (S27e); this module only
/// defines the shape and the verification logic that consumes it.
///
/// `Eq` (added alongside S27e, which embeds this inside
/// `engine::SourcePointer`/`OutlineItem`): every field here was already
/// `Eq`-capable (`Option<u32>`, `Option<String>`, `Option<Identifier>` —
/// `Identifier` itself derives `Eq` — and `SourceKind`); the original
/// `PartialEq`-only derive was incidental, not a deliberate exclusion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedItem {
    pub title: String,
    /// "First Last" order assumed, same as [`super::acervo::ExpectedItem::authors`]
    /// — good enough for the surname heuristic, not a bibliography formatter.
    pub authors: Vec<String>,
    #[serde(default)]
    pub year: Option<u32>,
    #[serde(default)]
    pub edition: Option<String>,
    #[serde(default)]
    pub identifier: Option<Identifier>,
    pub kind: SourceKind,
}

/// Which catalog a routing decision selects — see [`verification_plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Catalog {
    OpenLibrary,
    GoogleBooks,
    Crossref,
    Arxiv,
    OpenAlex,
}

/// Three distinct outcomes, deliberately not a boolean — a caller (S27f+)
/// must treat "the catalogs were unreachable" completely differently from
/// "we asked, and this genuinely doesn't seem to exist" (module doc).
///
/// `Eq` (added alongside S27e, same reasoning as [`ProposedItem`]'s: every
/// field — `Catalog`, `String`, `Vec<String>` — was already `Eq`-capable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum VerificationOutcome {
    /// A plausible match was found in at least one catalog.
    Verified {
        catalog: Catalog,
        /// The matching candidate's own title, as the catalog returned it —
        /// useful for a later UI to show *what* matched, not just that
        /// something did.
        matched_title: String,
    },
    /// Every catalog in the plan was reachable and none returned a
    /// plausible match — the lenient "does something like this exist?"
    /// question was actually asked and answered no.
    NotFound,
    /// No catalog in the plan could be reached at all. Degrades — does not
    /// block (module doc) — and is never cached (see [`BibliographyCache`]'s
    /// doc).
    Unavailable { errors: Vec<String> },
}

/// Pure routing decision, no I/O (unit-tested directly, independent of the
/// network calls in [`BibliographyClient`]) — SPEC's documented order: book →
/// OpenLibrary then Google Books; article with a DOI → Crossref; article with
/// an arXiv id → the arXiv API; article with neither → Crossref then
/// OpenAlex.
pub fn verification_plan(item: &ProposedItem) -> Vec<Catalog> {
    match item.kind {
        SourceKind::Book => vec![Catalog::OpenLibrary, Catalog::GoogleBooks],
        SourceKind::Article => match &item.identifier {
            Some(Identifier::Arxiv(_)) => vec![Catalog::Arxiv],
            Some(Identifier::Doi(_)) => vec![Catalog::Crossref],
            _ => vec![Catalog::Crossref, Catalog::OpenAlex],
        },
    }
}

/// One candidate record as returned by a catalog — just enough to run the
/// normalized title+author comparison, not a full bibliographic record.
#[derive(Debug, Clone)]
struct CatalogCandidate {
    title: String,
    authors: Vec<String>,
}

/// The lenient normalized-comparison rule (module doc): title matches either
/// direction of containment (a catalog's title may carry a subtitle the
/// proposal dropped, or vice versa) **and**, when the proposal names any
/// authors, at least one surname must appear among the candidate's authors.
/// A proposal with no authors at all passes on title alone — SPEC's own
/// wording only requires "at least one author's surname" when one is given.
fn plausible_match(item: &ProposedItem, candidate: &CatalogCandidate) -> bool {
    let target_title = normalize(primary_title(&item.title));
    if target_title.is_empty() {
        return false;
    }
    let candidate_title = normalize(primary_title(&candidate.title));
    if candidate_title.is_empty() {
        return false;
    }
    let title_matches =
        candidate_title.contains(&target_title) || target_title.contains(&candidate_title);
    if !title_matches {
        return false;
    }
    if item.authors.is_empty() {
        return true;
    }
    let candidate_haystack = normalize(&candidate.authors.join(" "));
    item.authors.iter().any(|a| {
        let surname = normalize(surname_of(a));
        !surname.is_empty() && candidate_haystack.contains(&surname)
    })
}

fn net_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// The real HTTP client — one `reqwest::Client` reused across catalogs (same
/// idiom as [`super::libgen::LibGenSource`]), identifying itself via
/// [`USER_AGENT`] to every catalog, not just the ones that ask for it.
pub struct BibliographyClient {
    http: reqwest::Client,
}

impl Default for BibliographyClient {
    fn default() -> Self {
        Self::new()
    }
}

impl BibliographyClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { http }
    }

    /// Walks [`verification_plan`]'s catalog list in order, trying every
    /// entry even after one fails (module doc: a catalog being down must not
    /// block). Returns on the first plausible match; otherwise `NotFound`
    /// once at least one catalog answered cleanly, or `Unavailable` if none
    /// could be reached at all.
    pub async fn verify(&self, item: &ProposedItem) -> VerificationOutcome {
        let plan = verification_plan(item);
        let mut reached_any = false;
        let mut errors = Vec::new();
        for catalog in plan {
            match self.query(catalog, item).await {
                Ok(candidates) => {
                    reached_any = true;
                    if let Some(hit) = candidates.iter().find(|c| plausible_match(item, c)) {
                        return VerificationOutcome::Verified {
                            catalog,
                            matched_title: hit.title.clone(),
                        };
                    }
                }
                Err(e) => errors.push(format!("{catalog:?}: {e}")),
            }
        }
        if reached_any {
            VerificationOutcome::NotFound
        } else {
            VerificationOutcome::Unavailable { errors }
        }
    }

    async fn query(
        &self,
        catalog: Catalog,
        item: &ProposedItem,
    ) -> Result<Vec<CatalogCandidate>, String> {
        match catalog {
            Catalog::OpenLibrary => self.query_open_library(item).await,
            Catalog::GoogleBooks => self.query_google_books(item).await,
            Catalog::Crossref => self.query_crossref(item).await,
            Catalog::Arxiv => self.query_arxiv(item).await,
            Catalog::OpenAlex => self.query_open_alex(item).await,
        }
    }

    /// `GET https://openlibrary.org/search.json?title=&author=` — no key
    /// needed. Probed live during development (see this module's `#[ignore]`d
    /// tests): `docs[].title` / `docs[].author_name` carry what's needed.
    async fn query_open_library(
        &self,
        item: &ProposedItem,
    ) -> Result<Vec<CatalogCandidate>, String> {
        let title = primary_title(&item.title);
        let mut query = vec![("title", title.to_string()), ("limit", "5".to_string())];
        if let Some(author) = item.authors.first() {
            query.push(("author", author.clone()));
        }
        let resp = self
            .http
            .get("https://openlibrary.org/search.json")
            .query(&query)
            .send()
            .await
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(format!("openlibrary HTTP {}", resp.status()));
        }
        let body: OpenLibraryResponse = resp.json().await.map_err(net_err)?;
        Ok(body
            .docs
            .into_iter()
            .map(|d| CatalogCandidate {
                title: d.title,
                authors: d.author_name.unwrap_or_default(),
            })
            .collect())
    }

    /// `GET https://www.googleapis.com/books/v1/volumes?q=intitle:...` — no
    /// key needed for a light query volume (probed live; a shared quota can
    /// return HTTP 429 across unrelated callers, which surfaces here as a
    /// normal catalog-unreachable error, not a special case).
    async fn query_google_books(
        &self,
        item: &ProposedItem,
    ) -> Result<Vec<CatalogCandidate>, String> {
        let title = primary_title(&item.title);
        let mut q = format!("intitle:{title}");
        if let Some(author) = item.authors.first() {
            q.push_str(&format!(" inauthor:{author}"));
        }
        let resp = self
            .http
            .get("https://www.googleapis.com/books/v1/volumes")
            .query(&[("q", q.as_str())])
            .send()
            .await
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(format!("google books HTTP {}", resp.status()));
        }
        let body: GoogleBooksResponse = resp.json().await.map_err(net_err)?;
        Ok(body
            .items
            .into_iter()
            .filter_map(|i| i.volume_info)
            .map(|vi| CatalogCandidate {
                title: vi.title.unwrap_or_default(),
                authors: vi.authors.unwrap_or_default(),
            })
            .collect())
    }

    /// Article with a DOI: `GET https://api.crossref.org/works/{doi}` (direct
    /// record fetch — the DOI's `prefix/suffix` slash goes straight into the
    /// path, matching Crossref's own content-negotiation convention, probed
    /// live). Article with no identifier: `GET .../works?query.bibliographic=`
    /// (real search, so every returned item is checked, not just the first —
    /// module doc's "top hit isn't trusted blindly").
    async fn query_crossref(&self, item: &ProposedItem) -> Result<Vec<CatalogCandidate>, String> {
        if let Some(Identifier::Doi(doi)) = &item.identifier {
            let url = format!("https://api.crossref.org/works/{doi}");
            let resp = self.http.get(&url).send().await.map_err(net_err)?;
            if !resp.status().is_success() {
                return Err(format!("crossref HTTP {}", resp.status()));
            }
            let body: CrossrefWorkResponse = resp.json().await.map_err(net_err)?;
            return Ok(vec![crossref_work_to_candidate(&body.message)]);
        }
        let resp = self
            .http
            .get("https://api.crossref.org/works")
            .query(&[("query.bibliographic", item.title.as_str()), ("rows", "5")])
            .send()
            .await
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(format!("crossref HTTP {}", resp.status()));
        }
        let body: CrossrefSearchResponse = resp.json().await.map_err(net_err)?;
        Ok(body
            .message
            .items
            .iter()
            .map(crossref_work_to_candidate)
            .collect())
    }

    /// Article with an arXiv id: `GET https://export.arxiv.org/api/query?id_list=`
    /// — an Atom feed (probed live; `http://` 301s to `https://`, so this
    /// calls the secure host directly). Even though the identifier routed
    /// here, the resolved entry's title+author are still compared against the
    /// proposal (module doc: identifier resolving is not verification by
    /// itself).
    async fn query_arxiv(&self, item: &ProposedItem) -> Result<Vec<CatalogCandidate>, String> {
        // Unreachable via `verification_plan` today (only routed to when the
        // identifier is `Arxiv`), but `Err` here — not `Ok(empty)` — matters:
        // an empty candidate list reads as "reached the catalog, found
        // nothing", which folds into `NotFound`, the expensive false-negative
        // outcome the whole module is calibrated against (module doc). If a
        // future routing change or a direct caller ever reaches this without
        // an arXiv id, it must degrade to `Unavailable`, not silently claim
        // "checked, not found".
        let Some(Identifier::Arxiv(id)) = &item.identifier else {
            return Err("arxiv route reached without an arxiv identifier".to_string());
        };
        let resp = self
            .http
            .get("https://export.arxiv.org/api/query")
            .query(&[("id_list", id.as_str())])
            .send()
            .await
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(format!("arxiv HTTP {}", resp.status()));
        }
        let text = resp.text().await.map_err(net_err)?;
        parse_arxiv_feed(&text)
    }

    /// `GET https://api.openalex.org/works?search=` — no key needed; the
    /// "polite pool" is requested via [`USER_AGENT`], never a user email
    /// (module doc). Probed live: a bibliographic-title search's top hit was
    /// observed to be an unrelated work during development, which is exactly
    /// why every returned candidate is checked, not just the first.
    async fn query_open_alex(&self, item: &ProposedItem) -> Result<Vec<CatalogCandidate>, String> {
        let resp = self
            .http
            .get("https://api.openalex.org/works")
            .query(&[("search", item.title.as_str()), ("per_page", "5")])
            .send()
            .await
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(format!("openalex HTTP {}", resp.status()));
        }
        let body: OpenAlexResponse = resp.json().await.map_err(net_err)?;
        Ok(body
            .results
            .into_iter()
            .map(|w| CatalogCandidate {
                title: w.title.unwrap_or_default(),
                authors: w
                    .authorships
                    .into_iter()
                    .filter_map(|a| a.author.display_name)
                    .collect(),
            })
            .collect())
    }
}

fn crossref_work_to_candidate(work: &CrossrefWork) -> CatalogCandidate {
    CatalogCandidate {
        title: work.title.first().cloned().unwrap_or_default(),
        authors: work
            .author
            .iter()
            .map(|a| format!("{} {}", a.given, a.family).trim().to_string())
            .collect(),
    }
}

fn parse_arxiv_feed(xml: &str) -> Result<Vec<CatalogCandidate>, String> {
    let feed: ArxivFeed = quick_xml::de::from_str(xml).map_err(|e| e.to_string())?;
    Ok(feed
        .entries
        .into_iter()
        .map(|e| CatalogCandidate {
            title: e.title.split_whitespace().collect::<Vec<_>>().join(" "),
            authors: e.authors.into_iter().map(|a| a.name).collect(),
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct OpenLibraryResponse {
    #[serde(default)]
    docs: Vec<OpenLibraryDoc>,
}

#[derive(Debug, Deserialize)]
struct OpenLibraryDoc {
    #[serde(default)]
    title: String,
    #[serde(default)]
    author_name: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct GoogleBooksResponse {
    #[serde(default)]
    items: Vec<GoogleBooksItem>,
}

#[derive(Debug, Deserialize)]
struct GoogleBooksItem {
    #[serde(default, rename = "volumeInfo")]
    volume_info: Option<GoogleBooksVolumeInfo>,
}

#[derive(Debug, Deserialize)]
struct GoogleBooksVolumeInfo {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    authors: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct CrossrefWorkResponse {
    message: CrossrefWork,
}

#[derive(Debug, Deserialize)]
struct CrossrefSearchResponse {
    message: CrossrefSearchMessage,
}

#[derive(Debug, Deserialize)]
struct CrossrefSearchMessage {
    #[serde(default)]
    items: Vec<CrossrefWork>,
}

#[derive(Debug, Deserialize)]
struct CrossrefWork {
    #[serde(default)]
    title: Vec<String>,
    #[serde(default)]
    author: Vec<CrossrefAuthor>,
}

#[derive(Debug, Deserialize)]
struct CrossrefAuthor {
    #[serde(default)]
    given: String,
    #[serde(default)]
    family: String,
}

#[derive(Debug, Deserialize)]
struct ArxivFeed {
    #[serde(rename = "entry", default)]
    entries: Vec<ArxivEntry>,
}

#[derive(Debug, Deserialize)]
struct ArxivEntry {
    title: String,
    #[serde(rename = "author", default)]
    authors: Vec<ArxivAuthor>,
}

#[derive(Debug, Deserialize)]
struct ArxivAuthor {
    name: String,
}

#[derive(Debug, Deserialize)]
struct OpenAlexResponse {
    #[serde(default)]
    results: Vec<OpenAlexWork>,
}

#[derive(Debug, Deserialize)]
struct OpenAlexWork {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    authorships: Vec<OpenAlexAuthorship>,
}

#[derive(Debug, Deserialize)]
struct OpenAlexAuthorship {
    author: OpenAlexAuthor,
}

#[derive(Debug, Deserialize)]
struct OpenAlexAuthor {
    #[serde(default)]
    display_name: Option<String>,
}

/// Global, per-library cache of verification outcomes (module doc): keyed by
/// a hash of the *proposed item's* normalized title+authors+kind, not a file
/// content hash (there is no real file yet at this point) — stored alongside
/// the acervo gate's own index cache (`<data>/index/library/`) under its own
/// subpath so the two never collide.
pub struct BibliographyCache {
    dir: PathBuf,
}

impl BibliographyCache {
    /// Opens (creating if needed) `<data>/index/bibliography/`, mirroring
    /// [`super::acervo::validate_acervo`]'s `<data>/index/library/` convention
    /// — same `<data>/index/` root, a separate subpath.
    pub fn open(data_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = data_dir.as_ref().join("index").join("bibliography");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn get(&self, item: &ProposedItem) -> Option<VerificationOutcome> {
        let bytes = fs::read(self.path_for(item)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Persists an outcome, atomically (tmp file + rename, same idiom as
    /// [`super::acervo::build_index_cache`]). Callers should not persist
    /// [`VerificationOutcome::Unavailable`] — see [`verify_bibliography`],
    /// the orchestration function that enforces this — but `put` itself
    /// stays a plain unconditional write so a test can exercise the
    /// round-trip directly.
    pub fn put(&self, item: &ProposedItem, outcome: &VerificationOutcome) -> std::io::Result<()> {
        let path = self.path_for(item);
        let json = serde_json::to_vec_pretty(outcome)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn path_for(&self, item: &ProposedItem) -> PathBuf {
        self.dir.join(format!("{}.json", cache_key(item)))
    }
}

/// SHA-256 of the proposed item's normalized title + sorted normalized author
/// surnames + kind — stable across re-proposals of the same work with
/// authors listed in a different order, but distinct across different works
/// (including two different works that happen to share a title, since the
/// author surnames differ).
fn cache_key(item: &ProposedItem) -> String {
    use sha2::{Digest, Sha256};
    let mut surnames: Vec<String> = item
        .authors
        .iter()
        .map(|a| normalize(surname_of(a)))
        .collect();
    surnames.sort();
    let mut hasher = Sha256::new();
    hasher.update(normalize(primary_title(&item.title)).as_bytes());
    hasher.update(b"\0");
    hasher.update(surnames.join(",").as_bytes());
    hasher.update(b"\0");
    hasher.update(format!("{:?}", item.kind).as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Top-level entry point (module doc): checks the cache first, otherwise
/// performs the real catalog round-trips and persists a fresh
/// `Verified`/`NotFound` result (never `Unavailable` — see
/// [`BibliographyCache`]'s doc on why a transient outage must not be
/// persisted as a durable fact about the work).
pub async fn verify_bibliography(
    client: &BibliographyClient,
    cache: &BibliographyCache,
    item: &ProposedItem,
) -> VerificationOutcome {
    if let Some(cached) = cache.get(item) {
        return cached;
    }
    let outcome = client.verify(item).await;
    if !matches!(outcome, VerificationOutcome::Unavailable { .. }) {
        // Best-effort: a cache write failure must not turn a successful
        // catalog round-trip into a failed verification.
        let _ = cache.put(item, &outcome);
    }
    outcome
}

#[cfg(test)]
impl BibliographyClient {
    /// A client that fails fast against every real catalog host (1ms
    /// connect/response timeout) — lets a test prove
    /// [`verify_bibliography`]'s cache-first ordering without touching the
    /// network for real: if the cache is consulted first, this client is
    /// never even invoked; if the cache-first check were ever broken, this
    /// client turns that into a fast, deterministic `Unavailable` instead of
    /// a slow or flaky real round-trip.
    fn unreachable_for_test() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_millis(1))
            .build()
            .unwrap_or_default();
        Self { http }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(title: &str, authors: &[&str]) -> ProposedItem {
        ProposedItem {
            title: title.to_string(),
            authors: authors.iter().map(|a| a.to_string()).collect(),
            year: None,
            edition: None,
            identifier: None,
            kind: SourceKind::Book,
        }
    }

    fn article(title: &str, authors: &[&str], identifier: Option<Identifier>) -> ProposedItem {
        ProposedItem {
            title: title.to_string(),
            authors: authors.iter().map(|a| a.to_string()).collect(),
            year: None,
            edition: None,
            identifier,
            kind: SourceKind::Article,
        }
    }

    fn candidate(title: &str, authors: &[&str]) -> CatalogCandidate {
        CatalogCandidate {
            title: title.to_string(),
            authors: authors.iter().map(|a| a.to_string()).collect(),
        }
    }

    // -- Routing ------------------------------------------------------

    #[test]
    fn book_routes_to_open_library_then_google_books() {
        let item = book(
            "Introduction to the Theory of Computation",
            &["Michael Sipser"],
        );
        assert_eq!(
            verification_plan(&item),
            vec![Catalog::OpenLibrary, Catalog::GoogleBooks]
        );
    }

    #[test]
    fn book_routing_is_unaffected_by_an_isbn_identifier() {
        // The identifier is cross-check confirmation, never the search key
        // (module doc) — it must not change which catalogs get tried.
        let mut item = book("Some Book", &["Some Author"]);
        item.identifier = Some(Identifier::Isbn("9780262533058".into()));
        assert_eq!(
            verification_plan(&item),
            vec![Catalog::OpenLibrary, Catalog::GoogleBooks]
        );
    }

    #[test]
    fn article_with_doi_routes_to_crossref_only() {
        let item = article(
            "Attention Is All You Need",
            &["Ashish Vaswani"],
            Some(Identifier::Doi("10.48550/arXiv.1706.03762".into())),
        );
        assert_eq!(verification_plan(&item), vec![Catalog::Crossref]);
    }

    #[test]
    fn article_with_arxiv_id_routes_to_arxiv_only() {
        let item = article(
            "Attention Is All You Need",
            &["Ashish Vaswani"],
            Some(Identifier::Arxiv("1706.03762".into())),
        );
        assert_eq!(verification_plan(&item), vec![Catalog::Arxiv]);
    }

    #[test]
    fn article_with_no_identifier_routes_to_crossref_then_openalex() {
        let item = article("Some Paper", &["Some Author"], None);
        assert_eq!(
            verification_plan(&item),
            vec![Catalog::Crossref, Catalog::OpenAlex]
        );
    }

    #[test]
    fn article_with_isbn_still_uses_the_no_identifier_article_route() {
        // ISBN is a book identifier; an article carrying one (a model
        // mistake) must not be routed to a book catalog or dropped —
        // it falls through to the identifier-less article route.
        let item = article(
            "Some Paper",
            &["Some Author"],
            Some(Identifier::Isbn("0000000000".into())),
        );
        assert_eq!(
            verification_plan(&item),
            vec![Catalog::Crossref, Catalog::OpenAlex]
        );
    }

    // -- Lenient matching -----------------------------------------------

    #[test]
    fn matches_on_normalized_title_and_author_surname() {
        let item = book(
            "Introduction to the Theory of Computation",
            &["Michael Sipser"],
        );
        let cand = candidate(
            "Introduction to the Theory of Computation, 3rd Edition",
            &["Sipser, Michael"],
        );
        assert!(plausible_match(&item, &cand));
    }

    #[test]
    fn matches_despite_subtitle_and_punctuation_variance() {
        let item = book("Calculus: An Intuitive Approach", &["Jane Doe"]);
        let cand = candidate("Calculus, An Intuitive Approach!!", &["Jane Q. Doe"]);
        assert!(plausible_match(&item, &cand));
    }

    #[test]
    fn does_not_match_when_the_author_is_genuinely_different() {
        let item = book(
            "Introduction to the Theory of Computation",
            &["Michael Sipser"],
        );
        let cand = candidate(
            "Introduction to the Theory of Computation",
            &["Someone Else"],
        );
        assert!(!plausible_match(&item, &cand));
    }

    #[test]
    fn does_not_match_when_the_title_is_genuinely_different() {
        let item = book(
            "Introduction to the Theory of Computation",
            &["Michael Sipser"],
        );
        let cand = candidate("A Completely Unrelated Cookbook", &["Michael Sipser"]);
        assert!(!plausible_match(&item, &cand));
    }

    #[test]
    fn title_alone_is_enough_when_the_proposal_names_no_authors() {
        let item = book("Some Obscure Title With No Known Author", &[]);
        let cand = candidate("Some Obscure Title With No Known Author", &["Anyone"]);
        assert!(plausible_match(&item, &cand));
    }

    // -- Arxiv feed parsing (real response shape, captured 2026-08-27) --

    const ARXIV_FEED_FIXTURE: &str = r#"<?xml version='1.0' encoding='UTF-8'?>
<feed xmlns:opensearch="http://a9.com/-/spec/opensearch/1.1/" xmlns="http://www.w3.org/2005/Atom">
  <id>https://arxiv.org/api/abc</id>
  <title>arXiv Query</title>
  <updated>2026-08-27T21:18:42Z</updated>
  <opensearch:itemsPerPage>10</opensearch:itemsPerPage>
  <opensearch:totalResults>1</opensearch:totalResults>
  <opensearch:startIndex>0</opensearch:startIndex>
  <entry>
    <id>http://arxiv.org/abs/1706.03762v7</id>
    <title>Attention Is All You Need</title>
    <updated>2023-08-02T00:41:18Z</updated>
    <published>2017-06-12T17:57:34Z</published>
    <author>
      <name>Ashish Vaswani</name>
    </author>
    <author>
      <name>Noam Shazeer</name>
    </author>
  </entry>
</feed>"#;

    #[test]
    fn parses_a_real_arxiv_atom_feed_shape() {
        let candidates = parse_arxiv_feed(ARXIV_FEED_FIXTURE).expect("should parse");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].title, "Attention Is All You Need");
        assert_eq!(
            candidates[0].authors,
            vec!["Ashish Vaswani".to_string(), "Noam Shazeer".to_string()]
        );
    }

    #[test]
    fn parses_an_empty_arxiv_feed_with_no_entries() {
        let empty = r#"<?xml version='1.0' encoding='UTF-8'?>
<feed xmlns:opensearch="http://a9.com/-/spec/opensearch/1.1/" xmlns="http://www.w3.org/2005/Atom">
  <id>https://arxiv.org/api/abc</id>
  <title>arXiv Query</title>
  <updated>2026-08-27T21:18:42Z</updated>
</feed>"#;
        let candidates = parse_arxiv_feed(empty).expect("should parse");
        assert!(candidates.is_empty());
    }

    // -- Caching ----------------------------------------------------------

    #[test]
    fn cache_round_trips_a_verified_outcome() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = BibliographyCache::open(tmp.path()).expect("open cache");
        let item = book(
            "Introduction to the Theory of Computation",
            &["Michael Sipser"],
        );

        assert!(cache.get(&item).is_none(), "nothing cached yet");

        let outcome = VerificationOutcome::Verified {
            catalog: Catalog::OpenLibrary,
            matched_title: "Introduction to the Theory of Computation".into(),
        };
        cache.put(&item, &outcome).expect("cache write");

        assert_eq!(cache.get(&item), Some(outcome));
    }

    #[test]
    fn cache_round_trips_a_not_found_outcome() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = BibliographyCache::open(tmp.path()).expect("open cache");
        let item = book("A Book That Genuinely Does Not Exist Anywhere", &["Nobody"]);

        cache
            .put(&item, &VerificationOutcome::NotFound)
            .expect("cache write");
        assert_eq!(cache.get(&item), Some(VerificationOutcome::NotFound));
    }

    #[test]
    fn cache_key_is_stable_across_author_order_but_distinct_across_works() {
        let a = book("Some Title", &["Alice Author", "Bob Author"]);
        let b = book("Some Title", &["Bob Author", "Alice Author"]);
        assert_eq!(cache_key(&a), cache_key(&b), "author order must not matter");

        let different = book("Some Title", &["Carol Author"]);
        assert_ne!(cache_key(&a), cache_key(&different));

        let different_kind = article("Some Title", &["Alice Author", "Bob Author"], None);
        assert_ne!(
            cache_key(&a),
            cache_key(&different_kind),
            "kind must be part of the cache key"
        );
    }

    #[test]
    fn cache_is_reused_alongside_the_acervo_index_cache_layout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = BibliographyCache::open(tmp.path()).expect("open cache");
        assert!(tmp.path().join("index").join("bibliography").is_dir());
        // Same `<data>/index/` root the acervo gate's own cache uses
        // (`<data>/index/library/`), a separate subpath.
        drop(cache);
        std::fs::create_dir_all(tmp.path().join("index").join("library")).unwrap();
        assert!(tmp.path().join("index").join("bibliography").is_dir());
        assert!(tmp.path().join("index").join("library").is_dir());
    }

    // -- Top-level orchestration: cache-first ordering ---------------------

    #[tokio::test]
    async fn verify_bibliography_returns_the_cached_outcome_without_touching_the_network() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = BibliographyCache::open(tmp.path()).expect("open cache");
        let item = book(
            "Introduction to the Theory of Computation",
            &["Michael Sipser"],
        );
        let outcome = VerificationOutcome::Verified {
            catalog: Catalog::OpenLibrary,
            matched_title: "Introduction to the Theory of Computation".into(),
        };
        cache.put(&item, &outcome).expect("cache write");

        // A client that cannot reach any real catalog within 1ms — if
        // `verify_bibliography` consulted it before the cache, this would
        // come back `Unavailable`, not the cached `Verified`.
        let client = BibliographyClient::unreachable_for_test();
        let result = verify_bibliography(&client, &cache, &item).await;
        assert_eq!(
            result, outcome,
            "a cache hit must short-circuit before any network call"
        );
    }

    // -- Live catalog round-trips ------------------------------------------
    //
    // These hit real, public, keyless bibliographic APIs. Not part of the
    // normal `cargo test` run — run manually with:
    //   cargo test -p learnive --bin learnive source::bibliography::tests::live -- --ignored --test-threads=1

    #[tokio::test]
    #[ignore = "hits a real external API; run manually, see doc comment"]
    async fn live_verifies_a_well_known_textbook_via_open_library() {
        let client = BibliographyClient::new();
        let item = book(
            "Introduction to the Theory of Computation",
            &["Michael Sipser"],
        );
        let outcome = client.verify(&item).await;
        assert!(
            matches!(outcome, VerificationOutcome::Verified { .. }),
            "expected Sipser's textbook to verify, got {outcome:?}"
        );
    }

    #[tokio::test]
    #[ignore = "hits a real external API; run manually, see doc comment"]
    async fn live_does_not_verify_a_nonsense_title() {
        let client = BibliographyClient::new();
        let item = book(
            "Zzyzzqx Flibbertigibbet Nonsense Title That Cannot Possibly Exist 9999",
            &["Not A Real Author Zzqx"],
        );
        let outcome = client.verify(&item).await;
        assert!(
            matches!(
                outcome,
                VerificationOutcome::NotFound | VerificationOutcome::Unavailable { .. }
            ),
            "expected a nonsense title not to verify, got {outcome:?}"
        );
    }

    #[tokio::test]
    #[ignore = "hits a real external API; run manually, see doc comment"]
    async fn live_verifies_a_well_known_arxiv_paper() {
        let client = BibliographyClient::new();
        let item = article(
            "Attention Is All You Need",
            &["Ashish Vaswani"],
            Some(Identifier::Arxiv("1706.03762".into())),
        );
        let outcome = client.verify(&item).await;
        assert!(
            matches!(outcome, VerificationOutcome::Verified { .. }),
            "expected the Transformer paper to verify via arXiv, got {outcome:?}"
        );
    }

    #[tokio::test]
    #[ignore = "hits a real external API; run manually, see doc comment"]
    async fn live_verifies_a_well_known_doi_via_crossref() {
        let client = BibliographyClient::new();
        let item = article(
            "Nanometre-scale thermometry in a living cell",
            &["G. Kucsko"],
            Some(Identifier::Doi("10.1038/nature12373".into())),
        );
        let outcome = client.verify(&item).await;
        assert!(
            matches!(outcome, VerificationOutcome::Verified { .. }),
            "expected the Nature paper to verify via Crossref, got {outcome:?}"
        );
    }

    #[tokio::test]
    #[ignore = "hits a real external API; run manually, see doc comment"]
    async fn live_identifier_less_article_verifies_via_crossref_or_openalex() {
        let client = BibliographyClient::new();
        let item = article("Attention Is All You Need", &["Ashish Vaswani"], None);
        let outcome = client.verify(&item).await;
        assert!(
            matches!(outcome, VerificationOutcome::Verified { .. }),
            "expected the Transformer paper to verify via Crossref/OpenAlex search, got {outcome:?}"
        );
    }

    /// Exercises [`verify_bibliography`] itself (not just [`BibliographyClient::verify`])
    /// end to end against a real catalog: first call round-trips the
    /// network and persists to disk, second call must be served from the
    /// cache — proven by pointing the second call at a client that fails
    /// fast against any real host.
    #[tokio::test]
    #[ignore = "hits a real external API; run manually, see doc comment"]
    async fn live_verify_bibliography_caches_a_verified_outcome_and_reuses_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = BibliographyCache::open(tmp.path()).expect("open cache");
        let client = BibliographyClient::new();
        let item = book(
            "Introduction to the Theory of Computation",
            &["Michael Sipser"],
        );

        let first = verify_bibliography(&client, &cache, &item).await;
        assert!(
            matches!(first, VerificationOutcome::Verified { .. }),
            "expected the first call to verify via a real catalog, got {first:?}"
        );
        assert!(
            cache.get(&item).is_some(),
            "the outcome must be persisted to disk after the first call"
        );

        let fast_fail_client = BibliographyClient::unreachable_for_test();
        let second = verify_bibliography(&fast_fail_client, &cache, &item).await;
        assert_eq!(
            second, first,
            "the second call must be served from cache, not a (failing) network round-trip"
        );
    }
}
