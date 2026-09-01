# Live QA — "Limites e Derivadas" (Stewart only), 2026-09-01

Doc id: lsxii19bhg. Server: cargo run, LEARNIVE_NO_OPEN=1, port 7420, provider
Groq (openai/gpt-oss-20b fast / openai/gpt-oss-120b robust), driven live via
Playwright, not a mock.

## Reproduced findings

### (d) chapter_match_failed / remediation-modal contradiction — CONFIRMED, ROOT CAUSED, FIXED
`cold_start.rs`'s `outline_view` computed `chapter_match_failed` purely from
outline state (`resolved_page.is_none() && parent.expansion == Expanded`).
`api::reading::prepare` never consulted it. Root cause: `prepare` only skips
the chapter->node split (`try_split_chapter`, reading.rs:1151-1154) when
`resolved_page.is_none()`, but does NOT refuse generation for that chapter —
it falls through to `ground_node`'s unscoped full-book-search fallback and
generates real content anyway. Result: a chapter can have real generated
content AND still show the "restart document / skip chapter" remediation
modal, because the two code paths disagreed about what "failed" means.

Fix applied and committed (`ee676a6`):
- Promoted the predicate to `engine::chapter_match_failed(items, item)`
  (engine.rs), shared by both `outline_view` and a new gate in `prepare()`
  (reading.rs) that refuses with `EventKind::GenerationBlocked` BEFORE
  `ground_node` runs. Closes the contradiction structurally: it is now
  impossible for a chapter this flag is true for to ever reach generation.
- `cargo build`/`clippy --all-targets`/`fmt --check`/`test --workspace`
  all clean (364 passed, 0 failed).
- **Caveat still stands**: NOT verified against a live repro of the
  original trigger — Stewart's "Limits and Continuity" chapter matched
  cleanly in this run, so the refusal branch itself was never exercised
  live, only compile/unit-level. The fix is structurally sound (it's the
  right invariant — a node must never generate before its chapter's match
  is settled) but that specific branch wants a live repro with a book
  whose matching genuinely fails before this can be called fully closed.

### NEW, not in the original report: stale `rec.nodeId` after chapter→child redirect — CONFIRMED, ROOT CAUSED, FIXED, LIVE-VERIFIED TWICE
This is the strongest finding of the session — discovered live, not
hypothesized. `node.js`'s `buildSection(nodeId)` fixes `rec.nodeId` to
the outline item id the learner clicked (often a *chapter* id) at
section-creation time. When `prepare()`'s chapter→child-node redirect
fires (S27g item 2 — the ordinary case, since almost every chapter
splits into real child nodes), the SSE `exercise` event's `data` carries
the REAL generated node id, which differs from the chapter id.
`renderExerciseInto` already used `data` correctly for the iframe's own
URL — the exercise rendered and displayed fine — but `submitAnswer`
reads `rec.nodeId`, never updated, and POSTs the answer against the
wrong (chapter) id. `grading.rs::answer` reads `{node_id}.rubric.json`
keyed strictly by the URL path's node id, so this produced a clean
`404 Not Found`, not any other failure mode.

Fixed by setting `rec.nodeId = data;` in the `exercise` SSE handler, the
first point the real id is known (`node.js` commit `ea0d8c8`).
`cargo build`/`clippy --all-targets`/`fmt --check`/`test --workspace`
all clean. **Live-verified twice**: the first exercise submission
(`POST .../nodes/vmpie7eszf/answer => 200 OK`, after a transient 502
from a live Groq error, unrelated to this fix), and again on the §8.2
remediation retry (`200 OK`, graded "demonstrated — Answer matches the
exact limit.", node marked ✓ in the sidebar, next outline item
"Continuity of functions" unlocked, and generation auto-started on the
next node). This confirms the entire move→exercise→answer→grade→
advance loop closes correctly end to end once this fix is applied.

### (a) generation much slower than pre-pivot — CONFIRMED, PARTIALLY ROOT CAUSED, NOT A NEW BUG
Measured live, real Groq API, real Stewart PDF:
- Move 1 (`explain`): ~257s wall (confirm click -> move_generated)
- Move 2 (`ask`): ~182s wall
- Move 3 (`explain`): ~187s wall

Ruled out: PDF re-hashing/re-parsing (S27o cache already fixed this —
hashing Stewart's 25MB PDF takes 142ms; the embedding index for this book
was already built in an earlier session, `check_index_cache` hits).
`ensure_document_grounded` does re-scan+re-hash+re-deserialize cached
pdftext for ALL 11 library books on every move (not just the 1 in this
doc's reading list) — real overhead, but sub-second per the measurement
above, not the dominant cost.

Actual dominant cost, evidenced by a single OS thread pegged at ~100% CPU
continuously during "generating..." AND an established outbound TLS
connection to the provider the whole time: provider (Groq
gpt-oss-20b/120b) latency itself. This matches an ALREADY-DOCUMENTED,
pre-existing constraint in `.env`'s comments (2026-08-20 measurement:
"126-180s+ measured direct at provider for a structured grounded prompt of
~8.5KB... not a hang; COMPLETE_BUDGET adjusted 60s -> 200s because of it")
and in CLAUDE.md's "free tiers 429 constantly" operational corollary.
**This is very likely NOT a pivot-introduced regression** — it's model/
provider choice drift (moved to Groq's gpt-oss reasoning models
2026-08-20, documented as slow-but-tolerated at the time) compounding with
the pivot's heavier grounded-prompt payloads (whole-chapter context now
vs. smaller atomic-node context pre-pivot). The user's "muito mais lento
que antes do pivot" perception is real and measured, but the lever isn't
in the pivot's own code — it's prompt payload size interacting with an
already-known-slow reasoning model. Not fixed in this session; flagging
as a PLAN.md item (see below) rather than attempting a same-session fix,
since the two candidate fixes (switch provider/model, or cut prompt
payload) are both product decisions, not bugs.

CPU-bound local work (`Embedder::Static` model2vec embedding,
`build_index_cache`) IS real and IS synchronous inside `prepare()` the
FIRST time a book's index is missing — confirmed by reading
`ensure_document_grounded`'s `missing_index` loop (reading.rs:1791-1827).
Did not fire in this run only because Stewart's index already existed
from a prior session. This is a real, separate, not-yet-triggered risk:
a first-ever chapter open on a brand new book blocks the FIRST move
behind a synchronous whole-book local embedding pass with zero visible
progress (the acervo gate's "index" phase only CHECKS the cache, per
`build_index_cache`'s own doc comment: "not called by validate_acervo...
a plain validation pass must never force a model download"). Flagging
as a follow-up PLAN.md item, not fixed this session (didn't reproduce
live, and moving it into the visible acervo-gate progress screen is a
nontrivial design call, not a quick patch).

### (c) exercises render as plain HTML, not sandboxed — CLOSED, NOT A BUG
The `ask` move (not `test`) rendered a "Check Your Understanding" question
with 4 plain-text answer choices directly in the document body — no
`data-interactive`/`exercise-frame`. This is BY DESIGN: `MoveType::ask` is
`MoveRender::Streamed`, never reaches the `graded` branch in
generation.rs, and per `movement.rs` module docs streamed moves are
sanitized prose in the app origin by contract, not graded exercises. The
`ask` move is a Socratic question (§7's "the question is the most
valuable signal"), not §8's rubric-graded assessment — it looking
quiz-like (multiple-choice options) is a tactic choice
(`"ask-question","multiple-choice"` in the move_generated event), not a
mis-route.

**A real `test` move WAS subsequently observed in this run and it is
correct.** It rendered inside a genuine sandboxed `<iframe title="Exercise">`
(served from `.../blocks/{id}/frame` with its own CSP, per §4.4), the
answer submitted via `postMessage`/structured POST, and it graded
correctly (`not demonstrated` on a wrong answer, `demonstrated` on a
correct one, exact match to §8's per-objective grading). Closing this as
"working as designed" — the user's original report ("exercicios ... sem
parar... sem bloco interativo") is best explained by `ask` moves (which
fire routinely mid-node, quiz-like UI, no sandbox by design) being
mistaken for graded `test` exercises, not by an actual sandbox-escape or
mis-route. Recommend only a cosmetic follow-up: give `ask`'s
multiple-choice rendering a visual cue distinguishing it from a real
graded exercise, so this mistake is harder for a user to make again.

### (b) auto-generation not pausing at exercises — DID NOT REPRODUCE
In this run, generation paused after EVERY move (`explain`, `ask`,
`explain`) and required an explicit `window.scrollTo(0,
document.body.scrollHeight)` to fire `node_read_to_end` and trigger the
next move. §S18's read-to-end-gate architecture is working as designed
in this run. Hypothesis for the original report (not confirmed): a node
whose rendered content is short enough to fit entirely in the viewport
on first paint may satisfy the "read to end" intersection sentinel
immediately, without the user ever scrolling — this QA run's content was
long enough (many paragraphs, a table, 5 LaTeX laws) to never hit that
case. Flagging as a follow-up to check specifically with a short move,
not fixed/confirmed this session.

### §8.2 remediation flow — CONFIRMED WORKING AS SPECIFIED (positive finding)
On the `test` move's first wrong answer ("does not exist"), the node
opened a tutor conversation in place: a worked example specific to the
failed problem ("Let's review", full rationalized-numerator derivation
tied to the exact function the learner was given), followed by a new,
structurally similar problem in its own sandboxed exercise iframe
("Now try this one"). Answering that one correctly graded
"demonstrated", closed the remediation thread, and advanced the node.
This is an exact live match to §8.2's spec (worked example → new similar
problem → demonstrated) — the first positive confirmation on record for
this subsystem post-pivot, not just an absence of bugs.

### Persisting from earlier in this session: grounding-not-confirmed warning — ROOT CAUSED
Two sections in this run carried: "This section's grounding could not be
fully confirmed against its cited source — some claims may not be
accurately supported." Same warning the user reported earlier this
session on a document where the book WAS confirmed and chapter
resolution DID succeed (resolved_page: 110) — so it's not a matching
problem. Server log (`grep grounding server.log`) gives the exact cause:

```
grounding re-check failed: provider: API error (429): Rate limit reached
  for model `openai/gpt-oss-20b`... tokens per minute (TPM): Limit 8000...
grounding corrective regeneration failed: provider: API error (429): ...
```

`movement::grounding::verify_and_correct` (grounding.rs) runs a SEPARATE
structured call after a grounded move finishes, to catch fabricated/
uncited claims (§S21, added after a real fabrication was caught live
2026-08-27). Its own doc comment says the check can cost "up to three
extra calls" beyond the move's own generation. Both banners trace to
that verification pass's OWN calls (the corrective-regeneration call,
and/or the re-check-after-regeneration call) hitting Groq's free-tier
8000 TPM limit — not to a genuine "content is fabricated" verdict in the
cases that failed to complete verification (the code path IS careful:
line 85-90's own check failing degrades unflagged; only 108/114 flag).
Zero instances of the FIRST check itself failing were in the log — the
gate's initial pass ran fine and (per the code's own escalation shape)
found real unsupported claims worth flagging in the first place; it's
the RECOVERY attempt (regenerate + re-check) that starves the same
scarce TPM budget the move's own generation already used most of.

This directly violates CLAUDE.md's own stated hard constraint: "any
recovery path that spends an extra model call is broken exactly when it
is needed... Failure recovery must cost zero tokens" (§12.2 operational
corollary, measured 2026-08-30). The grounding gate's recovery path
(regenerate + re-check = 2 more calls, on top of the 1 verification
call = 3 total) is exactly the anti-pattern that constraint was written
to rule out, just in a subsystem (§S21, added 2026-08-27) that predates
the constraint being written down (2026-08-30) — the two were never
reconciled. Real fix candidates (not implemented this session — a
design call, not a quick patch): cap the gate to its first check only
and drop the token-spending recovery attempts, spend the recovery
budget on the fast tier instead of re-running on the same
already-exhausted tier, or make the banner distinguish "verification
infrastructure failed" from "content genuinely flagged" so a rate-limit
hiccup doesn't read to the user as "this may be fabricated." Recommend
as a PLAN.md item.

### NEW: transient CSP `style-src` console violations from model-authored `<span style>` — CONFIRMED HARMLESS, NOT A FUNCTIONAL OR SECURITY BUG
3 identical console errors appeared during this run: "Applying inline
style violates ... 'style-src 'self'' ... @ core.js:215" (the line where
`sanitizeHtml` does `tpl.innerHTML = html`). Initially hypothesized as
`pulldown-latex`'s MathML output (which does emit `style=` on some
`<mspace>`/`<mtr>`/`<merror>` paths, confirmed by reading the crate
source) — **this hypothesis was checked and falsified**: a live DOM scan
(`querySelectorAll('[style]')` across the main document and all
same-origin iframes) after the violations fired found zero MathML
elements carrying a `style` attribute anywhere in the rendered page.

Grepping the actual generated node file
(`learnive-data/lsxii19bhg/vmpie7eszf.html`) found the real source:
`<span style="font-family:monospace;">`, repeated — **not** MathML at
all. This is the MODEL's own formatting choice around its unparseable-
LaTeX fallback text (`core::math`'s `to_mathml` copies unparseable LaTeX
through verbatim, per its own `unparseable_latex_survives_as_text` test
— it does not add this span; the LLM wrote it into its own prose).

`sanitizeHtml` (core.js:247-248) already strips `style` attributes on
every element (`if (name === "on..." || name === "srcdoc" || name ===
"style") elm.removeAttribute(...)`) — confirmed correct by the same DOM
scan finding zero surviving `style` attributes in what's actually
displayed. The violation is a **parse-time-only artifact**: assigning
`tpl.innerHTML = html` makes the browser parse and attempt to apply the
inline style as part of HTML parsing (which CSP blocks and logs)
*before* the synchronous tree-walker cleanup pass (which runs
immediately after, on the same `tpl.content`, and strips the attribute)
gets to it. The stripped output is what's actually returned and
inserted into the live document — so this never reaches the user as a
rendering or security issue, only as console noise.

**Recommendation: leave the CSP header alone** (`style-src 'self'` with
no `unsafe-inline` is a deliberate §3.1 posture — loosening it to quiet
a cosmetic console warning would be a real security trade for nothing).
If the noise itself is worth silencing, the options are: strip `style`
attributes in a pre-pass before assigning to `tpl.innerHTML` in the
first place (avoids the parse-time violation entirely), or accept the
console noise as harmless. Not fixed this session — correctly
low-priority once traced to its actual, harmless cause.

## Observed in this run (were "not yet observed" as of the first draft)
- A `test` move (structured, force-graded) — rendered correctly in a
  sandboxed `exercise-frame` iframe, graded correctly both wrong and
  right. See (c) above, now closed.
- Node completion / advance to the next outline item — confirmed via
  the §8.2 remediation retry: "demonstrated" grade, ✓ badge in sidebar,
  next chapter item unlocked, next node's generation auto-started.

## Not yet observed in this run
- The chapter_match_failed remediation modal actually opening (Stewart's
  chapters matched cleanly this run, so (d)'s fix is unverified live —
  see the caveat in that section, unchanged).
- Full document completion (all 5 remaining outline items in "Limits
  and Continuity" plus "Differentiation") — the session stopped after
  one full node lifecycle (explain→ask→explain→test→remediation→
  demonstrated→advance) once that was confirmed working end to end;
  driving the remaining ~5 nodes at observed ~3-5 min/move would add
  real API time without new information (same code paths already
  exercised). Advisor-reviewed decision to stop here.

## Status: fixes committed
- `ee676a6` — `engine::chapter_match_failed` + enforcement gate in
  `prepare()` (bug d).
- `ea0d8c8` — `rec.nodeId = data;` in node.js's `exercise` SSE handler
  (new bug, not in the original report).
- `9d1bcbe` — `.tasks` sync.
Not pushed (standing directive requires explicit confirmation before
push, not just commit).

## Open items deferred to PLAN.md (not fixed this session)
1. **§S21 grounding-gate token-recovery violates §12.2** (most
   significant open item — see write-up above under "grounding-not-
   confirmed warning — ROOT CAUSED"). The gate's own recovery calls
   (regenerate + re-check, 2 of its up-to-3 extra calls) are exactly
   the "recovery path that spends an extra model call" pattern §12.2
   rules out, hitting real Groq 429s in this run. Needs a design
   decision (cap to first check only / route recovery to fast tier /
   distinguish "verification infra failed" from "content flagged" in
   the banner text), not a quick patch.
2. First-time index-build blocking the first move synchronously
   (untriggered this run — Stewart's index pre-existed — but real per
   `ensure_document_grounded`'s `missing_index` loop, reading.rs
   ~1791-1827).
3. `ask` move's quiz-like UI has no visual distinction from a real
   graded `test` exercise (cosmetic, from bug (c)'s recharacterization).
4. Possible short-content premature read-to-end (bug (b), did not
   reproduce with this run's long content — unconfirmed hypothesis).
5. Cosmetic-only CSP `style-src` console noise from model-authored
   `<span style>` (see write-up above) — confirmed harmless, optional
   pre-pass fix available if the noise itself matters.
