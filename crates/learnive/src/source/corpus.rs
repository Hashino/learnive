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
//!     source.json         # normalized: SourceMeta + Section[] (§11.1)
//! ```
//!
//! A source id is written **once**: `store` is a no-op if the id already exists,
//! so re-acquiring the same source is free and never clobbers it (§5 non-destructive).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{FetchedSource, Origin, SourceMeta};

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
    /// was newly written.
    pub fn store(&self, source: &FetchedSource) -> Result<bool> {
        let id = safe_id(&source.meta.id)?;
        let dir = self.root.join(id);
        let path = dir.join("source.json");
        if path.is_file() {
            return Ok(false);
        }
        fs::create_dir_all(&dir)?;
        let json =
            serde_json::to_string_pretty(source).map_err(|e| CorpusError::Decode(e.to_string()))?;
        // Atomic write (tmp + rename), mirroring `store.rs`, so an interrupted
        // fetch never leaves a half-written source.json.
        let tmp = dir.join("source.json.tmp");
        fs::write(&tmp, json.as_bytes())?;
        fs::rename(&tmp, &path)?;
        self.append_manifest(&source.meta)?;
        Ok(true)
    }

    /// Loads a normalized source from the corpus.
    pub fn load(&self, id: &str) -> Result<FetchedSource> {
        let id = safe_id(id)?;
        let path = self.root.join(id).join("source.json");
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(CorpusError::NotFound(id.to_string()));
            }
            Err(e) => return Err(e.into()),
        };
        serde_json::from_slice(&bytes).map_err(|e| CorpusError::Decode(e.to_string()))
    }

    /// Lists metadata for every source in the corpus (for the §10 index rebuild
    /// and the read-only source viewer §11).
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
            if let Ok(src) = serde_json::from_slice::<FetchedSource>(&bytes) {
                out.push(src.meta);
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
