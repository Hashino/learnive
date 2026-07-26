# PLAN.md — learnive development plan

> Checkbox legend: `[x]` done · `[~]` partial (the essentials exist, the noted part is missing) · `[ ]` to do.

> Living document. This plan **can and should** change as development advances — especially because almost all of the project's risk is *calibration* (assessment quality, profile fidelity, cross-ref sensitivity), which is only learned by using. The `§N` references point to the sections of `SPEC.md` (the authoritative specification).

## Phasing principle

The order is not "one complete subsystem at a time", it is **full loop first, depth later**. Phase 1 exercises the central thesis end to end with minimal depth; Phase 2 deepens each subsystem to the quality of the spec, still with **a single living document**; Phase 3 adds the graph across multiple documents. Each phase is genuinely usable when it is finished.

---

## Phase 1 — Minimum complete loop (vertical slice)

**Goal:** prove the central thesis with a working end-to-end path: a topic → node generated on demand → comprehension check with a locked rubric → grading fires the next node. Minimal depth in everything; what matters is that the *cycle* closes.

**Minimal foundation (what the loop requires to exist):**
- [x] `axum` server bound only to `127.0.0.1`; mandatory session token; strict `Origin`/CORS validation; no state-changing endpoint on GET (§3.1).
- [x] Server→client streaming of generated content (§3) — the SSE format over a POST (read via `fetch`), because `EventSource` does not carry the token nor POST, and §3.1 forbids state-changing GET.
- [~] **Frontend**: minimal vanilla JS + token-by-token streaming done. Missing: vendored HTMX, the **wasm** anchoring module, the scroll-based reading line, optimistic UI (§3).
- [~] **Sandbox for generated interactive blocks (§3.1, §4.4)**: the exercise renders in an `<iframe sandbox>` (only `allow-scripts`, no `allow-same-origin`) and returns the answer via `postMessage`; prose/remediation (LLM HTML in the app origin) is **sanitized** on the client; a restrictive CSP on every response. The **HTML contract** (what the sanitizer allows/removes, and the sandbox freedom + `postMessage`) is told to the model in the prompts (`PROSE_HTML_CONTRACT`/`EXERCISE_HTML_CONTRACT` in `engine.rs`), mirroring the sanitizer so it does not generate something that disappears on render. Missing: an artifact schema locked together with the rubric and validated; server-side sanitization too (defense in depth, ideally derived from the same contract) and a CSP without `'unsafe-inline'` (externalize the inline JS).
- [~] AI provider: swappable seam + OpenRouter path + OAuth PKCE primitives ready; runs in demo mode without a key. Missing: the OAuth browser round-trip + keychain (with the setup, §12).
- [x] File storage: one directory = one living document, one HTML file per node (§4, §4.1).
- [x] **Two-layer node contract (§4.3)**: frozen content with `data-block-id` + append-only interaction; anchoring by ID with a fuzzy quote fallback; v0 vocabulary.

**Loop:**
- [~] **Cold start (§6.1)**: single question "What are we learning?" → generates an outline. Missing: the agent deciding to open a scope-negotiation conversation instead of starting directly.
- [x] Initial outline generation from the topic (§6).
- [x] On-demand node generation (§6).
- [~] Objectives generated **together** with the content; rubric locked at creation (server-only); per-objective grade in `{not demonstrated, partial, demonstrated}`; a transfer item. Missing: grounding the exercise in a real source (§8, depends on acquisition §11.1).
- [~] "Living document" UI: streamed prose + sandboxed interactive exercise + outline (§4.4, §9). Missing: the highlighted reading line and text-selection + question that edits the document (§9).
- [x] **Remediation on failure (§8.2)**: an append-only tutor thread in the exercise's context, with increasing similarity per attempt; only advances when `demonstrated`.
- [ ] **Abstraction-level calibration (§6.2)**: raise/lower abstraction per concept based on error + question rate.
- [ ] Minimal profile: record interactions and feed the recent context of the next node (§7).
- [x] Advance on grading the exercise ("next" button); fine pause/redirect is a later refinement (§9).

**Responsiveness in this phase (Phase 1 must be pleasant, otherwise it fails its purpose) (§14):**
- [x] Token-by-token streaming in the document focused on **time-to-first-token**, not time-to-complete.
- [ ] **Predictive prefetch** of the next node(s) over the outline while the user reads/answers — cost-aware (§6).
- [x] Pipeline within the node: prose (robust) streams first; exercise + rubric in a separate call (rubric locked before submission, §8).
- [x] Basic model tiering: light for exercise/grading, robust for prose (§12.1) — routed per sub-task and swappable.
- [ ] Optimistic UI: the user's action reflects in the document immediately, no blocking modal.

**Provider/model setup in this phase (§12, §12.1):**
- [~] **OpenRouter OAuth as the default option**; direct BYOK as an option. Selection via environment today; the setup screen is missing.
- [ ] Choice by **intent** (free vs paid), not by model name; recommended pairing applied automatically.
- [x] Graceful degradation: a single model serves both tiers (`Models::single`) — tiering never blocks starting.
- [ ] Basic cost control (§12.2): show running spend + a simple limit that throttles prefetch before pausing generation.

**Source grounding in this phase (crawl from the start):**
- [ ] Crawl of **LibGen (books) + arXiv (articles)** already in the loop, behind a **swappable acquisition interface** (§11.1). A simple version — without refined format preference or full normalization (Phase 2).
- [ ] **Explicit fallback to web search** when LibGen/arXiv produce no source; web content attributed inline ("according to site X ..."), links recorded in `SOURCES.md` (§11, §11.1).
- [ ] Nodes cite book/chapter or article; immutable corpus, single download reused (§11).

**Out of scope in this phase:** multiple documents and cross-referencing; the retrieval/embeddings layer; the non-destructive versioned revision chain (§5); long-term profile compaction (§7.1); complete provider onboarding (§12); a polished source viewer; the EPUB>PDF>DJVU format preference and normalization (§11.1).

**Done criterion:** a user can start from a topic, read nodes generated from real sources (LibGen/arXiv, or web with explicit attribution), be assessed, receive remediation on failure, and see the curriculum advance/adjust — all without leaving the loop.

---

## Phase 2 — Complete application, single living document

**Goal:** bring each subsystem to the quality of the spec, still within **a single living document** (no graph across documents). This is where the product becomes genuinely good; the calibration decisions learned in Phase 1 guide the depth.

**AI providers (§12, §12.1):**
- [ ] OpenRouter OAuth PKCE as the default path (one-click connection).
- [ ] Direct BYOK (Anthropic, OpenAI, OpenCode Zen) with immediate key validation and a link to key generation.
- [ ] Key storage in the OS keychain.
- [ ] Maintained recommended pairings per provider/tier + an advanced model override; minimal explanation with examples at setup.
- [ ] Free tier with rate-limit handling (queue/fallback/degradation) without breaking the session.

**Deepening the interactive generative HTML (§4.4) — the basic sandbox+protocol has existed since Phase 1:**
- [ ] Raise the quality/reliability of the generated visualizations and interactive exercises (the chosen mode — arbitrary JS always sandboxed — has variance; measure and harden the prompts/artifact validation).
- [ ] Cache/reuse of widgets and checking the artifact schema against the rubric at generation.

**Deepening responsiveness (§14) — the basics have existed since Phase 1:**
- [ ] Speculative prefetch of multiple branches with a refined cost-aware policy; separate skeleton generation (predictable) from the post-grade calibration delta.
- [ ] Per-sub-task tuned tiering (measure where the light model suffices vs. where it degrades quality).

**Deepening source grounding (§11, §11.1) — the basic crawl has existed since Phase 1:**
- [ ] Robust version selection: most recent edition, user's language when possible (fallback to another language).
- [ ] Format preference **EPUB > PDF > DJVU**; normalization of any format into the internal representation (extracted text + HTML dialect).
- [ ] **Read-only** source viewer; selecting in the source routes the cited passage to the living document (§11).

**Canonical node serialization (§4, §4.3):**
- [ ] Stable attribute order when re-emitting the node HTML (`scraper`'s `inner_html` does not preserve byte-for-byte order) — for clean diffs in the "human-readable files" spirit of §4. Today the content is normalized at ingestion and frozen; canonicalizing the output is missing.

**Versioned concept graph (§5):**
- [ ] Non-destructive revision: revisiting a concept generates a new version node; the original stays intact with anchored annotations.
- [ ] Version chain via front-matter/sequential name; future references point to the most recent tip.

**Complete curriculum engine (§6):**
- [ ] Prune/expand/reorder nodes based on the assessment; scope negotiation during generation; flexible atomic granularity.

**Complete assessment engine (§8, §8.1):**
- [ ] Synthesis exercises crossing distant nodes **within the document** (integration test).
- [ ] Rubrics for non-deterministic domains: ideological Turing test, position mapping, consistency over time.

**Complete memory / profile (§7, §7.1):**
- [ ] Immutable append-only event log + profile as a materialized projection.
- [ ] Multi-resolution memory (recent verbatim → summaries → distilled traits + per-concept retention).
- [ ] Derived/rebuildable retrieval index (embedded vector store / sqlite) — needed already here because a profile used over months requires retrieval (§4, §10).
- [ ] Decay and versioned revision of profile beliefs (reuses the §5 philosophy).
- [ ] User-inspectable and editable profile.
- [ ] Adversarial behavior: build the strongest counter-argument; distinguish legitimate disagreement from failed comprehension.
- [ ] Living-document annotations visible to the user and the AI.

**Out of scope in this phase:** multiple living documents; summaries/links/side panel across documents; cross-reference sensitivity control.

**Done criterion:** a single living document works at the full depth of the spec — real LibGen sources, versioned revision, rich assessment, a profile that survives prolonged use.

---

## Phase 3 — Multiple living documents + cross-referencing ("second brain", §10)

**Goal:** turn the set of living documents into a cross-referencing graph.

- [ ] Multiple living documents as a cross-referenced graph.
- [ ] When a learning depends on knowledge from another document: a brief summary + link, possibly opening a side panel that renders the content + the user's notes from that other document/node.
- [ ] Scale the retrieval layer (the Phase 2 index) to the whole document corpus.
- [ ] Integration exercises crossing nodes from **different documents** (§8).
- [ ] **Cross-reference sensitivity control** adjustable by the user over time — §10 marks this as the biggest risk (calibration, not technical): firing too often becomes noise, too rarely loses value.

**Done criterion:** the user navigates between living documents with useful, adjustable cross-references, without noise.

---

## Cross-cutting risks (apply to all phases)

- **Assessment quality is the cornerstone** — the entire adaptive engine adapts on top of the "understood?" signal. A weak signal = adaptation on noise. The rubric locked at generation (§8) is the mitigation; even so it is the riskiest dependency.
- **Cost/latency of atomic nodes** — a short cycle multiplies LLM calls. BYOK pushes the cost to the user, but the per-node latency is UX. Monitor the atomic ↔ number-of-rounds tension.
- **Grounding fidelity** — text extraction varies by format; the LLM can still drift from the source. Citation + visible source audit, they do not prevent.
- **Profile-compaction calibration** (§7.1) — deciding what to forget is judgment, not a closed algorithm. The immutable log makes errors recoverable; the inspectable profile makes them correctable.
