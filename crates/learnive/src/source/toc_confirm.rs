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
//! **Extended for S27k (PLAN.md, 2026-08-29).** Two additions over the S27f
//! shape this module started as:
//! - [`ConfirmedTocEntry::inferred`] — `true` for an entry `source::toc`'s
//!   deduction cascade placed automatically, `false` for one the user typed
//!   or corrected by hand. This is the "invisible provenance" PLAN.md asks
//!   for (*"o usuário não vê nada, ou a app falhou"*): nothing in the public
//!   shape ever ranks or exposes this to the user, but a later deduction
//!   pass must never clobber a user's own correction — see [`TocConfirmStore::put_deduced`].
//! - [`ConfirmedToc::unresolved`] — titles the deduction cascade read off
//!   the contents page but could not place on a real physical page. This is
//!   now the ONLY thing the S27f confirmation screen should still ask about
//!   (PLAN.md: *"passa a listar só os capítulos não resolvidos"*), not a
//!   blank per-chapter form.
//!
//! **Shape note for S27g, flagged rather than solved here:** a
//! heuristic-path entry (the cascade's last resort, `super::acervo::heuristic_toc`)
//! still always has `page: None` — that heuristic only ever returns titles.
//! S27g's book→chapter contextual expansion will need a real page number to
//! build a chapter pointer and will have to get it some other way (or ask
//! the user directly) for anything that came through the heuristic instead
//! of a real embedded outline or a successful S27k deduction. The
//! embedded-outline path (read-only display, `needs_user_confirmation() ==
//! false`) and a resolved S27k entry both carry real page numbers.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One entry in a confirmed table of contents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ConfirmedTocEntry {
    pub title: String,
    /// The entry's own printed chapter/section number (S27g, 2026-08-30,
    /// `source::toc::ResolvedTocEntry::number`'s doc) — `None` for anything
    /// confirmed by the user directly (`confirm_one` never asks for one) or
    /// deduced from a book whose contents page carried no numbering.
    /// [`match_chapter`] tries this first, before falling back to title.
    #[serde(default)]
    pub number: Option<String>,
    /// `None` for anything confirmed from the heuristic path — see the
    /// module doc's shape note.
    #[serde(default)]
    pub page: Option<usize>,
    /// `true` when `source::toc`'s deduction cascade placed this entry
    /// automatically (S27k); `false` (the default, and always the case for
    /// anything pre-S27k) for a title the user typed or corrected
    /// themselves. Never exposed to the user — internal only, so a later
    /// deduction pass knows never to overwrite a user's own correction (see
    /// [`TocConfirmStore::put_deduced`]).
    #[serde(default)]
    pub inferred: bool,
}

/// A confirmed table of contents for one PDF, flat (see the module doc —
/// the heuristic this replaces never produced hierarchy either).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ConfirmedToc {
    pub entries: Vec<ConfirmedTocEntry>,
    /// Titles `source::toc`'s deduction cascade read off the contents page
    /// but could not place on a real physical page (S27k) — the only thing
    /// left to ask the user about. A title moves out of here into `entries`
    /// (with `inferred: false`) once the user answers it via
    /// [`TocConfirmStore::confirm_one`].
    #[serde(default)]
    pub unresolved: Vec<String>,
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
        Self::open_at(data_dir.as_ref().join("index").join("toc"))
    }

    /// Opens (creating if needed) the store directly at `dir`, with no
    /// `index/toc` join — for a caller that already computed the exact
    /// directory itself (mirrors `source::acervo`'s own `index_cache_dir`
    /// convention: the caller assembles the path once, this module doesn't
    /// re-derive it). S27k's `validate_acervo`/`check_toc` wiring uses this
    /// so the six-check engine can open the store from a plain path
    /// parameter, the same shape as its existing `index_cache_dir` — no new
    /// `data_dir`-shaped argument needed on a module that otherwise never
    /// takes one.
    pub fn open_at(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
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

    /// Merges a fresh `source::toc` deduction pass into whatever is already
    /// stored for this hash (S27k) — never via a blind overwrite, because a
    /// re-check must not clobber a user's own correction (module doc). Rule:
    /// keep every existing `inferred: false` entry untouched; replace or add
    /// `inferred: true` entries from `resolution.resolved`; anything still
    /// unresolved (skipping titles the user already confirmed by hand)
    /// becomes the new `unresolved` list.
    pub fn put_deduced(
        &self,
        content_hash: &str,
        resolution: &crate::source::toc::TocResolution,
    ) -> std::io::Result<()> {
        let mut toc = self.get(content_hash).unwrap_or_default();
        let confirmed_titles: std::collections::HashSet<String> = toc
            .entries
            .iter()
            .filter(|e| !e.inferred)
            .map(|e| e.title.clone())
            .collect();

        toc.entries.retain(|e| !e.inferred);
        for resolved in &resolution.resolved {
            if confirmed_titles.contains(&resolved.title) {
                continue; // a user correction already covers this title
            }
            toc.entries.push(ConfirmedTocEntry {
                title: resolved.title.clone(),
                number: resolved.number.clone(),
                page: Some(resolved.page),
                inferred: true,
            });
        }
        toc.unresolved = resolution
            .unresolved
            .iter()
            .filter(|t| !confirmed_titles.contains(*t))
            .cloned()
            .collect();

        self.put(content_hash, &toc)
    }

    /// Records the user's answer for one previously-unresolved title (S27k's
    /// retrofit of the S27f confirmation screen: it now asks about
    /// individual unresolved chapters, not a whole blank form). Moves
    /// `title` out of `unresolved` and into `entries` as a real,
    /// never-to-be-overwritten (`inferred: false`) confirmation.
    pub fn confirm_one(
        &self,
        content_hash: &str,
        title: &str,
        page: Option<usize>,
    ) -> std::io::Result<()> {
        let mut toc = self.get(content_hash).unwrap_or_default();
        toc.unresolved.retain(|t| t != title);
        toc.entries.retain(|e| e.title != title);
        toc.entries.push(ConfirmedTocEntry {
            title: title.to_string(),
            number: None,
            page,
            inferred: false,
        });
        self.put(content_hash, &toc)
    }

    fn path_for(&self, content_hash: &str) -> PathBuf {
        self.dir.join(format!("{content_hash}.json"))
    }
}

/// Resolves an outline-proposed chapter/section (S27g's book→chapter
/// contextual expansion, revised 2026-08-30 to elicit both a hierarchical
/// number like `"2.2.1"` and a name — see `engine::prompt::propose_outline`'s
/// doc for the full account of the reversal) against a book's own confirmed
/// table of contents.
///
/// Number first: an exact match, after stripping everything but digits and
/// dots (so `"§4.10"`, `"Ch. 4.10"`, `"4.10."` all normalize to `"4.10"`),
/// is unambiguous within one printing and is trusted over the name. Falls
/// back to the same lenient either-direction containment
/// `source::matching::normalize` gives every other title comparison in
/// `source` (`bibliography::plausible_match`, `acervo::candidate_matches`)
/// when there's no proposed number, or the number matches nothing (a
/// different edition's numbering, or the model was simply wrong) — a
/// missing/wrong number must never block an otherwise-good name match.
///
/// Unlike `plausible_match` (a handful of catalog search results, title
/// collisions unlikely), the name fallback here searches one book's ENTIRE
/// table of contents, where a short, generic entry title ("Introduction",
/// "Summary", "Functions") is common and can sit anywhere in the list.
/// First-match-wins under either-direction containment is order-dependent
/// and biased toward these short entries: `needle.contains(&hay)` is true
/// the moment ANY short `hay` happens to be a substring of the proposed
/// `name`, whether or not it's the entry the model meant. So among every
/// entry that clears the containment bar, this picks the one with the
/// LONGEST normalized title — the most specific match, and in particular
/// the exact match when one exists (an exact title has `hay == needle`,
/// which is always at least as long as any shorter substring competitor).
/// (Flagged by review 2026-08-30 before this ever ran against a real,
/// long table of contents; see `match_chapter_prefers_the_longer_of_two_containment_matches`.)
///
/// Returns `None`, not an error, when nothing clears the bar: the caller
/// degrades to whole-work scope, the same convention as every other step of
/// this cascade (S27k's `is_resolution_acceptable`, the acervo gate's
/// `TocCheck::Heuristic`) — a chapter that fails to resolve is a "no
/// page-level citation yet" outcome, not a "block generation" one.
pub fn match_chapter<'a>(
    entries: &'a [ConfirmedTocEntry],
    number: Option<&str>,
    name: &str,
) -> Option<&'a ConfirmedTocEntry> {
    if let Some(target) = number.map(normalize_number).filter(|n| !n.is_empty())
        && let Some(hit) = entries
            .iter()
            .find(|e| e.number.as_deref().map(normalize_number).as_deref() == Some(target.as_str()))
    {
        return Some(hit);
    }
    let needle = super::matching::normalize(name);
    if needle.is_empty() {
        return None;
    }
    entries
        .iter()
        .filter_map(|e| {
            let hay = super::matching::normalize(&e.title);
            if !hay.is_empty() && (hay.contains(&needle) || needle.contains(&hay)) {
                Some((hay.len(), e))
            } else {
                None
            }
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, e)| e)
}

/// Keeps only digits and dots, then trims a stray trailing dot — `"§4.10"`,
/// `"Chapter 4.10"`, `"4.10."` and `"4.10"` all collapse to the same string.
fn normalize_number(n: &str) -> String {
    n.chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>()
        .trim_matches('.')
        .to_string()
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
                    number: None,
                    page: Some(3),
                    inferred: false,
                },
                ConfirmedTocEntry {
                    title: "Chapter 2".into(),
                    number: None,
                    page: None,
                    inferred: false,
                },
            ],
            unresolved: Vec::new(),
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
                        number: None,
                        page: None,
                        inferred: false,
                    }],
                    unresolved: Vec::new(),
                },
            )
            .expect("put 1");
        store
            .put(
                hash,
                &ConfirmedToc {
                    entries: vec![ConfirmedTocEntry {
                        title: "Corrected".into(),
                        number: None,
                        page: Some(1),
                        inferred: false,
                    }],
                    unresolved: Vec::new(),
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
                        number: None,
                        page: None,
                        inferred: false,
                    }],
                    unresolved: Vec::new(),
                },
            )
            .expect("put a");

        assert_eq!(store.get("hash-b"), None);
    }

    fn confirmed_entry(
        number: Option<&str>,
        title: &str,
        page: Option<usize>,
    ) -> ConfirmedTocEntry {
        ConfirmedTocEntry {
            title: title.to_string(),
            number: number.map(String::from),
            page,
            inferred: true,
        }
    }

    /// S27g (2026-08-30): an exact number match wins even when the proposed
    /// name is a total miss — numbering is unambiguous within one printing,
    /// so it's trusted over a name the model may have guessed poorly.
    #[test]
    fn match_chapter_prefers_an_exact_number_match_over_the_name() {
        let entries = vec![
            confirmed_entry(Some("4"), "Functions and Program Structure", Some(70)),
            confirmed_entry(Some("4.10"), "Recursion", Some(84)),
        ];
        let hit = match_chapter(&entries, Some("4.10"), "totally unrelated wording").unwrap();
        assert_eq!(hit.page, Some(84));
    }

    /// The exact K&R counter-example this redesign is built around:
    /// recursion lives in a section whose own chapter title never says
    /// "recursion" — number-matching must land on the SECTION entry, not
    /// the chapter's.
    #[test]
    fn match_chapter_resolves_the_kr_recursion_counter_example_by_number() {
        let entries = vec![
            confirmed_entry(Some("4"), "Functions and Program Structure", Some(70)),
            confirmed_entry(Some("4.10"), "Recursion", Some(84)),
        ];
        let hit = match_chapter(&entries, Some("4.10"), "recursion in C").unwrap();
        assert_eq!(hit.title, "Recursion");
        assert_eq!(hit.page, Some(84));
    }

    /// A number that doesn't match anything in the real book (wrong
    /// edition, or the model was simply wrong) must not block an otherwise-
    /// good name match — falls back to lenient containment.
    #[test]
    fn match_chapter_falls_back_to_name_when_the_number_is_wrong() {
        let entries = vec![confirmed_entry(Some("5.2"), "Recursion", Some(84))];
        let hit = match_chapter(&entries, Some("4.10"), "Recursion").unwrap();
        assert_eq!(hit.page, Some(84));
    }

    /// No proposed number at all — pure name matching, either-direction
    /// containment (same rule `bibliography::plausible_match` uses).
    #[test]
    fn match_chapter_matches_by_name_alone_when_no_number_is_proposed() {
        let entries = vec![confirmed_entry(None, "4.10 Recursion", Some(84))];
        let hit = match_chapter(&entries, None, "recursion").unwrap();
        assert_eq!(hit.page, Some(84));
    }

    /// A generic, short TOC entry ("Functions") is a valid containment
    /// match for a longer proposed name ("Functions and Program
    /// Structure") purely because it's a substring of it — but it's the
    /// wrong chapter when a more specific entry with the exact title also
    /// exists. First-match-wins would pick whichever is earlier in
    /// `entries`; this asserts the LONGER (more specific) match wins
    /// regardless of order, catching the regression flagged in
    /// `match_chapter`'s own doc comment before it ever hit a real,
    /// long table of contents.
    #[test]
    fn match_chapter_prefers_the_longer_of_two_containment_matches() {
        let entries_short_first = vec![
            confirmed_entry(None, "Functions", Some(10)),
            confirmed_entry(None, "Functions and Program Structure", Some(60)),
        ];
        let hit = match_chapter(&entries_short_first, None, "Functions and Program Structure")
            .unwrap();
        assert_eq!(hit.page, Some(60));

        let entries_long_first = vec![
            confirmed_entry(None, "Functions and Program Structure", Some(60)),
            confirmed_entry(None, "Functions", Some(10)),
        ];
        let hit = match_chapter(&entries_long_first, None, "Functions and Program Structure")
            .unwrap();
        assert_eq!(hit.page, Some(60));
    }

    /// Number strings are normalized before comparison: punctuation/prefix
    /// noise around the digits must not defeat an otherwise-exact match.
    #[test]
    fn match_chapter_normalizes_number_punctuation_before_comparing() {
        let entries = vec![confirmed_entry(Some("2.2.1"), "Nested Loops", Some(40))];
        let hit = match_chapter(&entries, Some("§2.2.1."), "unrelated").unwrap();
        assert_eq!(hit.page, Some(40));
    }

    /// Nothing clears the bar — degrades to `None`, never an error, so the
    /// caller can leave the chapter un-narrowed.
    #[test]
    fn match_chapter_returns_none_when_nothing_matches() {
        let entries = vec![confirmed_entry(Some("1"), "Introduction", Some(1))];
        assert!(match_chapter(&entries, Some("9.9"), "completely unrelated topic").is_none());
    }

    fn resolution(
        resolved: &[(&str, usize)],
        unresolved: &[&str],
    ) -> crate::source::toc::TocResolution {
        use crate::source::toc::{ResolvedTocEntry, TocResolution};
        TocResolution {
            resolved: resolved
                .iter()
                .map(|(title, page)| ResolvedTocEntry {
                    title: title.to_string(),
                    number: None,
                    page: *page,
                })
                .collect(),
            unresolved: unresolved.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn put_deduced_stores_resolved_entries_as_inferred_and_the_rest_as_unresolved() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = TocConfirmStore::open(dir.path()).expect("open store");
        let hash = "hash-1";

        store
            .put_deduced(
                hash,
                &resolution(&[("Intro", 3), ("Chapter One", 10)], &["Appendix"]),
            )
            .expect("put_deduced");

        let toc = store.get(hash).expect("present");
        assert_eq!(toc.unresolved, vec!["Appendix".to_string()]);
        assert_eq!(toc.entries.len(), 2);
        assert!(toc.entries.iter().all(|e| e.inferred));
        assert!(
            toc.entries
                .iter()
                .any(|e| e.title == "Intro" && e.page == Some(3))
        );
    }

    #[test]
    fn put_deduced_never_overwrites_a_user_confirmed_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = TocConfirmStore::open(dir.path()).expect("open store");
        let hash = "hash-2";

        store
            .confirm_one(hash, "Intro", Some(1))
            .expect("user confirms Intro=1");
        // A later deduction pass disagrees (says Intro is page 3) — the
        // user's own correction must win.
        store
            .put_deduced(hash, &resolution(&[("Intro", 3), ("Chapter One", 10)], &[]))
            .expect("put_deduced");

        let toc = store.get(hash).expect("present");
        let intro = toc
            .entries
            .iter()
            .find(|e| e.title == "Intro")
            .expect("Intro present");
        assert_eq!(
            intro.page,
            Some(1),
            "user's confirmation must survive a later deduction"
        );
        assert!(!intro.inferred);
        let chapter_one = toc
            .entries
            .iter()
            .find(|e| e.title == "Chapter One")
            .expect("present");
        assert!(chapter_one.inferred);
    }

    #[test]
    fn put_deduced_replaces_a_stale_inferred_entry_on_a_later_pass() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = TocConfirmStore::open(dir.path()).expect("open store");
        let hash = "hash-3";

        store
            .put_deduced(hash, &resolution(&[("Intro", 3)], &[]))
            .expect("first pass");
        store
            .put_deduced(hash, &resolution(&[("Intro", 5)], &[]))
            .expect("second pass");

        let toc = store.get(hash).expect("present");
        assert_eq!(toc.entries.len(), 1);
        assert_eq!(toc.entries[0].page, Some(5));
    }

    #[test]
    fn confirm_one_moves_a_title_from_unresolved_to_entries() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = TocConfirmStore::open(dir.path()).expect("open store");
        let hash = "hash-4";

        store
            .put_deduced(hash, &resolution(&[("Intro", 3)], &["Appendix"]))
            .expect("deduce");
        store
            .confirm_one(hash, "Appendix", Some(200))
            .expect("confirm");

        let toc = store.get(hash).expect("present");
        assert!(toc.unresolved.is_empty());
        let appendix = toc
            .entries
            .iter()
            .find(|e| e.title == "Appendix")
            .expect("present");
        assert_eq!(appendix.page, Some(200));
        assert!(!appendix.inferred);
    }
}
