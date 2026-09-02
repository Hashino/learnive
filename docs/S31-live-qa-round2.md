# Live QA — slowness round 2, 2026-09-02

Doc id: zmu33mults ("Derivadas Regras", Stewart *Calculus: Early
Transcendentals*, post-pivot). Server on port 7420, provider Groq via
`LEARNIVE_API_BASE_URL` — `openai/gpt-oss-20b` (fast) / `openai/gpt-oss-120b`
(robust), the models from the user's `.env`, driven live via Playwright. The
user's ask for this round: verify the S29 slowness fixes actually landed,
diagnose whatever slowness remained, and fix other problems found along the
way.

## Verdict on S29: the per-generate cost was fixed; two holes were not

The S29 memoization removed `validate_acervo` from every `/generate` — but a
new document still paid ~3.5 minutes of dead time before its first token,
measured as two back-to-back full library re-reads:

- **The report endpoint had no memoization at all.** Creating a document
  auto-opens the Library-check panel (`documents.js:202`,
  `openAcervoGate("coldstart", …)`), and `GET …/acervo` ran the full
  `validate_acervo` every time — ~60s, and invisible, because unlike the gate
  it logged nothing.
- **S29's cache key was wrong for the gate itself.** `acervo_signature()`
  folded `outline.json` into the fingerprint, but the gate's own chapter
  resolution (S27g item 1) and split writes (item 2) mutate `outline.json`
  through `update_outline_file` — so the validation that had just finished
  invalidated itself, and the next `/generate` re-validated the whole library
  (133.3s measured live in the S29 round on exactly this sequence).

## S31 fix: one shared cache with a pure-function key

`AppState::acervo_cache` is now keyed by `(library_fingerprint,
expected_items_fingerprint)`:

- The library fingerprint (`acervo_signature()`, `api/reading.rs`) stats
  `library/`, `index/manual_matches/`, `index/toc/` — **`outline.json` is
  gone from the key**. The outline's influence on the report flows entirely
  through the expected items (title + authors + kind), hashed separately by
  `expected_items_fingerprint()`. The gate's own outline writes no longer
  miss the cache they just warmed.
- Both consumers share it: `ensure_document_grounded` (the `/generate` gate)
  and the report SSE. On a report hit, the `items`/`report`/`done` frames are
  emitted immediately — no `phase` events, nothing to make progress on.
- The key is global across documents (two documents citing the same books
  share one entry) and caches pass AND fail verdicts — the report is a pure
  function of the key, and fixing the library (adding the missing PDF,
  re-matching, confirming the TOC) changes the fingerprint, so a stale
  verdict can't outlive its inputs. I/O errors are never cached.
- Cache-hit counterpart of S27n: the mutating gate path writes the
  `LibraryFileIndex` records citation deep-links need, and it lives inside
  the validation a hit skips — `ensure_library_file_index()` scans the tiny
  record JSONs by filename and only reads + hashes + metadata-parses files
  genuinely missing a record (on the steady state, that loop body never
  runs).

## Live verification

- Cold gate on the fresh server: **80.7s, exactly once**
  (`acervo gate: full validation passed in 80.7s (memoized …)`), on the
  first `/generate` after the document reload.
- Everything after: the grep count of `acervo gate:` in the server log
  stayed at **1 for the rest of the session** — three more `/generate`
  requests, a Library-check panel open, and a page reload against the same
  data never re-validated. (The S31 cache is in-memory, so a server restart
  revalidates once by design; the S29 doc's per-process memoization
  semantics are unchanged.)
- Full loop completed live against Stewart with the free 20b model:
  4 moves (explain → revisit → explain → test), §S18 read-paced continue
  fired by the read-to-end sentinel each time, the sandboxed exercise
  rendered from its frame endpoint, and the rubric-locked answer
  `(9x^2+2)sin(2x)+(3x^3+2x)(2cos(2x))` graded **`demonstrated`** against
  `f(x) = (3x³ + 2x) sin(2x)` — auto-advance then correctly declared the
  outline complete (the two remaining chapters are still ⚠
  chapter-match-unresolved, and must not be treated as available).
- Remaining per-move latency: ~30–50s each, provider-side (streamed
  generation under Groq's 8000 TPM ceiling, 429s absorbed by the S29 retry
  ladder). That is the free-tier cost §15 accepts, not app overhead —
  the app adds none of it.

## Bugs found and fixed this round

1. **Chapter-match refusal dead-ended the programmatic generate path**
   (found live 2026-09-02). `prepare` refuses generation for a chapter
   `match_chapter` couldn't place (the 2026-09-01 enforcement), but the
   remediation modal only opened from an outline-row click — the
   post-creation auto-generate (and any programmatic path) showed a bare
   "generation error" paragraph with no way past. `node.js` now checks
   `chapter_match_failed` at the top of `generateNode` (opens the modal
   instead of POSTing, which also avoids the wasted request) and as a
   stale-flag backstop in `streamMoveRequest`'s catch. Verified live: the
   modal auto-opened on reload with the book named and the flat 26-entry TOC
   picker; picking page 200 for "The Product Rule" persisted
   `resolved_page: 200`, cleared its ⚠ badge after `refreshOutline`, and the
   refusal cleared.
2. **Leftover emojis violated the 2026-09-01 no-emoji decision**: 📚 in the
   Library-check button (`index.html`) and 🗑 in the delete-document button
   (`documents.js`). Replaced with plain text-presentation glyphs (▤ / ✕,
   the latter matching `acervo.js`'s remove affordance); title/aria-label
   already carry the meaning. A scan of all assets for emoji-range
   codepoints + U+FE0F is now clean; ⚙/✓/⚠/✕ remain as plain symbols, same
   allowance as the existing ✓/⚠.

## Findings flagged, not fixed (with reasons)

- **The S27g chapter-split attempt burned one Fast call and its latency on
  this chapter's first visit, and truncated.** With `resolved_page` set and
  `expansion` still `NotExpanded`, the gate's split attempt ran
  `propose_chapter_split`; gpt-oss-20b narrated in its reasoning channel and
  hit the output budget (`chapter split attempt failed for "The Product
  Rule": provider: the model hit its token budget after 5689 characters
  without finishing`), degrading to NoSplit — the chapter then generated as
  a single node, which is the designed fallback. The TOC shortcut can't help
  here (Stewart's PDF outline is flat — 26 top-level chapter bookmarks, no
  section children — so `sub_entries_within` finds nothing). Not fixed: the
  feature is correct; the failure mode is the known free-model
  reasoning-budget burn, whose fix is the `reasoning_effort`/`max_tokens`
  knob below.
- **Grounding verification failed open on 3 of 3 checked moves** (the
  visible "grounding could not be fully confirmed" banner on each — honest
  degradation, zero corrective tokens spent, per §12.2). Root cause is the
  same `ProviderError::Truncated`: the compact
  `{"unsupported_claims":[…]}` JSON loses to the reasoning narration under
  the default output budget. The clean fix is threading a per-call
  `reasoning_effort` (low, for verifier/parse-shaped calls) through
  `complete()` — an `Ai` trait change on a swappable boundary, so it needs
  explicit sign-off before being built.
- **One content-quality artifact from the free model**: an `explain` move
  presented a false "simplification" (`x² sin x = ⅓x³ cos x + ⅓ sin x`).
  Recorded because free models are the product target (§15), not a floor to
  tolerate — worth revisiting via prompt/verification work, not a
  regression from this round.
- `/acervo/toc` (the remediation modal's candidate fetch) still takes ~40s
  on the 1300-page Stewart (a `lopdf` outline walk per request). Untouched
  this round; the S31 cache does not and should not cover it (it's a
  different, per-book read), but it's the next-slowest interaction on this
  library.
- The remediation modal still doesn't auto-resume generation after a page
  pick — clicking the row regenerates, which is acceptable now that the
  modal is reachable from every path (item 1).

## Tests

`cargo test --workspace` 395 passing; clippy clean except the pre-existing
vendored `pdf-extract` warnings.
