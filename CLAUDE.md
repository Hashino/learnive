# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project state

Phase 1 in progress: the minimum end-to-end loop is functional. **`README.md` is the authoritative specification** (in Portuguese) and defines the architecture the implementation must follow — read it before writing anything non-trivial. **`PLAN.md`** holds the living, phased build plan (Phase 1: minimum end-to-end loop, LibGen crawl included from the start; Phase 2: full-depth single living document; Phase 3: multiple cross-referenced documents) — check it for what's in/out of the current phase. Sections below are cross-cutting decisions that require reading multiple spec sections to grasp.

**Workspace layout:** `crates/core` (`learnive-core`) holds the §4.3 node data contract + anchoring, deliberately free of tokio/axum so it compiles to wasm and is shared with the client; `crates/learnive` is the axum binary (`security`, `store`, `ai`, `engine`, `api`, `app`). `cargo run` runs the binary via `default-members`.

**What works today:** secure server (§3.1); node parse/serialize + anchoring (§4.3); file store (§4); swappable provider with streaming + tiering + OpenRouter PKCE (§12); the loop — cold start → outline → streamed node generation → locked-rubric grading → remediation/advance (§6/§8/§8.2) — usable in the browser. Runs keyless in a **demo mode** (prompt-aware mock) when `LEARNIVE_OPENROUTER_KEY` is unset. Env: `LEARNIVE_PORT`, `LEARNIVE_DATA_DIR`, `LEARNIVE_OPENROUTER_KEY`, `LEARNIVE_MODEL_FAST`/`_ROBUST`.

**Not yet built (Phase 1 remainder):** source acquisition (LibGen/arXiv/web §11.1), profile/memory (§7), abstraction calibration (§6.2), predictive prefetch (§14), setup UI + keychain + live OAuth round-trip (§12), the wasm anchoring build + HTMX vendoring, scroll reading-line (§9), text-selection→document routing (§7/§9), cost control (§12.2).

## Commands

- Build: `cargo build` (edition 2024 — needs a recent Rust toolchain / Rust 1.85+)
- Run: `cargo run`
- Test all: `cargo test`
- Single test: `cargo test <name>` (or `cargo test <module>::<name>` to disambiguate)
- Lint: `cargo clippy --all-targets` and `cargo fmt`

## Objetivo e princípios

Learnive transforma qualquer tema, ideia ou problema que o usuário queira explorar em um currículo adaptativo, construído progressivamente como um **"livro vivo"** — um documento que cresce e se ajusta conforme a compreensão real do usuário é avaliada, em vez de entregar material fixo de antemão.

Princípios que devem guiar cada decisão de implementação (não são metas soltas — restringem como as features são construídas):

- **A sofisticação do sistema acompanha a do usuário** — calibração contínua e local (por conceito/objetivo) de ritmo e interesse, nunca um nível fixado uma vez.
- **Aprendizado é holístico, não atômico** — todo conceito novo se integra ao que já foi aprendido (ver exercícios de síntese/"integração" §8 e o grafo entre documentos §10).
- **Avaliação usa objetivos travados no momento da geração do conteúdo**, não julgamento posterior — evita a leniência natural de um avaliador sem critério pré-definido (§8).
- **Discordância legítima do usuário não é erro de compreensão** — o sistema confronta ideias dialeticamente, sem bajular (§7).
- **Conhecimento nunca é editado destrutivamente** — revisão gera novas versões, preservando a trajetória de aprendizado (§5).
- **O ciclo aprendizado → feedback é o mais curto possível** — geração em blocos pequenos e atômicos sempre que o conceito permitir, cada bloco terminando numa checagem de compreensão (§6).
- **A interação do próprio usuário é o sinal mais valioso** — o que pergunta, onde seleciona, o que erra realimenta diretamente o que vem a seguir; nunca é registrada como conversa lateral desconectada (§7).

## Architecture (from the spec)

**learnive** is a self-directed learning app: for each topic a user wants to explore it generates an adaptive "generative book" — a living document that is built incrementally, assesses real understanding at each step, and uses that to decide what comes next (including revisiting earlier concepts).

Decisions that constrain how features must be built:

- **Topology (§3):** Rust backend as a *local HTTP server* (`axum`), rendered in the user's real browser — not a Tauri/Electron webview. The backend does all filesystem I/O; the browser never touches the filesystem, only talks to the backend over localhost. Main content flow is server→client via **SSE**; user actions (select text, ask, answer) are ordinary HTTP requests.

- **Frontend = HTMX + minimal vanilla JS + a shared-Rust wasm anchoring module (§3).** No JS VDOM framework (React/etc.): the canonical data *is* the HTML dialect (§4.2), so a JSON-state VDOM is impedance — the server generates the dialect, the client displays it. HTMX is the backbone (server→client HTML swaps, SSE, forms — mirrors the §3 flow); a thin vanilla-JS layer covers the genuinely client-side state (text selection, scroll-based reading line, token-by-token streaming, optimistic UI); the §4.3 anchoring (block-id + fuzzy quote resolution) is compiled to **wasm from the same Rust crate** so it's written once, no JS reimplementation/drift. No page build step; assets embedded in the binary (§15 portability).

- **Local-server security is mandatory, not optional (§3.1):** the server holds the user's API keys and is reachable by any browser tab, so it must defend against CSRF/DNS-rebinding. Bind only to `127.0.0.1`; require a session token on every request (Jupyter-style); strict `Origin`/CORS validation (never `*`); no state-changing endpoint responds to GET.

- **Storage is human-readable files, no binary DB (§4), Obsidian-style.** HTML for living documents (generated content + user annotations), PDF for immutable source books. Directory layout is only for loose human navigation — real graph relationships (prerequisite, cross-reference) live as `[[links]]` *inside* content, never implied by file position. One HTML file per concept node, inside one directory per living document.

- **App-specific HTML dialect (§4.2):** generated HTML uses a semantic tag/attribute convention (node IDs, version-chain pointers, learning-objective tags on spans, user-annotation spans, source-citation markers) — parse it in Rust via `html5ever`/`scraper`/`lol_html`, not ad-hoc string handling.

- **Data model is a versioned concept graph (§5).** The reading order shown to the user is a *linearization* (traversal) of the graph and can be re-planned as the graph changes. **No destructive edits:** revisiting an old concept creates a *new version node* in that concept's chain; the original node stays intact so user annotations remain anchored. Outside this revision mechanism the system only moves forward (incremental generation from the current reading point, never in-place rewrite).

- **Node file = two layers (§4.3).** A **content layer** frozen at creation (prose, objectives, exercise, citations; every block has a stable `data-block-id`) plus an **append-only interaction layer** (annotations, Q&A, remediation) that *references* content IDs and never mutates them. This resolves the §5-immutable vs §9-"AI edits the document" tension. Anchoring is by stable block ID (survives streaming/regen because content is frozen), with fuzzy quote+prefix/suffix for sub-block selections. The reading-highlight line is ephemeral client UI state, **not persisted**. A v0 tag/attribute vocabulary is sketched in §4.3 — treat it as the concrete data contract to nail down first.

- **Generative interactive HTML, always sandboxed (§4.4).** Full use of generated HTML: content can include bespoke **interactive visualizations** when they teach better (not just prose rewrites of sources), and exercises are **not limited to checkbox/textbox**. Strategy chosen: **arbitrary LLM-generated JS, always sandboxed** (no fixed component library). Two invariants this must respect: (1) **Security (§3.1)** — generated JS never runs in the app origin; it renders in a `<iframe sandbox>` with no same-origin/token access and reports back only via a narrow validated `postMessage` channel. (2) **Gradeability (§8)** — an interactive exercise must emit a **structured answer artifact** via `postMessage` whose schema is generated *together with* the rubric and locked *before* submission; the rubric grades the artifact, so interactivity never becomes eyeballed grading. Interactive blocks are frozen content-layer elements with stable `data-block-id` (§4.3); their internal fiddling is ephemeral client state (only the answer artifact is persisted). Heavier widgets hydrate after prose so they never hurt TTFT (§14).

- **Assessment rubric is locked at node-creation time (§8),** generated *together with* node content, to avoid LLM-as-grader leniency. Every "application" objective includes at least one item requiring transfer to a scenario *not* covered by the node text. Grades are per-objective `{not demonstrated, partial, demonstrated}`, not pass/fail; exercises are grounded in the same source material (§8) — clean for exact sciences, looser elsewhere.

- **Failure → remediation conversation (§8.2), not silent retry.** On a failed check, the node opens a tutor conversation in its interaction layer: explain the concept *in the exercise's context* (worked example / step-by-step of the failed problem), then propose a new similar problem whose similarity to the worked example **increases with each successive failure** (scaffolding converges toward the demonstrated solution, then difficulty ramps back up). Which explanation finally landed is high-signal for the profile (§7). Next node only fires once the objective is `demonstrated`.

- **Cold start (§6.1):** one generic prompt ("O que vamos aprender?") + textbox. The agent decides from the input whether to start the living document directly or open a scope-negotiation conversation until the outline is defined.

- **Abstraction calibration is the concrete hook for §2 (§6.2).** Signal = error+question rate per concept. Advancing with no errors/questions ⇒ abstraction too low ⇒ richer language, more superficial explanation, harder atomic+integration exercises. Inverse when struggling. Continuous and local per concept, driven by the profile (§7), parameterizes generation (§6).

- **User-profile / pedagogical-trajectory memory (§7)** accumulates from all interactions. Two behaviors to actively enforce against default LLM tendencies: (1) be *adversarial* — build the strongest counter-argument to the user's position rather than mirroring/flattering; (2) distinguish legitimate disagreement from failed comprehension. Text-selection-plus-question is the highest-signal input and must directly steer the next generated node, never be logged as disconnected side chat.

- **Long-term memory architecture (§7.1)** — the profile is NOT a replay of all interactions. Event-sourcing split: an **immutable append-only event log** (source of truth on disk, never fully loaded) vs. a **materialized profile projection** (compact, curated, the thing actually fed into generation). Multi-resolution (recent verbatim → mid-term summaries → distilled traits + per-concept retention); retrieved by relevance (reuses the §10 layer), not loaded whole. Profile beliefs are timestamped and **decay/revise** — reuse the §5 "new version, non-destructive" pattern for beliefs about the user, not just concept nodes. The profile is **user-inspectable and editable** (human-in-the-loop is the cheapest fix for drift/bad compaction). Residual risk is calibration of lossy compaction, not scale; the immutable log makes compaction errors recoverable, the inspectable profile makes them correctable.

- **Files are the source of truth; indexes are rebuildable derived caches (§4, §10, §7.1).** Retrieval at scale needs a binary index (embedded vector store / sqlite) — this does NOT violate the "no proprietary DB" stance because the index is always reconstructible from the files; deleting it only forces reindex. Same retrieval layer serves both cross-document context (§10) and profile memory (§7.1).

- **Source grounding via swappable acquisition module (§11, §11.1).** Node content is grounded in real sources cited by book+chapter or article. Acquisition is agent-driven, not manual: **LibGen** (books) + **arXiv** (papers), each an implementation behind one swappable acquisition interface (LibGen is *not* welded in — matters for the §15 hosting endgame and §16 legal risk). Fallback chain, **explicit to the user**: LibGen/arXiv → if no adequate source, internet search; web-grounded content is attributed inline ("segundo o site X ...") and its links tracked in a `SOURCES.md`. Preferred format when several exist: **EPUB > PDF > DJVU**. Regardless of format, ingestion **normalizes** everything to one internal representation (extracted text + the app's HTML dialect), so source format is an acquisition detail. Sources join the immutable corpus (§4), are fetched once and reused, and feed citation + the retrieval/embeddings layer (§10).

- **The living document is the only place for user notes (§9, §11).** The source viewer is **read-only** — no annotation on the source (it's immutable). Selecting text in the source is interaction, not a persistent mark: it routes into the living document (inserted/answered there, already cited by book+chapter). This keeps a single notes store and ensures user marks stay high-signal for §7, never trapped on a static source. Because the viewer is read-only, EPUB's reflow view has no downside — no need for a pixel-faithful page.

- **AI provider is bring-your-own (§12).** OpenRouter OAuth PKCE is the default path; direct Anthropic/OpenAI and OpenCode Zen are BYOK. Do **not** build on subscription OAuth tokens (Claude Pro/Max, ChatGPT Plus). Keys are stored in the OS keychain, never a centralized DB.

- **Model tiering, set up by intent not by model name (§12.1).** Two tiers — a light/fast model for frequent cheap tasks (exercise generation, grading against the rubric, summaries, embeddings, cross-ref decisions) and a robust model for explanatory prose and adversarial confrontation. Non-technical users never pick two models by name: the app ships maintained recommended pairings per provider/tier, and setup asks one high-level intent question (free vs paid) that derives both tiers. Manual two-model config is an optional advanced override; if only one model is available it serves both tiers (**tiering is an optimization, never a barrier to start**).

- **Latency is a UX/architecture problem, not a model problem (§14).** The app must be pleasant on today's autoregressive API models; a faster model (e.g. diffusion LLMs) is a future, non-load-bearing lever available only once the stack is self-hosted. Target metric is **time-to-first-token / time-to-reading (~1s)**, not time-to-complete. Levers: stream token-by-token so reading outpaces generation (§3); **predictive prefetch** of likely next nodes over the outline while the user reads/answers (§6), cost-aware because speculative work is the user's money under BYOK; pipeline prose vs. exercise+rubric (rubric must be locked before submission, not in the same call, §8); model tiering (§12.1); optimistic UI. Treat the model backend as a **swappable, per-subtask-routed knob**, not a dependency. Atomic nodes (§6) mean more round-trips — these levers are what make that density affordable.

- **Cost control (§12.2) & endgame (§15).** BYOK + deliberately token-heavy → user sets daily/weekly/monthly spend caps and sees running spend; hitting a cap throttles speculative prefetch first, then pauses generation. Endgame: desktop-first now, later self-hosted with Monero donations (TPB/LibGen-style) → keep everything local-first/portable and the acquisition module swappable. **§16 tracks open decisions/risks** (concurrency baseline, LibGen legal isolation, grounding in non-exact domains, evaluator-failure telemetry, backup/sync) — consult before building in those areas.
