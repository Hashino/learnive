//! Immutable source corpus (§4, §11).
//!
//! Sources join the **immutable corpus**: fetched once and reused, never edited
//! (§4/§11). Layout, human-readable and rebuildable-cache-friendly (the §10
//! index is derived from this, never the other way around):
//!
//! ```text
//! <data>/corpus/
//!   SOURCES.md            # human-readable manifest (esp. required for web sources, §11)
//!   <source-id>/
//!     source.json         # meta + table of contents only (§11.1 item 4, S19)
//!     sections/
//!       <locator-hash>.json  # one Section (locator, title, text, html) per file
//!     assets/
//!       <content-hash>.<ext> # downloaded figures (§11.1 item 5), src-rewritten
//! ```
//!
//! Split on purpose (§11.1 item 4): before this, `source.json` held every
//! section's full text+html, so `GET /api/sources/{id}` shipped a whole book
//! per citation click. Now the addressable unit is one section, loadable on
//! its own (`load_section`) — the reader navigates and loads on demand, the
//! agent reads the table of contents instead of the whole book (S21's
//! prerequisite), and a citation resolves against one file, not a megabyte
//! JSON blob.
//!
//! `source.json` is still the commit marker: `store` writes every section
//! file first, `source.json` last, so `has`/the no-op-if-exists guard below
//! only ever see a source as "acquired" once every section is actually on
//! disk — an interruption mid-fetch leaves no `source.json`. A retry then
//! starts by wiping `sections/` (not just skipping ahead), so a retry that
//! fetches a *different* section set (paginated further, or the two-phase
//! fetch item 6 will add) never leaves the previous attempt's orphaned files
//! sitting in an "immutable" corpus dir with nothing in the TOC pointing at
//! them.
//!
//! A source id is written **once** via `store`: a no-op if the id already exists,
//! so re-acquiring the same source is free and never clobbers it (§5 non-destructive).
//! `store_complete` (§11.1 item 6) is the one sanctioned exception — it overwrites
//! an existing entry, but only to replace a *capped* first acquisition
//! (`SourceMeta.complete == false`) with a fuller fetch of the same source,
//! never an arbitrary edit.
//!
//! **Backward compatibility**: every source acquired before this split has a
//! monolithic `source.json` (`{meta, sections: [full Section, ...]}`, no
//! `toc` key, no `sections/` dir). `load`/`load_index`/`load_section` all
//! detect this shape (presence of a top-level `"sections"` key) and read it
//! as-is rather than failing — non-destructive (§5): nothing here silently
//! rewrites an old source into the new layout. A future completion/re-ingest
//! pass (§11.1 item 6) is the explicit migration path.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{FetchedSource, Origin, Section, SectionSummary, SourceIndex, SourceMeta};

#[derive(Debug)]
pub enum CorpusError {
    /// Id with an unsafe character (path-traversal defense — mirrors `store`).
    InvalidId(String),
    NotFound(String),
    Io(io::Error),
    /// Malformed `source.json`.
    Decode(String),
}

impl std::fmt::Display for CorpusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CorpusError::InvalidId(id) => write!(f, "unsafe source id: {id:?}"),
            CorpusError::NotFound(id) => write!(f, "source not in corpus: {id}"),
            CorpusError::Io(e) => write!(f, "corpus I/O error: {e}"),
            CorpusError::Decode(e) => write!(f, "malformed source.json: {e}"),
        }
    }
}

impl std::error::Error for CorpusError {}

impl From<io::Error> for CorpusError {
    fn from(e: io::Error) -> Self {
        CorpusError::Io(e)
    }
}

type Result<T> = std::result::Result<T, CorpusError>;

/// The immutable source corpus, rooted at `<data>/corpus`.
#[derive(Debug, Clone)]
pub struct Corpus {
    root: PathBuf,
}

impl Corpus {
    /// Opens (creating if needed) the corpus under a data directory.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let root = data_dir.as_ref().join("corpus");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// True if this source id has already been acquired (so a fetch can be skipped).
    pub fn has(&self, id: &str) -> bool {
        safe_id(id)
            .map(|_| self.root.join(id).join("source.json").is_file())
            .unwrap_or(false)
    }

    /// Persists a normalized source **once**. If the id already exists this is a
    /// no-op (the corpus is immutable — fetched once, reused). Returns whether it
    /// was newly written. Writes every section body first, `source.json`
    /// (meta+toc) last — see the module doc comment on why `source.json`'s
    /// existence is the commit marker.
    pub fn store(&self, source: &FetchedSource) -> Result<bool> {
        let id = safe_id(&source.meta.id)?;
        if self.root.join(id).join("source.json").is_file() {
            return Ok(false);
        }
        self.write_source(id, source)?;
        self.append_manifest(&source.meta)?;
        Ok(true)
    }

    /// Overwrites an already-acquired source with a fuller fetch (§11.1 item
    /// 6's background completion pass) — the one sanctioned exception to
    /// `store`'s no-op-if-exists guard, since this replaces a *capped* first
    /// acquisition with the same source more complete, not an arbitrary edit.
    /// Errors if the id isn't already in the corpus (use `store` for a first
    /// acquisition); doesn't touch `SOURCES.md` — the source is already
    /// listed there.
    pub fn store_complete(&self, source: &FetchedSource) -> Result<()> {
        let id = safe_id(&source.meta.id)?;
        if !self.root.join(id).join("source.json").is_file() {
            return Err(CorpusError::NotFound(id.to_string()));
        }
        self.write_source(id, source)
    }

    /// Shared by `store`/`store_complete`: writes every section body first,
    /// `source.json` (meta+toc) last — see the module doc comment on why
    /// `source.json`'s existence is the commit marker. Wipes `sections/`
    /// first so a retry/completion that fetches a *different* section set
    /// never leaves the previous attempt's files orphaned in the TOC.
    fn write_source(&self, id: &str, source: &FetchedSource) -> Result<()> {
        let dir = self.root.join(id);
        let path = dir.join("source.json");
        let sections_dir = dir.join("sections");
        fs::remove_dir_all(&sections_dir).ok();
        fs::create_dir_all(&sections_dir)?;

        for section in &source.sections {
            let stem = locator_filename(&section.locator);
            let sec_path = sections_dir.join(format!("{stem}.json"));
            let sec_json = serde_json::to_string_pretty(section)
                .map_err(|e| CorpusError::Decode(e.to_string()))?;
            let sec_tmp = sections_dir.join(format!("{stem}.json.tmp"));
            fs::write(&sec_tmp, sec_json.as_bytes())?;
            fs::rename(&sec_tmp, &sec_path)?;
        }

        let index = SourceIndex {
            meta: source.meta.clone(),
            toc: source
                .sections
                .iter()
                .map(|s| SectionSummary {
                    locator: s.locator.clone(),
                    title: s.title.clone(),
                })
                .collect(),
        };
        let json =
            serde_json::to_string_pretty(&index).map_err(|e| CorpusError::Decode(e.to_string()))?;
        // Atomic write (tmp + rename), mirroring `store.rs`.
        let tmp = dir.join("source.json.tmp");
        fs::write(&tmp, json.as_bytes())?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Loads a normalized source **in full** — every section's body, assembled
    /// from `sections/` (or read as-is from a pre-split, monolithic
    /// `source.json`, see the module doc comment). Prefer [`Corpus::load_index`]
    /// or [`Corpus::load_section`] when the whole book isn't actually needed
    /// (§10 index rebuild is the one caller that legitimately wants everything).
    pub fn load(&self, id: &str) -> Result<FetchedSource> {
        let id = safe_id(id)?;
        let dir = self.root.join(id);
        let value = read_source_json(&dir, id)?;
        if value.get("sections").is_some() {
            return serde_json::from_value(value).map_err(|e| CorpusError::Decode(e.to_string()));
        }
        let index: SourceIndex =
            serde_json::from_value(value).map_err(|e| CorpusError::Decode(e.to_string()))?;
        let sections_dir = dir.join("sections");
        let mut sections = Vec::with_capacity(index.toc.len());
        for summary in &index.toc {
            sections.push(read_section_file(&sections_dir, &summary.locator)?);
        }
        Ok(FetchedSource {
            meta: index.meta,
            sections,
        })
    }

    /// Loads just the metadata + table of contents (§11.1 item 4) — what the
    /// source panel, the reader's navigation, and the S21 agent-reads-a-sumário
    /// path use instead of loading every section's body.
    pub fn load_index(&self, id: &str) -> Result<SourceIndex> {
        let id = safe_id(id)?;
        let dir = self.root.join(id);
        let value = read_source_json(&dir, id)?;
        if value.get("sections").is_some() {
            // Pre-split monolithic source: no cheap path yet (the whole file
            // has to be read either way), derive a TOC from it in memory.
            let full: FetchedSource =
                serde_json::from_value(value).map_err(|e| CorpusError::Decode(e.to_string()))?;
            return Ok(SourceIndex {
                meta: full.meta,
                toc: full
                    .sections
                    .iter()
                    .map(|s| SectionSummary {
                        locator: s.locator.clone(),
                        title: s.title.clone(),
                    })
                    .collect(),
            });
        }
        serde_json::from_value(value).map_err(|e| CorpusError::Decode(e.to_string()))
    }

    /// Loads one section's body by locator (§11.1 item 4) — what a citation
    /// resolves against and what the reader loads on demand instead of the
    /// whole book.
    pub fn load_section(&self, id: &str, locator: &str) -> Result<Section> {
        let id = safe_id(id)?;
        let dir = self.root.join(id);
        let sections_dir = dir.join("sections");
        if sections_dir.is_dir() {
            return read_section_file(&sections_dir, locator);
        }
        // Pre-split monolithic source: the section lives inline.
        let value = read_source_json(&dir, id)?;
        let full: FetchedSource =
            serde_json::from_value(value).map_err(|e| CorpusError::Decode(e.to_string()))?;
        full.sections
            .into_iter()
            .find(|s| s.locator == locator)
            .ok_or_else(|| CorpusError::NotFound(format!("{id}#{locator}")))
    }

    /// Stores a downloaded figure's bytes (§11.1 item 5), addressed by a
    /// content-derived filename (`<sha256-prefix>.<ext>`, mirrors
    /// `locator_filename`'s hash idiom) rather than the original URL — so
    /// there's no path-traversal surface and no leak of the source host's
    /// URL structure into the corpus. Idempotent by construction: the same
    /// bytes always hash to the same filename, so a re-download harmlessly
    /// overwrites identical content instead of needing an existence check
    /// like `store`'s `source.json` guard.
    pub fn store_asset(&self, id: &str, filename: &str, bytes: &[u8]) -> Result<()> {
        let id = safe_id(id)?;
        let filename = safe_id(filename)?;
        let dir = self.root.join(id).join("assets");
        fs::create_dir_all(&dir)?;
        let path = dir.join(filename);
        let tmp = dir.join(format!("{filename}.tmp"));
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Loads a previously downloaded figure's bytes (§11.1 item 5) — what
    /// `GET /api/sources/{id}/assets/{filename}` serves.
    pub fn load_asset(&self, id: &str, filename: &str) -> Result<Vec<u8>> {
        let id = safe_id(id)?;
        let filename = safe_id(filename)?;
        let path = self.root.join(id).join("assets").join(filename);
        fs::read(&path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                CorpusError::NotFound(format!("{id}/assets/{filename}"))
            } else {
                CorpusError::Io(e)
            }
        })
    }

    /// Lists metadata for every source in the corpus (for the §10 index rebuild
    /// and the read-only source viewer §11). Reads only the `meta` field, which
    /// has the same shape whether `source.json` is the new split layout or an
    /// old monolithic one — no format branching needed here.
    pub fn list(&self) -> Result<Vec<SourceMeta>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path().join("source.json");
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Some(meta_value) = value.get("meta").cloned() else {
                continue;
            };
            if let Ok(meta) = serde_json::from_value::<SourceMeta>(meta_value) {
                out.push(meta);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Appends one human-readable line to `SOURCES.md` (§11 requires this for web
    /// sources; we do it for every source so the corpus is browsable by a human).
    fn append_manifest(&self, meta: &SourceMeta) -> Result<()> {
        use std::io::Write;
        let manifest = self.root.join("SOURCES.md");
        let fresh = !manifest.exists();
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&manifest)?;
        if fresh {
            writeln!(f, "# Sources\n")?;
            writeln!(
                f,
                "Immutable corpus (§4, §11): each source is fetched once and cited by \
                 id. Legal open sources only — no LibGen.\n"
            )?;
        }
        let authors = if meta.authors.is_empty() {
            String::new()
        } else {
            format!(" — {}", meta.authors.join(", "))
        };
        let origin = match &meta.origin {
            Origin::Web { url } => format!("web: {url}"),
            other => format!("{other:?}"),
        };
        writeln!(
            f,
            "- `{}` — **{}**{authors} · {origin} · {}",
            meta.id, meta.title, meta.license
        )?;
        Ok(())
    }
}

/// Reads and parses `<dir>/source.json` as a raw JSON value — the shared first
/// step for `load`/`load_index`/`load_section`, which each then branch on
/// whether it's the new split layout (`toc` key) or an old monolithic one
/// (`sections` key present).
fn read_source_json(dir: &Path, id: &str) -> Result<serde_json::Value> {
    let path = dir.join("source.json");
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(CorpusError::NotFound(id.to_string()));
        }
        Err(e) => return Err(e.into()),
    };
    serde_json::from_slice(&bytes).map_err(|e| CorpusError::Decode(e.to_string()))
}

/// Reads one section's file out of `sections_dir`, addressed by locator.
fn read_section_file(sections_dir: &Path, locator: &str) -> Result<Section> {
    let path = sections_dir.join(format!("{}.json", locator_filename(locator)));
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(CorpusError::NotFound(locator.to_string()));
        }
        Err(e) => return Err(e.into()),
    };
    serde_json::from_slice(&bytes).map_err(|e| CorpusError::Decode(e.to_string()))
}

/// Deterministic filesystem-safe filename for a section's locator (§11.1 item
/// 4). Locators carry `:`/`;` (e.g. `"chap:2;sec:1"`) that aren't safe path
/// segments, and are otherwise backend-controlled text — hashed rather than
/// escaped (mirrors `corpus_id`'s slug+hash idiom in `source/mod.rs`), so
/// there's no path-traversal surface no matter what a backend puts in a
/// locator string.
fn locator_filename(locator: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(locator.as_bytes());
    hash.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// Path-traversal defense: an id may contain only `[A-Za-z0-9._-]` and no `..`.
/// Mirrors the guard in `store.rs`.
fn safe_id(id: &str) -> Result<&str> {
    let ok = !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains("..")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(id)
    } else {
        Err(CorpusError::InvalidId(id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{Section, SourceKind};

    fn sample(id: &str) -> FetchedSource {
        FetchedSource {
            meta: SourceMeta {
                id: id.to_string(),
                title: "Calculus, Volume 1".into(),
                authors: vec!["Gilbert Strang".into()],
                kind: SourceKind::Book,
                license: "CC BY 4.0".into(),
                origin: Origin::OpenStax,
                handle: String::new(),
                complete: true,
            },
            sections: vec![Section {
                locator: "chap:2;sec:1".into(),
                title: "The Limit of a Function".into(),
                text: "A limit describes the value a function approaches.".into(),
                html: "<p>A limit describes the value a function approaches.</p>".into(),
            }],
        }
    }

    #[test]
    fn store_then_load_roundtrips() {
        let dir = std::env::temp_dir().join(format!("learnive-corpus-{}", std::process::id()));
        let corpus = Corpus::open(&dir).unwrap();
        let src = sample("calculus-v1-abcd1234");

        assert!(!corpus.has(&src.meta.id));
        assert!(corpus.store(&src).unwrap(), "first store writes");
        assert!(corpus.has(&src.meta.id));

        let loaded = corpus.load(&src.meta.id).unwrap();
        assert_eq!(loaded.meta.title, "Calculus, Volume 1");
        assert_eq!(loaded.sections[0].locator, "chap:2;sec:1");

        // Immutable: a second store is a no-op, never clobbers.
        assert!(!corpus.store(&src).unwrap(), "second store is a no-op");

        let listed = corpus.list().unwrap();
        assert!(listed.iter().any(|m| m.id == src.meta.id));

        let manifest = fs::read_to_string(dir.join("corpus/SOURCES.md")).unwrap();
        assert!(manifest.contains("Calculus, Volume 1"));
        assert!(manifest.contains("CC BY 4.0"));

        fs::remove_dir_all(&dir).ok();
    }

    /// §S19 item 4: `store` actually splits meta+toc from section bodies on
    /// disk (not just via the API) — the whole point being that
    /// `source.json` stays small regardless of how much text a source has.
    #[test]
    fn store_splits_meta_toc_from_section_bodies_on_disk() {
        let dir =
            std::env::temp_dir().join(format!("learnive-corpus-split-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        let corpus = Corpus::open(&dir).unwrap();
        let src = sample("calculus-v1-split1234");
        corpus.store(&src).unwrap();

        let source_json =
            fs::read_to_string(dir.join("corpus/calculus-v1-split1234/source.json")).unwrap();
        assert!(
            !source_json.contains("A limit describes"),
            "section body must not be inlined in source.json anymore: {source_json}"
        );
        let value: serde_json::Value = serde_json::from_str(&source_json).unwrap();
        assert!(value.get("toc").is_some(), "new layout has a toc key");
        assert!(
            value.get("sections").is_none(),
            "new layout has no sections key"
        );

        let sections_dir = dir.join("corpus/calculus-v1-split1234/sections");
        let entries: Vec<_> = fs::read_dir(&sections_dir).unwrap().collect();
        assert_eq!(entries.len(), 1, "one file per section");
        let section_file = fs::read_to_string(entries[0].as_ref().unwrap().path()).unwrap();
        assert!(section_file.contains("A limit describes"));

        fs::remove_dir_all(&dir).ok();
    }

    /// Advisor-flagged risk: `store` writes sections then `source.json` last so
    /// an *interrupted* fetch leaves nothing recognized as acquired — but a
    /// *retry* that fetches a different section set under the same id used to
    /// leave the first attempt's section files sitting in `sections/`, orphaned
    /// (nothing in the new TOC points at them) but never cleaned up. `store`
    /// now wipes `sections/` before writing, so a retry's on-disk section files
    /// always match its own TOC exactly.
    #[test]
    fn store_retry_does_not_orphan_stale_section_files() {
        let dir =
            std::env::temp_dir().join(format!("learnive-corpus-retry-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        let corpus = Corpus::open(&dir).unwrap();

        let mut first = sample("calculus-v1-retry1234");
        first.sections.push(Section {
            locator: "chap:2;sec:2".into(),
            title: "Continuity".into(),
            text: "A function is continuous where it has no breaks.".into(),
            html: "<p>A function is continuous where it has no breaks.</p>".into(),
        });
        assert!(corpus.store(&first).unwrap(), "first attempt writes");

        let source_json_path = dir.join("corpus/calculus-v1-retry1234/source.json");
        fs::remove_file(&source_json_path).unwrap();

        // Retry fetches only the first section — simulates a shorter second
        // attempt (fewer pages, or a two-phase fetch's first phase).
        let second = sample("calculus-v1-retry1234");
        assert!(
            corpus.store(&second).unwrap(),
            "retry after interruption writes again"
        );

        let sections_dir = dir.join("corpus/calculus-v1-retry1234/sections");
        let entries: Vec<_> = fs::read_dir(&sections_dir).unwrap().collect();
        assert_eq!(
            entries.len(),
            1,
            "retry's sections/ must contain only the retry's own sections, not the first attempt's leftovers"
        );

        let index = corpus.load_index(&second.meta.id).unwrap();
        assert_eq!(index.toc.len(), 1);
        assert_eq!(index.toc[0].locator, "chap:2;sec:1");

        fs::remove_dir_all(&dir).ok();
    }

    /// §11.1 item 6: `store_complete` overwrites an existing entry (the one
    /// sanctioned exception to `store`'s no-op guard) with a fuller fetch,
    /// and flips `complete` to `true`.
    #[test]
    fn store_complete_overwrites_a_capped_entry() {
        let dir =
            std::env::temp_dir().join(format!("learnive-corpus-complete-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        let corpus = Corpus::open(&dir).unwrap();

        let mut capped = sample("calculus-v1-complete1234");
        capped.meta.complete = false;
        assert!(
            corpus.store(&capped).unwrap(),
            "first (capped) store writes"
        );

        let mut full = sample("calculus-v1-complete1234");
        full.meta.complete = true;
        full.sections.push(Section {
            locator: "chap:2;sec:2".into(),
            title: "Continuity".into(),
            text: "A function is continuous where it has no breaks.".into(),
            html: "<p>A function is continuous where it has no breaks.</p>".into(),
        });
        corpus
            .store_complete(&full)
            .expect("store_complete overwrites");

        let index = corpus.load_index(&full.meta.id).unwrap();
        assert!(index.meta.complete, "completion marks the source complete");
        assert_eq!(
            index.toc.len(),
            2,
            "the fuller fetch's sections replace the capped ones"
        );

        // `store` (not `store_complete`) is still a no-op against the now-complete entry.
        assert!(
            !corpus.store(&capped).unwrap(),
            "store never overwrites an existing entry, even a capped one"
        );

        fs::remove_dir_all(&dir).ok();
    }

    /// `store_complete` refuses to create a new entry — it exists only to
    /// replace an already-acquired source, never as a `store` bypass.
    #[test]
    fn store_complete_errors_when_the_source_does_not_exist_yet() {
        let dir = std::env::temp_dir().join(format!(
            "learnive-corpus-complete-missing-{}",
            std::process::id()
        ));
        fs::remove_dir_all(&dir).ok();
        let corpus = Corpus::open(&dir).unwrap();

        let src = sample("calculus-v1-nevercalled1234");
        let err = corpus.store_complete(&src).unwrap_err();
        assert!(matches!(err, CorpusError::NotFound(_)));
        assert!(!corpus.has(&src.meta.id));

        fs::remove_dir_all(&dir).ok();
    }

    /// §S19 item 4: `load_index`/`load_section` against the split layout —
    /// the cheap paths the reader/agent/citation are meant to use instead of
    /// `load`'s whole-book read.
    #[test]
    fn load_index_and_load_section_work_against_split_layout() {
        let dir = std::env::temp_dir().join(format!("learnive-corpus-idx-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        let corpus = Corpus::open(&dir).unwrap();
        let src = sample("calculus-v1-idx1234");
        corpus.store(&src).unwrap();

        let index = corpus.load_index(&src.meta.id).unwrap();
        assert_eq!(index.meta.title, "Calculus, Volume 1");
        assert_eq!(index.toc.len(), 1);
        assert_eq!(index.toc[0].locator, "chap:2;sec:1");
        assert_eq!(index.toc[0].title, "The Limit of a Function");

        let section = corpus.load_section(&src.meta.id, "chap:2;sec:1").unwrap();
        assert_eq!(
            section.text,
            "A limit describes the value a function approaches."
        );

        assert!(matches!(
            corpus.load_section(&src.meta.id, "no:such:locator"),
            Err(CorpusError::NotFound(_))
        ));

        fs::remove_dir_all(&dir).ok();
    }

    /// §S19 item 1 regression: `Section` gained `html` with `#[serde(default)]`
    /// specifically so every `source.json` already on disk (written before this
    /// field existed) keeps loading — a source is fetched once and reused
    /// (§4/§11 immutable corpus), so this file predates the field permanently
    /// until an explicit re-ingest/completion pass (§11.1 item 6) backfills it.
    #[test]
    fn loads_a_pre_html_field_source_json_without_the_html_key() {
        let dir =
            std::env::temp_dir().join(format!("learnive-corpus-oldshape-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        let corpus_dir = dir.join("corpus").join("old-source-1234");
        fs::create_dir_all(&corpus_dir).unwrap();
        fs::write(
            corpus_dir.join("source.json"),
            r#"{
                "meta": {
                    "id": "old-source-1234",
                    "title": "Integral",
                    "authors": ["Wikipedia contributors"],
                    "kind": "article",
                    "license": "CC BY-SA 4.0",
                    "origin": {"backend": "wikipedia"}
                },
                "sections": [
                    {"locator": "sec:1", "title": "Introduction", "text": "An integral sums infinitesimal pieces."}
                ]
            }"#,
        )
        .unwrap();

        let corpus = Corpus::open(&dir).unwrap();
        let loaded = corpus.load("old-source-1234").unwrap();
        assert_eq!(
            loaded.sections[0].text,
            "An integral sums infinitesimal pieces."
        );
        assert_eq!(
            loaded.sections[0].html, "",
            "missing key defaults to empty, not a decode error"
        );

        fs::remove_dir_all(&dir).ok();
    }

    /// §S19 item 4 backward compat: `load_index`/`load_section` (new in this
    /// item) must also work against a pre-split monolithic `source.json`, not
    /// just `load` — a legacy source is still a valid target for a citation
    /// or the reader's TOC, just without the cheap-read path.
    #[test]
    fn load_index_and_load_section_work_against_legacy_monolithic_layout() {
        let dir = std::env::temp_dir().join(format!(
            "learnive-corpus-oldshape-idx-{}",
            std::process::id()
        ));
        fs::remove_dir_all(&dir).ok();
        let corpus_dir = dir.join("corpus").join("old-source-5678");
        fs::create_dir_all(&corpus_dir).unwrap();
        fs::write(
            corpus_dir.join("source.json"),
            r#"{
                "meta": {
                    "id": "old-source-5678",
                    "title": "Integral",
                    "authors": ["Wikipedia contributors"],
                    "kind": "article",
                    "license": "CC BY-SA 4.0",
                    "origin": {"backend": "wikipedia"}
                },
                "sections": [
                    {"locator": "sec:1", "title": "Introduction", "text": "An integral sums infinitesimal pieces."}
                ]
            }"#,
        )
        .unwrap();

        let corpus = Corpus::open(&dir).unwrap();

        let index = corpus.load_index("old-source-5678").unwrap();
        assert_eq!(index.meta.title, "Integral");
        assert_eq!(index.toc.len(), 1);
        assert_eq!(index.toc[0].locator, "sec:1");
        assert_eq!(index.toc[0].title, "Introduction");

        let section = corpus.load_section("old-source-5678", "sec:1").unwrap();
        assert_eq!(section.text, "An integral sums infinitesimal pieces.");

        assert!(matches!(
            corpus.load_section("old-source-5678", "sec:99"),
            Err(CorpusError::NotFound(_))
        ));

        fs::remove_dir_all(&dir).ok();
    }

    /// §S19 item 5: an asset roundtrips through `store_asset`/`load_asset`,
    /// and an id/filename outside the safe-id charset is rejected the same
    /// way `load`/`store` already reject one (path-traversal defense).
    #[test]
    fn store_asset_then_load_asset_roundtrips() {
        let dir =
            std::env::temp_dir().join(format!("learnive-corpus-asset-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        let corpus = Corpus::open(&dir).unwrap();

        corpus
            .store_asset(
                "calculus-v1-abcd1234",
                "deadbeef1234.png",
                b"\x89PNG fake bytes",
            )
            .unwrap();
        let loaded = corpus
            .load_asset("calculus-v1-abcd1234", "deadbeef1234.png")
            .unwrap();
        assert_eq!(loaded, b"\x89PNG fake bytes");

        assert!(matches!(
            corpus.load_asset("calculus-v1-abcd1234", "no-such-file.png"),
            Err(CorpusError::NotFound(_))
        ));
        assert!(matches!(
            corpus.store_asset("../escape", "x.png", b"x"),
            Err(CorpusError::InvalidId(_))
        ));
        assert!(matches!(
            corpus.store_asset("calculus-v1-abcd1234", "../../escape.png", b"x"),
            Err(CorpusError::InvalidId(_))
        ));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_unsafe_ids() {
        let dir = std::env::temp_dir().join(format!("learnive-corpus-bad-{}", std::process::id()));
        let corpus = Corpus::open(&dir).unwrap();
        assert!(matches!(
            corpus.load("../secret"),
            Err(CorpusError::InvalidId(_))
        ));
        assert!(!corpus.has("../secret"));
        fs::remove_dir_all(&dir).ok();
    }
}
