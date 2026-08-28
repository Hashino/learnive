//! User-confirmed table of contents store (§11.1's cascade, step 3, PLAN.md
//! S27f) — persists corrections to a PDF's *deduced* structure when
//! [`super::acervo::TocCheck::needs_user_confirmation`] is true, i.e. the PDF
//! had no embedded `/Outlines` and the app fell back to
//! [`super::acervo::heuristic_toc`]'s best-effort guess over the extracted
//! text. **This is a safety net, never a gate**: nothing in this module or
//! its caller (`api::acervo`) rejects a PDF for lacking bookmarks — SPEC
//! §11.1's own words, "nenhum PDF é rejeitado por não ter bookmarks."
//!
//! Keyed by the PDF's content hash ([`super::acervo::content_hash`]), like
//! the acervo gate's own retrieval-index cache (`<data>/index/library/`) —
//! a confirmed TOC belongs to the **file**, not to whichever bibliographic
//! item currently points at it: a renamed or re-matched file keeps its
//! confirmation, and two items that happen to share one PDF (rare, but nothing
//! rules it out) share the confirmation too.
//!
//! **Shape note for S27g, flagged rather than solved here:** entries are
//! flat with an *optional* page number, not nested with a guaranteed one.
//! [`super::acervo::heuristic_toc`] only ever returns titles — no page
//! numbers, no hierarchy — so a heuristic-path entry persisted here always
//! has `page: None`. S27g's book→chapter contextual expansion will need a
//! real page number to build a chapter pointer and will have to get it some
//! other way (or ask the user directly) for anything that came through the
//! heuristic instead of a real embedded outline. The embedded-outline path
//! (read-only display, `needs_user_confirmation() == false`) does carry real
//! page numbers via [`super::pdf::OutlineEntry`] — only the heuristic path
//! is missing them.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One entry in a confirmed table of contents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ConfirmedTocEntry {
    pub title: String,
    /// `None` for anything confirmed from the heuristic path — see the
    /// module doc's shape note.
    #[serde(default)]
    pub page: Option<usize>,
}

/// A user-confirmed table of contents for one PDF, flat (see the module
/// doc — the heuristic this replaces never produced hierarchy either).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ConfirmedToc {
    pub entries: Vec<ConfirmedTocEntry>,
}

/// Global, content-hash-keyed store of confirmed TOCs (module doc) — stored
/// alongside the acervo gate's own index cache and the manual-match store,
/// under its own subpath so none of the three ever collide.
#[derive(Debug, Clone)]
pub struct TocConfirmStore {
    dir: PathBuf,
}

impl TocConfirmStore {
    /// Opens (creating if needed) `<data>/index/toc/`.
    pub fn open(data_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = data_dir.as_ref().join("index").join("toc");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn get(&self, content_hash: &str) -> Option<ConfirmedToc> {
        let bytes = fs::read(self.path_for(content_hash)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Persists a confirmed TOC, atomically (tmp file + rename, same idiom
    /// as [`super::acervo::build_index_cache`]).
    pub fn put(&self, content_hash: &str, toc: &ConfirmedToc) -> std::io::Result<()> {
        let path = self.path_for(content_hash);
        let json = serde_json::to_vec_pretty(toc)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn path_for(&self, content_hash: &str) -> PathBuf {
        self.dir.join(format!("{content_hash}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_confirmed_toc() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = TocConfirmStore::open(dir.path()).expect("open store");
        let hash = "deadbeef";

        assert_eq!(store.get(hash), None);
        let toc = ConfirmedToc {
            entries: vec![
                ConfirmedTocEntry {
                    title: "Chapter 1".into(),
                    page: Some(3),
                },
                ConfirmedTocEntry {
                    title: "Chapter 2".into(),
                    page: None,
                },
            ],
        };
        store.put(hash, &toc).expect("put");
        assert_eq!(store.get(hash), Some(toc));
    }

    #[test]
    fn a_later_put_overwrites_the_earlier_confirmation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = TocConfirmStore::open(dir.path()).expect("open store");
        let hash = "abc123";

        store
            .put(
                hash,
                &ConfirmedToc {
                    entries: vec![ConfirmedTocEntry {
                        title: "First draft".into(),
                        page: None,
                    }],
                },
            )
            .expect("put 1");
        store
            .put(
                hash,
                &ConfirmedToc {
                    entries: vec![ConfirmedTocEntry {
                        title: "Corrected".into(),
                        page: Some(1),
                    }],
                },
            )
            .expect("put 2");

        let got = store.get(hash).expect("present");
        assert_eq!(got.entries.len(), 1);
        assert_eq!(got.entries[0].title, "Corrected");
    }

    #[test]
    fn different_content_hashes_stay_independent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = TocConfirmStore::open(dir.path()).expect("open store");

        store
            .put(
                "hash-a",
                &ConfirmedToc {
                    entries: vec![ConfirmedTocEntry {
                        title: "A's chapter".into(),
                        page: None,
                    }],
                },
            )
            .expect("put a");

        assert_eq!(store.get("hash-b"), None);
    }
}
