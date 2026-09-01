# Live QA — slowness round, 2026-09-01

Doc id: lsxii19bhg ("Limites e Derivadas", Stewart, post-pivot). Server on port
7420, provider Groq via `LEARNIVE_API_BASE_URL` — `openai/gpt-oss-20b` (fast) /
`openai/gpt-oss-120b` (robust), the models from the user's `.env`, driven live
via Playwright. This round opened with the user's report: "if the model being
used is actually gpt-oss-120b there's something really wrong for the generation
to be taking this long" and "before the pivot the content started appearing
much faster".

## Instrument added first

`ai/provider.rs` (S29 item 1): per-call telemetry on both paths —
`ai stream[{model}]: connected on attempt N after {elapsed}` / `attempt N got
{status} …, sleeping {backoff}` / `first body chunk after {elapsed}` (real
TTFB, not connect time), and the `ai complete` counterparts. Every finding
below is read off those lines plus wall-clock, not inferred.

## Root cause of the TTFT complaint — the acervo gate, not the models

Three independent measurements, one conclusion:

- Model speed is fine: `first body chunk after 0.44–0.81s` on both tiers;
  whole structured calls 0.46–2.6s.
- A stalled n3 generation sat at **98.8% CPU with zero `ai` log lines** —
  pinned *before* the first model call, inside `prepare`.
- The stall was `ensure_document_grounded` → `validate_acervo`, whose own
  comment already said it: it **re-parses every PDF in the library**
  (pdf-extract + lopdf) — 11 books here, including a 1300-page Stewart — and
  `prepare` ran it on **every `/generate`**. Measured after instrumenting:
  **103.8s and 110.4s** per validation, once per cold state.

Before the pivot there was no per-generation gate of this shape, which is
exactly why "content started appearing much faster".

**Fix (S29 item 2):** `ensure_document_grounded` memoizes a pass per document
in `AppState::acervo_cache`, keyed by `acervo_signature()` — a stat-walk
fingerprint (DefaultHasher over name/len/mtime of `library/`,
`index/manual_matches/`, `index/toc/`, plus `outline.json`'s stat) of
everything the gate reads. Same fingerprint ⇒ instant `Ok(())`; any library,
manual-match, TOC, or outline change misses and forces the full validation
again. Failures are never cached. Gate runs now print their duration when they
do run, so future stalls are visible in the log instead of invisible.

Known limitation, accepted: concurrent `/generate` for the same document can
both miss a cold cache and run the validation twice (observed once live: two
~105s validations overlapping — the second request's scroll-triggered
`continue` arrived while the first still validated). Duplicate CPU, not
duplicate side effects; single-flight would only save CPU, not latency, since
the gate is on the request's critical path either way.

## Free-tier 429s — the second slowness mode, working as designed

Caught live after the gate fix: grading hit Groq's **8000 tokens-per-minute**
cap on `gpt-oss-20b` — attempt 0 and 1 refused with `retry_after ≈ 2–2.8s`,
attempt 2 succeeded after **7.8s** total. The S29 item-1 retry ladder absorbed
it exactly as built; nothing to fix. This is the irreducible cost of the free
tier (§15's product target) and is now *visible* per call instead of silent.

## Bugs found on the way, fixed and live-verified

- **Graded exercises never got `render_math`** (raw `\begin{cases}` LaTeX in
  the settled exercise iframe): ungraded moves go through `tag_move_html`
  (which renders), but the graded move's HTML is stored raw at all three
  sidecar-construction sites — `finalize` (reading.rs), remediation and
  practice (grading.rs). Fixed by rendering exactly once at each site, mirroring
  the remediation-explanation precedent. Verified: new exercises carry
  `<math display="block">` with the `cases` typeset; pre-fix attempts stay raw
  (append-only past, §5 — not rewritten).
- **`advanceAfterGrading` picked a container row**: after demonstrating "n2",
  the scan over creation-ordered `allItems` selected **chapter 1** (first row,
  state `available`) — GET 404 (containers have no node file), the fallback
  generate POST died in the regen guard ("this node was already generated"),
  and the next node never auto-generated (user-reported live mid-round).
  Client fix: reading-order successor over `state.displayOrder`, real nodes
  only (`isNodeItem`); same filter applied to skip eligibility and the
  next-topic continuation. Server fix: `redirect_into_chapter_child` now
  receives the event log's generated set and skips already-generated children
  (its doc comment's "there is no route back to this chapter's own id"
  assumption is what the live bug falsified); a fully-generated chapter falls
  through to the container refusal instead of the misleading regen-guard
  error.
- **Lazy neighbor loader relied on creation order** (carried from the
  pre-compaction half of the round): `setOutlineItems` now builds
  `state.displayOrder` (DFS linearization) and both `maybeLoadNeighbor` and
  `insertSectionInOrder` index into it; container rows are skipped via the
  S27e `item_type` field surfaced on `OutlineItemView` (previously probed and
  404'd on every boot), and a locked neighbor stops the walk (BYOK rule —
  verified live: no fetch for the locked n3 before its gate opened).
- **CSP `style-src` console noise** (9 violations per boot): parse-time
  `style=` attributes fire before the sanitizer's walk strips them; pre-stripped
  in `sanitizeHtml` (the walk stays the security authority).
- **`.source-edge-toggle` used an undefined token** (`--body-text` → `--fg`).

## Verification

`cargo test --workspace` 395 passed / 0 failed; clippy clean except the
pre-existing vendored `pdf-extract` warnings; fmt applied. Live Playwright
pass: boot with zero console violations, chapter-boundary lazy loads, wrong
answer → remediation with rendered math, correct answer → gate → advance to
the next real node with no gate re-validation and sub-1.5s model TTFB.
