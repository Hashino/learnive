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
/// Number first, **but the number is no longer trusted blindly** (revised
/// 2026-08-30 from measurement, `docs/S27g-chapter-matching-measurements.md`
/// §4.3): an exact number match, after stripping everything but digits and
/// dots (so `"§4.10"`, `"Ch. 4.10"`, `"4.10."` all normalize to `"4.10"`),
/// is unambiguous *within one printing* — but the model is recalling the
/// number from memory, and when it misremembers, following the number lands
/// the node in a confidently wrong chapter. That is not hypothetical: **all
/// three models that completed the 2026-08-30 bake-off** proposed Pro Git's
/// "Git Internals" with a wrong number (`9`, `9`, `6`; the real one is `10`)
/// and the old number-first rule followed every one of them into the wrong
/// chapter. So a number hit is confirmed by a **similarity veto**: if the
/// name of the entry the number points at is too far from the proposed name,
/// the number is discarded and the name tier decides instead.
///
/// [`NAME_SIMILARITY_FLOOR`] is picked from the measured pairs, not guessed —
/// on the real bake-off data the correct number matches score ≥ 0.786
/// (`"branching"` vs `"git branching"`) and the wrong ones ≤ 0.682
/// (`"git internals"` vs `"git and other systems"`), so the floor sits in
/// that gap. The metric is **Jaro-Winkler**, which is prefix-weighted and
/// character-level; token overlap gets these exactly backwards, because a
/// correct match can share no whole token at all (`"integration"` vs
/// `"integrals"`, 0.883) while a wrong one shares a leading word
/// (`"git internals"` vs `"git and other systems"`).
///
/// Unlike `plausible_match` (a handful of catalog search results, title
/// collisions unlikely), the name tier here searches one book's ENTIRE
/// table of contents, where a short, generic entry title ("Introduction",
/// "Summary", "Functions") is common and can sit anywhere in the list. A
/// candidate qualifies by either-direction containment **or** by clearing
/// the same similarity floor, and among the qualifiers the **most similar**
/// wins.
///
/// Ranking by similarity rather than by length is itself a correction:
/// shipping "longest normalized title wins" (`ecc2488`) was measured wrong
/// in the opposite direction — for the needle `"Integrals"` it picked
/// `"15 - Multiple integrals"` (p978) over `"5 - Integrals"` (p382), 596
/// pages off, because the wrong entry is longer. No length rule satisfies
/// both that case and the `"Functions"` / `"Functions and Program Structure"`
/// case it was added for; "most similar" satisfies both (exact title = 1.0,
/// which no competitor can beat). See §2.7 of the measurements doc.
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
    let needle = super::matching::normalize(name);

    if let Some(target) = number.map(normalize_number).filter(|n| !n.is_empty())
        && let Some(hit) = entries
            .iter()
            .find(|e| e.number.as_deref().map(normalize_number).as_deref() == Some(target.as_str()))
    {
        // The similarity veto. With nothing to compare against — the model
        // sent no name, or the TOC entry has no title — there is no
        // disagreement to detect, so the number still stands.
        let hay = super::matching::normalize(&hit.title);
        if needle.is_empty()
            || hay.is_empty()
            || strsim::jaro_winkler(&needle, &hay) >= NAME_SIMILARITY_FLOOR
        {
            return Some(hit);
        }
        // Vetoed: the number pointed somewhere the name disagrees with. Fall
        // through and let the name decide, rather than returning None — a
        // wrong number must not block an otherwise-good name match.
    }

    if needle.is_empty() {
        return None;
    }
    entries
        .iter()
        .filter_map(|e| {
            let hay = super::matching::normalize(&e.title);
            if hay.is_empty() {
                return None;
            }
            let score = strsim::jaro_winkler(&needle, &hay);
            let qualifies =
                hay.contains(&needle) || needle.contains(&hay) || score >= NAME_SIMILARITY_FLOOR;
            qualifies.then_some((score, e))
        })
        .max_by(|(a, _), (b, _)| a.total_cmp(b))
        .map(|(_, e)| e)
}

/// Jaro-Winkler floor shared by the number-match veto and the name tier,
/// calibrated on the 2026-08-30 bake-off pairs rather than guessed: the
/// lowest-scoring *correct* number match measured 0.786 and the
/// highest-scoring *wrong* one measured 0.682, so this sits in the gap.
/// Raising it past ~0.78 starts rejecting real matches; lowering it past
/// ~0.69 starts admitting the "Git Internals" class of error that all three
/// bake-off models produced.
pub const NAME_SIMILARITY_FLOOR: f64 = 0.72;

/// Keeps only digits and dots, then trims a stray trailing dot — `"§4.10"`,
/// `"Chapter 4.10"`, `"4.10."` and `"4.10"` all collapse to the same string.
fn normalize_number(n: &str) -> String {
    n.chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

/// Splits a leading printed chapter number off a raw embedded-bookmark
/// title — `"2 - Limits and derivatives"` becomes
/// `(Some("2"), "Limits and derivatives")`. Handles a bare `"4.10 Recursion"`,
/// a `"Chapter 4: ..."`/`"Part 4 "` prefix, and the assorted dash/colon/dot
/// separators real books use. Returns `(None, trimmed)` when there's no
/// leading number to split (front matter, "Appendixes", etc.).
///
/// Ported from `toc_bench`'s `as_split` measurement (S27g bake-off,
/// 2026-08-30) into production for S27o (2026-08-31, live bug): without
/// this, [`match_chapter`]'s entries built from an embedded PDF outline
/// always carried `number: None` (nothing populated it), which skips the
/// number-first tier entirely and falls straight to name similarity —
/// exactly the tier the veto floor's own comment says loses real matches
/// whose wording differs (measured live: Stewart's real bookmark "2 -
/// Limits and derivatives" never matched a proposed "Limits and Continuity"
/// chapter numbered "2", because "continuity" vs "derivatives" alone don't
/// clear [`NAME_SIMILARITY_FLOOR`] — but WITH the number split out, the
/// number-first tier finds the "2" match directly and the veto's own
/// wording-tolerant comparison, `"limits and continuity"` vs `"limits and
/// derivatives"`, easily clears the floor on their long shared prefix).
pub fn split_printed_number(title: &str) -> (Option<String>, String) {
    // Real books put NON-BREAKING spaces inside the numbering (Think
    // Python's bookmarks are literally "Chapter\u{a0}1.\u{a0}The Way of the
    // Program") — measured 2026-08-30, and it silently defeated a first
    // version of this splitter on all 270 of that book's entries. Fold
    // every unicode space to a plain one before anything else.
    let folded: String = title
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let t = folded.trim();
    let lower = t.to_lowercase();
    let rest = if let Some(stripped) = lower.strip_prefix("chapter ") {
        &t[t.len() - stripped.len()..]
    } else if let Some(stripped) = lower.strip_prefix("part ") {
        &t[t.len() - stripped.len()..]
    } else {
        t
    };

    let digits_end = rest
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit() || *c == '.')
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    if digits_end == 0 {
        return (None, rest.trim().to_string());
    }
    let number = rest[..digits_end].trim_matches('.').to_string();
    if number.is_empty() {
        return (None, rest.trim().to_string());
    }
    let tail = rest[digits_end..]
        .trim_start_matches([' ', '\t', '-', '\u{2013}', '\u{2014}', ':', '.', ')'])
        .trim();
    // A "number" that ate the whole title (a TOC entry that is literally
    // just "1") leaves nothing to match on by name — keep the original.
    if tail.is_empty() {
        return (None, rest.trim().to_string());
    }
    (Some(number), tail.to_string())
}

/// Direct TOC sub-entries of a chapter — exactly one more dotted segment
/// than the chapter's own number (chapter `"4"` -> `"4.1"`, `"4.2"`, never
/// `"4.1.1"` or `"4"` itself). This is S27g item 2's zero-token first
/// choice for the chapter split: when the book's own confirmed TOC already
/// has this structure, using it beats asking a model to guess it from
/// prose. `chapter_number` must be the *matched TOC entry's own* number
/// (`ConfirmedTocEntry::number`, from [`match_chapter`]'s hit) — never an
/// unverified model-proposed number, which the name-tier match can return
/// when the model misremembered it; anchoring to the wrong number would
/// silently return a different chapter's children. Returns empty when
/// `chapter_number` is absent or nothing matches — both are sanctioned
/// "fall through to the model, or stay one node" outcomes, not errors.
pub fn sub_entries_within<'a>(
    entries: &'a [ConfirmedTocEntry],
    chapter_number: Option<&str>,
) -> Vec<&'a ConfirmedTocEntry> {
    let Some(raw) = chapter_number else {
        return Vec::new();
    };
    let parent = normalize_number(raw);
    if parent.is_empty() {
        return Vec::new();
    }
    let parent_depth = parent.split('.').count();
    let prefix = format!("{parent}.");
    entries
        .iter()
        .filter(|e| {
            let Some(n) = e.number.as_deref().map(normalize_number) else {
                return false;
            };
            n.starts_with(&prefix) && n.split('.').count() == parent_depth + 1
        })
        .collect()
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

    /// S27g: an exact number match wins over a merely-similar name, as long
    /// as the name at the number AGREES. **Rewritten 2026-08-30**: this test
    /// used to assert that the number wins even against `"totally unrelated
    /// wording"`, which the bake-off proved to be the wrong contract — see
    /// `match_chapter_vetoes_a_number_match_whose_name_loudly_disagrees`.
    #[test]
    fn match_chapter_prefers_an_exact_number_match_over_the_name() {
        let entries = vec![
            confirmed_entry(Some("4"), "Functions and Program Structure", Some(70)),
            confirmed_entry(Some("4.10"), "Recursion", Some(84)),
        ];
        let hit = match_chapter(&entries, Some("4.10"), "Recursion").unwrap();
        assert_eq!(hit.page, Some(84));
    }

    /// The measured failure this veto exists for, taken verbatim from the
    /// 2026-08-30 bake-off: **all three** models that completed proposed Pro
    /// Git's "Git Internals" with a wrong number (`9`, `9`, `6`; the real one
    /// is `10`), and number-first followed each into the wrong chapter. The
    /// name has to be able to overrule the number.
    #[test]
    fn match_chapter_vetoes_a_number_match_whose_name_loudly_disagrees() {
        let entries = vec![
            confirmed_entry(Some("9"), "Git and Other Systems", Some(305)),
            confirmed_entry(Some("10"), "Git Internals", Some(324)),
        ];
        let hit = match_chapter(&entries, Some("9"), "Git Internals").unwrap();
        assert_eq!(
            hit.page,
            Some(324),
            "the name must overrule a confidently wrong number"
        );
    }

    /// The veto must not fire on the many correct matches where the model's
    /// wording differs from the book's — these all scored ≥ 0.786 in the
    /// measured pairs and every one of them is a genuine hit.
    #[test]
    fn match_chapter_keeps_a_number_match_when_the_name_only_differs_in_wording() {
        for (proposed, title) in [
            ("Integration", "Integrals"),
            ("Differentiation", "Differentiaton rules"), // the book's own typo
            ("Limits and Continuity", "Limits and derivatives"),
            ("Branching", "Git Branching"),
        ] {
            let entries = vec![
                confirmed_entry(Some("3"), title, Some(200)),
                confirmed_entry(Some("99"), "Decoy", Some(1)),
            ];
            let hit = match_chapter(&entries, Some("3"), proposed)
                .unwrap_or_else(|| panic!("{proposed:?} vs {title:?} must not be vetoed"));
            assert_eq!(hit.page, Some(200), "{proposed:?} vs {title:?}");
        }
    }

    /// A vetoed number falls THROUGH to the name tier rather than returning
    /// `None` — otherwise a wrong number would be worse than no number.
    #[test]
    fn match_chapter_vetoed_number_still_lets_the_name_find_the_right_entry() {
        let entries = vec![
            confirmed_entry(Some("2"), "Completely Different Topic", Some(10)),
            confirmed_entry(Some("7"), "Pointers and Arrays", Some(93)),
        ];
        let hit = match_chapter(&entries, Some("2"), "Pointers and Arrays").unwrap();
        assert_eq!(hit.page, Some(93));
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

    /// A generic, short TOC entry ("Functions") is a valid containment match
    /// for a longer proposed name purely because it's a substring of it — but
    /// it's the wrong chapter when a more specific entry with the exact title
    /// also exists. Most-similar-wins picks the exact one (1.0) regardless of
    /// order in `entries`.
    #[test]
    fn match_chapter_prefers_the_most_similar_of_two_containment_matches() {
        for entries in [
            vec![
                confirmed_entry(None, "Functions", Some(10)),
                confirmed_entry(None, "Functions and Program Structure", Some(60)),
            ],
            vec![
                confirmed_entry(None, "Functions and Program Structure", Some(60)),
                confirmed_entry(None, "Functions", Some(10)),
            ],
        ] {
            let hit = match_chapter(&entries, None, "Functions and Program Structure").unwrap();
            assert_eq!(hit.page, Some(60));
        }
    }

    /// The case that proved "longest wins" wrong in the opposite direction
    /// (measurements doc §2.7): for the needle `"Integrals"`, Stewart's TOC
    /// offers both `"Integrals"` (p382) and the LONGER `"Multiple integrals"`
    /// (p978). Length picked the one 596 pages away; similarity picks the
    /// exact title. Both orders, since the old bug was order-sensitive too.
    #[test]
    fn match_chapter_does_not_let_a_longer_qualified_title_beat_the_exact_one() {
        for entries in [
            vec![
                confirmed_entry(None, "Multiple integrals", Some(978)),
                confirmed_entry(None, "Integrals", Some(382)),
            ],
            vec![
                confirmed_entry(None, "Integrals", Some(382)),
                confirmed_entry(None, "Multiple integrals", Some(978)),
            ],
        ] {
            let hit = match_chapter(&entries, None, "Integrals").unwrap();
            assert_eq!(hit.page, Some(382), "must not drift to Multiple integrals");
        }
    }

    /// Number strings are normalized before comparison: punctuation/prefix
    /// noise around the digits must not defeat an otherwise-exact match.
    /// (Name kept agreeing so the similarity veto isn't what's under test.)
    #[test]
    fn match_chapter_normalizes_number_punctuation_before_comparing() {
        let entries = vec![confirmed_entry(Some("2.2.1"), "Nested Loops", Some(40))];
        let hit = match_chapter(&entries, Some("§2.2.1."), "Nested Loops").unwrap();
        assert_eq!(hit.page, Some(40));
    }

    /// With no name to compare against there is no disagreement to detect,
    /// so a number hit still stands — the veto must not turn "no name" into
    /// "no match".
    #[test]
    fn match_chapter_keeps_a_number_match_when_there_is_no_name_to_veto_with() {
        let entries = vec![confirmed_entry(Some("4.10"), "Recursion", Some(84))];
        let hit = match_chapter(&entries, Some("4.10"), "   ").unwrap();
        assert_eq!(hit.page, Some(84));
    }

    /// Nothing clears the bar — degrades to `None`, never an error, so the
    /// caller can leave the chapter un-narrowed.
    #[test]
    fn match_chapter_returns_none_when_nothing_matches() {
        let entries = vec![confirmed_entry(Some("1"), "Introduction", Some(1))];
        assert!(match_chapter(&entries, Some("9.9"), "completely unrelated topic").is_none());
    }

    /// S27g item 2's zero-token shortcut: direct children only, one dotted
    /// segment deeper than the chapter — grandchildren and the chapter's own
    /// entry are excluded.
    #[test]
    fn sub_entries_within_returns_only_direct_children() {
        let entries = vec![
            confirmed_entry(Some("4"), "Functions and Program Structure", Some(70)),
            confirmed_entry(Some("4.1"), "Functions", Some(70)),
            confirmed_entry(Some("4.2"), "Pointers", Some(75)),
            confirmed_entry(Some("4.2.1"), "Pointer Arithmetic", Some(76)),
            confirmed_entry(Some("5"), "Pointers and Arrays", Some(93)),
        ];
        let titles: Vec<&str> = sub_entries_within(&entries, Some("4"))
            .into_iter()
            .map(|e| e.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Functions", "Pointers"]);
    }

    /// A dotted chapter number narrows the prefix accordingly — `"4.10"`'s
    /// children are `"4.10.x"`, not `"4.x"`.
    #[test]
    fn sub_entries_within_handles_a_dotted_chapter_number() {
        let entries = vec![
            confirmed_entry(Some("4.10"), "Recursion", Some(84)),
            confirmed_entry(Some("4.10.1"), "Recursive Descent", Some(85)),
            confirmed_entry(Some("4.1"), "Functions", Some(70)),
        ];
        let titles: Vec<&str> = sub_entries_within(&entries, Some("4.10"))
            .into_iter()
            .map(|e| e.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Recursive Descent"]);
    }

    #[test]
    fn sub_entries_within_is_empty_with_no_chapter_number() {
        let entries = vec![confirmed_entry(Some("4.1"), "Functions", Some(70))];
        assert!(sub_entries_within(&entries, None).is_empty());
    }

    #[test]
    fn sub_entries_within_is_empty_when_the_toc_has_no_sub_entries() {
        let entries = vec![
            confirmed_entry(Some("4"), "Functions and Program Structure", Some(70)),
            confirmed_entry(Some("5"), "Pointers and Arrays", Some(93)),
        ];
        assert!(sub_entries_within(&entries, Some("4")).is_empty());
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

    /// The exact live bug (S27o, reported 2026-08-31): a raw embedded PDF
    /// bookmark ("2 - Limits and derivatives", Stewart's real chapter 2)
    /// fed straight into `match_chapter` with `number: None` — the shape
    /// `ensure_document_grounded`'s chapter-resolution pass produced before
    /// this fix — never matches a model-proposed "Limits and Continuity"
    /// numbered "2": the raw, unsplit title defeats both the (absent)
    /// number tier and the name-similarity tier ("2 limits and derivatives"
    /// vs "limits and continuity" doesn't clear the floor). Splitting the
    /// number out first (what `flatten_embedded_outline` now does) restores
    /// the number-first tier, whose veto-protected name comparison
    /// ("limits and derivatives" vs "limits and continuity", long shared
    /// prefix) clears the floor easily. This is the regression that left
    /// Stewart's own chapter 2 permanently unresolved end to end.
    #[test]
    fn split_number_then_match_resolves_a_bookmark_the_raw_title_alone_cannot() {
        let (number, title) = split_printed_number("2 - Limits and derivatives");
        assert_eq!(number.as_deref(), Some("2"));
        assert_eq!(title, "Limits and derivatives");

        let split_entries = vec![confirmed_entry(number.as_deref(), &title, Some(110))];
        assert!(
            match_chapter(&split_entries, Some("2"), "Limits and Continuity").is_some(),
            "the split entry must match by number, wording difference vetoed"
        );

        // Same bookmark, unsplit (the pre-fix shape) — must NOT match, which
        // is exactly what silently broke live.
        let raw_entries = vec![confirmed_entry(
            None,
            "2 - Limits and derivatives",
            Some(110),
        )];
        assert!(
            match_chapter(&raw_entries, Some("2"), "Limits and Continuity").is_none(),
            "the raw unsplit title is the documented failure mode, not a new expectation"
        );
    }
}
