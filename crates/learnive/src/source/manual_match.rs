//! Manual PDF↔item pairing store (§11.1, PLAN.md S27f) — persists the
//! user's explicit choice when automatic matching in [`super::acervo`] is
//! ambiguous (multiple plausible candidates for one expected item, or an
//! unmatched library PDF the reading list doesn't know about) rather than
//! guessed by the app. **Filename is never the contract** — this store
//! records a pairing keyed by the *item's* bibliographic identity, so a
//! renamed library file still needs the user to re-pair it (silently
//! reattaching by filename would be exactly the "adivinhar" S27's own
//! narrative warns against).
//!
//! **Scope reduction, deliberate (flagged per the S27f task brief, not
//! silently stubbed):** this is a pure persistence layer. It is NOT wired
//! back into [`super::acervo::validate_acervo`]'s own candidate search — the
//! task only asks that a manual decision be recorded "somewhere sensible"
//! for a later slice to read, and consuming it inside the acervo gate's
//! automatic matching belongs to whichever slice makes the gate itself real
//! (S27g's contextual expansion or S27h's mandatory-gate wiring), neither of
//! which this slice builds. The one place this slice *does* honor a manual
//! pairing is its own TOC-confirmation endpoint (`api::acervo`), which needs
//! *a* resolved filename before it can read a PDF's table of contents at
//! all.
//!
//! Storage mirrors [`super::bibliography::BibliographyCache`]'s convention
//! exactly: `<data>/index/manual_matches/`, one JSON file per item, keyed by
//! a hash of the item's normalized title + sorted normalized author
//! surnames + kind (the same shape as that module's own `cache_key`, kept as
//! an independent copy here rather than a shared helper so `acervo`'s
//! [`super::acervo::ExpectedItem`] doesn't have to depend on
//! `bibliography::ProposedItem`, or vice versa — the two check engines stay
//! decoupled, per this module family's existing pattern).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::acervo::ExpectedItem;
use super::matching::{normalize, primary_title, surname_of};

/// One recorded manual pairing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManualMatch {
    pub filename: String,
}

/// Global, per-library store of manual pairings (module doc) — stored
/// alongside the acervo gate's own index cache and the bibliography cache,
/// under its own subpath so none of the three ever collide.
#[derive(Debug, Clone)]
pub struct ManualMatchStore {
    dir: PathBuf,
}

impl ManualMatchStore {
    /// Opens (creating if needed) `<data>/index/manual_matches/`.
    pub fn open(data_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = data_dir.as_ref().join("index").join("manual_matches");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn get(&self, item: &ExpectedItem) -> Option<ManualMatch> {
        let bytes = fs::read(self.path_for(item)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Persists a pairing, atomically (tmp file + rename, same idiom as
    /// [`super::acervo::build_index_cache`]/`BibliographyCache::put`).
    pub fn set(&self, item: &ExpectedItem, filename: &str) -> std::io::Result<()> {
        let path = self.path_for(item);
        let record = ManualMatch {
            filename: filename.to_string(),
        };
        let json = serde_json::to_vec_pretty(&record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn path_for(&self, item: &ExpectedItem) -> PathBuf {
        self.dir.join(format!("{}.json", item_key(item)))
    }
}

/// SHA-256 of the item's normalized title + sorted normalized author
/// surnames + kind — stable across re-lookups of the same work regardless of
/// author listing order, distinct across different works (mirrors
/// `bibliography::cache_key`; see the module doc for why this is a separate
/// copy rather than a shared function).
fn item_key(item: &ExpectedItem) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceKind;

    fn item(title: &str, authors: &[&str]) -> ExpectedItem {
        ExpectedItem {
            title: title.to_string(),
            authors: authors.iter().map(|a| a.to_string()).collect(),
            kind: SourceKind::Book,
        }
    }

    #[test]
    fn round_trips_a_manual_pairing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ManualMatchStore::open(dir.path()).expect("open store");
        let it = item(
            "Introduction to the Theory of Computation",
            &["Michael Sipser"],
        );

        assert_eq!(store.get(&it), None);
        store.set(&it, "sipser.pdf").expect("set");
        assert_eq!(
            store.get(&it),
            Some(ManualMatch {
                filename: "sipser.pdf".into()
            })
        );
    }

    #[test]
    fn a_later_set_overwrites_the_earlier_pairing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ManualMatchStore::open(dir.path()).expect("open store");
        let it = item("A Book", &["An Author"]);

        store.set(&it, "first.pdf").expect("set 1");
        store.set(&it, "second.pdf").expect("set 2");
        assert_eq!(
            store.get(&it),
            Some(ManualMatch {
                filename: "second.pdf".into()
            })
        );
    }

    #[test]
    fn key_is_stable_across_author_order_but_distinct_across_works() {
        let a = item("A Book", &["First Author", "Second Author"]);
        let b = item("A Book", &["Second Author", "First Author"]);
        let c = item("A Different Book", &["First Author", "Second Author"]);

        let dir = tempfile::tempdir().expect("temp dir");
        let store = ManualMatchStore::open(dir.path()).expect("open store");
        store.set(&a, "same.pdf").expect("set a");
        assert_eq!(store.get(&b).map(|m| m.filename), Some("same.pdf".into()));
        assert_eq!(store.get(&c), None);
    }
}
