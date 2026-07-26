# learnive — Specification of the Self-Directed Exploratory Learning Platform

## 1. Overview

A self-directed learning application that generates, for every topic/idea/problem the user wants to explore, an adaptive curriculum in the form of a "generative book". The system does not deliver a fixed curriculum: it builds the material progressively, assesses the user's real understanding at each step, and uses that assessment to decide what comes next — including revisiting and revising already-taught concepts.

## 2. Target audience and central design principle

- Primary target user: a polymath seeking a holistic learning curriculum that they control themselves.
- Design principle: it is **not** a system designed "for sophisticated users" — it is a system that must be **as sophisticated as the user is**. The same need to calibrate pace and to use personal interests to make learning enjoyable applies equally to someone gifted and to someone with a learning difficulty. Calibration is continuous and local (per concept/objective), not a general level fixed once.

## 3. Overall architecture

- **Language**: Rust — makes it easy to compile binaries for multiple operating systems.
- **Topology**: a Rust backend running locally as an HTTP server; the frontend rendered in the user's own browser (not an embedded webview like Tauri/Electron). The backend does all file reading/writing — the browser never touches the filesystem directly, it only talks to the backend over the local network.
- **Streaming**: Server-Sent Events (SSE) to push incrementally generated content to the on-screen document — the main flow is server→client. User actions (selecting text, asking, answering an exercise) are ordinary HTTP requests.
- **HTTP framework**: `axum` (native SSE support).
- **Frontend**: **HTMX** as the backbone (server→client HTML swaps, SSE, forms — HTMX's model is exactly the §3 flow) + **minimal vanilla JavaScript** only for what is genuinely client state (text selection, scroll-based reading line, token-by-token streaming, optimistic UI) + a **WebAssembly module** compiled from the same Rust crate for the §4.3 anchoring (block/quote resolution written once, no JS reimplementation and no drift). **No JS VDOM framework (React/etc.)**: the canonical data is the HTML itself (§4.2), so a VDOM that wants JSON state as the source of truth would be impedance — the server generates the dialect, the client displays it. No build step for the page; assets embedded in the binary (portability §15).

### 3.1 Local-server security

The server holds the user's API keys and runs as HTTP reachable by any browser tab — it needs protection against CSRF/DNS-rebinding:

- Bind exclusively to `127.0.0.1`, never `0.0.0.0`.
- Mandatory session token on every request (the same pattern Jupyter uses: token in the URL/cookie).
- Strict `Origin`/CORS validation — never `Access-Control-Allow-Origin: *`.
- No state-changing endpoint responds to GET (prevents CSRF via a plain image/link tag).
- **Generated interactive content runs in a sandbox.** LLM-generated interactive HTML/JS (§4.4) **never** executes in the app origin: it goes in an `<iframe sandbox>` without `allow-same-origin`, with no access to the session token, cookies, or endpoints; it returns a result only over a narrow, validated `postMessage` channel. Without that isolation, a generated (or source-injected) script could exfiltrate the key/token.
- **Non-interactive generated HTML (prose, remediation) is sanitized before entering the app origin.** Prose becomes part of the document (selectable, anchorable §4.3), so it lives in the app origin — but it is still LLM output (potentially poisoned by a source §11.1). Before inserting, the client removes `<script>`, event attributes (`on*`) and dangerous URLs; only **interactive blocks** carry script, and only inside the sandbox. Reinforced by a **restrictive CSP** on every response (`default-src 'self'`, `connect-src 'self'` blocks exfiltration to another origin, `object-src`/`base-uri 'none'`).

## 4. Storage format

- A human-readable directory-and-file structure — no proprietary binary database — in the spirit of how Obsidian uses markdown files for the "second brain".
- **HTML** for the living documents (generated content + user annotations).
- **PDF** for the source books used as reference (original, immutable material).
- Directories/subdirectories serve only loose human navigation. The real graph relationships (prerequisite, cross-reference) are expressed as links inside the content, never implied by filesystem position — the same logic as Obsidian: the folder does not carry the graph, the `[[links]]` inside the files do.
- **Files are the source of truth; indexes are derived artifacts.** Retrieval at scale (§10) and the long-term profile (§7.1) require a binary index (embedded vector store / sqlite) for performance — this does not contradict "no proprietary database". The index is always a **rebuildable cache** from the files; deleting it never loses data, it only forces reindexing. The Obsidian spirit is preserved because the canonical data stays in readable files.

### 4.1 File granularity

One HTML file per concept node, inside one directory per living document. It maps directly onto the versioned-graph model (section 5). The version chain is expressed via sequential filename or front-matter pointing to the previous/next node.

### 4.2 The application's own HTML dialect

The generated HTML follows its own convention (semantic tags/attributes), it is not loose HTML — needed for: node ID, version-chain pointer, learning-objective tag associated with a span, user-annotation span, citation marker for source book/chapter. Parsing in Rust via `html5ever`/`scraper`/`lol_html`.

### 4.3 Node layer model and anchoring

Resolves the apparent tension between §5 (immutable node) and §9 (the AI "edits the document", the user annotates): a node is an HTML file with **two logical layers**.

- **Content layer — immutable after creation.** The generated prose, the learning objectives on spans, the exercise/form, and the source citations. Frozen at the moment the node is created (§5). Every addressable block carries a **stable, unique ID** assigned at generation (e.g. `data-block-id`). That ID is the anchoring target.
- **Interaction layer — append-only.** Everything that accumulates afterwards: user annotations, the remediation conversation (§8.2), Q&A threads the tutor writes. These are elements that **reference** content-layer IDs and never alter them. Append-only = new elements are added, existing ones are not rewritten (preserves the trajectory, §7.1).

**Anchoring:**
- Primary: by **stable block ID** — survives streaming, regeneration, and reflow, because the content layer is frozen, so the ID is permanent.
- Sub-block (a span inside a paragraph): **fuzzy quote** anchor — exact quote + prefix/suffix context (W3C Web Annotation / hypothes.is style), resolved against the frozen block text. Since the block is immutable, it resolves deterministically; the fuzzy part is only robustness against minimal normalization.

**Version chain (§5):** revising a concept generates a new node file whose front-matter/attribute points to the previous version's ID. The old file (both layers) stays intact — annotations remain correctly anchored to the frozen content they were made on. Future references resolve to the tip of the chain.

**v0 vocabulary (illustrative, to be refined in implementation):**
- Node root: `<article data-node-id data-doc-id data-prev-version>`
- Content block: any element with `data-block-id`
- Learning objective: `<span data-objective-id data-objective-type="knowledge|application|synthesis">` wrapping the span a rubric item targets
- Citation: `<cite data-source-id data-locator="chap:3;p:42">`; for a web source (§11.1), `data-source-url`
- Exercise: `<form data-exercise-id data-rubric-id>` with generated fields
- Interaction layer: `<aside data-annotation-id data-anchor-block data-anchor-quote>` (annotation); `<div data-thread-id data-anchor-block>` (Q&A/remediation)
- **The highlighted reading line (§9) is NOT persisted** — it is ephemeral UI state (scroll position), lives only on the client, and does not enter the node file.

### 4.4 Generated interactive blocks

The app uses **generative HTML to the fullest** (§9): beyond prose, the content can contain **interactive visualizations** when that teaches better (a manipulable chart, a simulation, a diagram), and the **exercises have free modality** — they are not limited to checkbox/textbox. That is the point of generating HTML instead of markdown: the node is not just a prose rewrite of the sources.

Generation strategy: **arbitrary generated JS, always sandboxed.** The LLM writes a bespoke widget per block; there is no fixed component library (maximum expressiveness, uniform security model).

- **Layer and immutability (§4.3):** an interactive block is content **frozen at creation**, with a stable `data-block-id` (e.g. `<figure data-block-id data-interactive>` wrapping the sandbox `srcdoc`). Its internal manipulation (dragging, simulating, exploring) is **ephemeral client state** — like the reading line, it is not persisted. Only the **answer artifact** of an exercise is captured.
- **Security (§3.1):** it runs in an `<iframe sandbox>` isolated from the origin and the token; it communicates only via `postMessage`.
- **Result protocol (exercises, §8):** every interactive exercise emits a **structured answer artifact** via `postMessage` (final state, action sequence, target reached, submitted value). The **schema of that artifact is generated together with** the content and the rubric and **locked before submission** — interactivity must not become "eyeballed" grading (§8). The burden falls on generation: to produce, when creating the exercise, both the rubric and the schema of the artifact it grades.
- **Latency (§14):** the prose streams first (TTFT ~1s); heavier widgets **hydrate afterwards**, never blocking the time-to-reading.
- **Grounding (§11):** a visualization is also grounded/citable — it visualizes source material and carries the citation like any block.

## 5. Data model: versioned concept-node graph

- The curriculum is internally a **graph**: concept nodes with prerequisite/relation edges between them.
- The reading presented to the user is a **linearization** (traversal) of the graph at that moment — it can be re-planned as the graph changes, without breaking the feeling of "a book being read".
- **No destructive editing**: when the user revisits an old concept with a new question (informed by later learnings), the system generates a **new version node** in that concept's chain. The original node stays intact, with the user's annotations still correctly anchored to it. From that point on, future references to that concept point to the most recent tip of the chain.
- This preserves the pedagogical-trajectory history (section 7) and avoids the problem of annotations anchored to text that changes underneath them.
- Outside this revision mechanism, the system moves **only forward** — incremental generation from the current reading point, never a retroactive in-place rewrite.

## 6. Curriculum engine

- **Outline generation**: a provisional hierarchical map/graph is generated at the start (from the user's initial topic/idea/problem), serving as a skeleton. Each real content node is generated only when it is its turn to be read, using the map as a guide — able to prune, expand, or reorder nodes as the assessment (section 8) reveals what the user already knows or does not retain.
- **Scope negotiation**: happens not only at the initial outline generation, but occasionally during the generation of future nodes as well.
- **Flexible generation granularity**: not fixed to a "chapter" format. Simple concepts may occupy a single node; complex concepts that do not split well into large units are decomposed into smaller, more atomic blocks whenever possible. The goal is to keep the learning → feedback cycle as short as possible: each node, whatever its size, ends with a comprehension check, instead of accumulating several concepts before testing.

### 6.1 Cold start / topic entry

The initial screen is **a single generic question** ("What are we learning?") and a text box. From what the user types, the agent adaptively decides between two paths:

- **The living document starts directly**: when the topic is already clear/bounded enough to generate an outline.
- **A conversation session with the tutor**: when it is still vague/broad, a scope-negotiation conversation (§6) runs until the starting point and the document outline are defined.

The choice between the two is made by the agent itself from the input — it is the lightweight front of the same scope-negotiation machinery that §6 already describes.

### 6.2 Abstraction-level calibration

Concrete mechanism for the "sophistication follows the user" principle (§2). It is continuous and local (per concept/objective), never a global level fixed once.

- **Signal**: error rate + questions per concept. Advancing **without errors and without questions** ⇒ the abstraction level is too low.
- **Response when it is too low**: language richer in meaning; content explained more superficially (less hand-holding); exercises that demand greater understanding — both atomic (deeper into the isolated concept) and integrative (§8, synthesis crossing nodes).
- **Inverse when the user gets stuck**: lower abstraction, more detailed/explicit explanation, more scaffolded exercises.
- The calibration lives in the profile (§7) and parameterizes generation (§6).

## 7. Memory system / user profile

All of the user's interactions — questions asked, answers to exercises, requests to change the curriculum, annotations in the document — feed a cumulative pedagogical-trajectory profile, used to:

- Anticipate, when generating future nodes, the kind of question that user tends to ask.
- Measure what kind of explanation/exercise produces the most learning for that user specifically.
- Identify how the user thinks and **explicitly confront them** with their philosophical basis/premises and their limitations — adversarially: the system builds the strongest counter-argument against the user's position, not merely describing it generically or complacently (LLMs tend to flatter/mirror instead of challenge; this behavior must be actively avoided).
- Recognize what generates the most interest in the user.
- Explicitly distinguish **legitimate disagreement** from **failed comprehension**: if the user demonstrates understanding the mechanism of a concept but diverges from it, that is not treated as an unmet objective — it becomes a position recorded in the profile, and can feed a future node that confronts it dialectically.
- **Text selection + question is the system's most important signal**: it is explicit, localized curiosity/confusion, more informative than any implicit inference. It directly influences the direction of the next generated node — it is never recorded merely as side chat disconnected from the curriculum.

### 7.1 Long-term memory architecture (months/years)

The profile is not a replay of all interactions — that is unsustainable and unnecessary. Split into two layers, in the spirit of event sourcing / materialized view:

- **Immutable, append-only event log** (questions, answers, selections, change requests, annotations) = the source of truth on disk. Never loaded whole into context. It is what lets a nuance discarded by an earlier compaction be recovered.
- **Profile = a materialized projection** of that log: compact, curated, it is what actually enters node generation. A *model* of the user maintained incrementally, not the raw history.

On that basis:

- **Multi-resolution memory**: recent interactions at high fidelity (verbatim); mid-term as per-session/topic summaries; long-term as stable distilled traits + per-concept retention state (analogous to spaced repetition). A node generated months later consumes the distilled conclusion, not the verbatim exercise.
- **Retrieval, not whole context**: the slice of the profile relevant to the current node's topic is retrieved (the same retrieval layer as §10), not the whole profile.
- **Structured where possible, prose only where needed**: per-concept retention, recorded positions/disagreements (§7, §8.1), and interest tags are structured, queryable data; only the "how this user thinks" is LLM-maintained prose — short and periodically re-synthesized from the structured signals + recent events.
- **Decay and revision, not just accumulation**: profile beliefs have a timestamp and can become false over time (the user changes over years). The profile revises/deprecates beliefs by reusing the §5 philosophy — no destructive editing: a revised belief becomes a new version in the chain, the old one stays in the history. The quality of the adversarial feature (confronting the user's premises) is limited by the profile's fidelity; targeting a strawman of the user is the worst failure mode.
- **User-inspectable and editable profile**: since everything is already visible to the user and the AI (§9), the model the system holds of the user is exposed and correctable by them. Human-in-the-loop is the cheapest mitigation for drift and bad compaction — and consistent with the thesis that the user controls their own curriculum.

The residual bottleneck here is not scale/storage (solved by the techniques above), but **calibration of lossy compaction**: deciding what to distill/forget is judgment, not a closed algorithm. The immutable log guarantees compaction errors are recoverable; the inspectable profile guarantees they are correctable.

## 8. Assessment engine

- **Learning objectives generated together with the node content**, not afterwards — the grading rubric is locked at the moment the node is created, avoiding the leniency bias of LLM-as-grader (an LLM tends to validate shallow answers as correct when it judges without a pre-defined criterion).
- Every "application" objective has at least one item requiring transfer to a scenario **not covered in the node text** — that is the test that generalizes to any domain, because it is not satisfiable by memorization/pattern recognition.
- **Unit/integration metaphor**: a node objective = a unit test (was the isolated concept understood?). An occasional synthesis exercise crossing distant nodes in the graph = an integration test (can the user connect learnings from different contexts into a new application?).
- **Grounding exercises in the original material**: the exercise and its solution are grounded in the same source (book/chapter) that backs the node, making the rubric more objective and reducing leniency. It works well in exact sciences; in less deterministic areas the grounding is looser and the weight falls on the §8.1 rubrics — a recognized, not solved, limitation.
- **Structured per-objective grade**: each objective is graded on `{not demonstrated, partial, demonstrated}`, not just pass/fail. Advancing requires every objective demonstrated; any one not demonstrated triggers remediation (§8.2). The grade feeds the per-concept retention state (§7.1).
- **Free exercise modality, but always gradeable (§4.4, §9):** the exercise can be an arbitrary interactive widget (drag-to-order, label a diagram, manipulate a simulation until a target is reached, draw), not just checkbox/textbox. Whatever the modality, it emits a **structured answer artifact** (via `postMessage`, §4.4) whose schema is locked **together with the rubric, before submission**; the rubric grades the artifact. Interactivity broadens how the user answers, it **never** replaces the pre-defined criterion.

### 8.1 Assessment in non-deterministic domains (philosophy, ethics, etc.)

- **Ideological Turing test**: the user articulates the position opposite to their own as strongly as possible; the rubric assesses whether the articulation would be recognized as fair by a real proponent of that position — testing genuine understanding separated from agreement.
- **Mapping a position against known territory**: the rubric defines the space of defensible positions in a debate (not a single right answer) and assesses whether the user recognizes where they sit in that space and why.
- **Consistency over time** as the analog of the integration test: it checks whether the user's position stays coherent when reexamined from different angles at different times.

### 8.2 Remediation on failure

When the user fails a comprehension check, the system does **not** just advance nor silently regenerate: the node enters a **conversation session with the tutor**, in the node's interaction layer (§4.3), in the context of the failed exercise.

- The tutor explains the tested concept **in the exercise's context**: it gives examples of how to use the concept to solve a similar problem, or shows the step by step of solving exactly the problem the user got wrong.
- Then it proposes a **new similar problem**. With each subsequent failure, the new problem becomes **increasingly similar** to the worked example — converging toward the demonstrated solution — until the user can solve an almost identical case; then the difficulty ramps back up. (Scaffolding by increasing proximity to the model, gradually withdrawn as the user succeeds.)
- The conversation is a high-value signal for the profile (§7): which explanation/example finally "landed" feeds "what explanation works for this user".
- It is **append-only** in the node (§4.3): the history of the difficulty is preserved in the trajectory (§7.1).
- Only when the objective becomes **demonstrated** (§8) does the next node fire (§9).

## 9. Interface — the "living document"

- **Full use of generative HTML.** The content is not just a prose rewrite/combination of the sources: when it teaches better, the node renders **interactive visualizations** (manipulable charts, simulations, diagrams) generated bespoke. Likewise, the exercise is **not limited to checkbox/textbox** — its modality (chatbox, form, interactive widget) varies per content/domain, decided at generation, not fixed by the system. Generated interactive blocks follow the sandbox + answer-artifact contract of §4.4 (and, for exercises, the locked rubric of §8).
- A central highlighted line follows the user's current reading position (based on scroll position, not eye-tracking).
- The user can, at any moment: select a block of text and say something, or say something without a selection (context = the highlighted line).
- The AI's response **edits the document itself** to contain the answer — it does not appear in a separate widget.
- The document works simultaneously as reference material and personal notes: the user can annotate/mark directly on it. It is the **only** place for the user's notes — the original source (§11) is read-only; anything the user wants to record from the source is brought into the living document.
- Annotations and marks are visible both to the user and to the tutoring AI that grades and generates text.
- **Next-node generation trigger**: automatic as soon as the exercise is graded, but the user can pause or redirect the curriculum at any time.

## 10. Second brain — the graph across living documents

- The user has multiple living documents, functioning as a cross-referencing graph.
- When a learning depends on knowledge/skill already learned in another document, the system shows a brief summary with a link — possibly opening in a side panel that renders the content + the user's notes from that other document/node.
- Above a certain accumulated volume, the set of documents no longer fits whole in any model context window — a retrieval layer (embeddings + index) is needed to decide what to bring in as context at each moment. The specific technology (vector store, embedding model, PDF chunking) is left to the implementation. That index is a derived artifact rebuildable from the files (§4), not the source of truth — and it is the same retrieval layer reused by the profile memory (§7.1).
- The biggest risk of this layer is not technical, it is calibration: firing cross-references too often becomes irritating noise; too rarely loses the feature's value. It needs a sensitivity control adjustable by the user over time.

## 11. Grounding in sources (real books and articles)

- The preferred "ground truth" of the generated content is real sources written by humans: books and articles (§11.1).
- The nodes are prose rewrites/combinations of the sources' content, citing which book/chapter or article the information came from. For content grounded in web search (fallback, §11.1), the attribution is **explicit and inline** ("according to site X ...") pointing to the link.
- A side panel allows the user **only to view** the original source to check it — with no annotation function on the source. The living document (§9) is the only place for the user's notes: the source is an immutable corpus (§4), notes live in the living document.
- **Selecting in the source is interaction, not a persistent mark**: the user can select a passage of the source in the viewer and act on it (ask, request an explanation, "bring this into my document"). The action routes to the living document — the passage is inserted/answered there, already **cited** (book + chapter). No persistent mark is written on the source.
- Since the notes live in the living document but the node cites the chapter (and can deep-link to the exact passage in the source), the marginalia is not lost: the note points back to the location in the source without duplicating the notes store.

### 11.1 Book acquisition / origin

- **Origin**: crawling **Library Genesis (LibGen)** for books and **arXiv** for articles. The AI agent decides, during generation, which sources are pertinent to ground a node/topic and downloads them on demand, adding them to the application's corpus.
- **Swappable acquisition module**: each source (LibGen, arXiv, web search) is one implementation behind a common acquisition interface. LibGen is not welded to the system — it can be replaced/extended without touching the rest (it matters for the hosting endgame §15, and for the legal risk §16).
- **Fallback chain, explicit to the user**: LibGen/arXiv → if no adequate source is found, an **explicit fallback to internet search**, signaled to the user. Web-grounded content is always attributed inline ("according to site X ...", §11).
- **`SOURCES.md`**: the links of the web-search results used are recorded in a `SOURCES.md`; the references in the living document point to those links. It keeps traceable where each statement not covered by a book/article came from.
- **Version selection**: it downloads the most recent edition/version available and, whenever possible, in the user's language (falling back to another language when there is none in the user's).
- **Preferred format**: among the formats available for the same book, the preference order is **EPUB > PDF > DJVU**. EPUB is structured XHTML (clean text extraction, native chapters via spine/TOC, smaller download, direct fit into the app's HTML stack/dialect §4.2). PDF is the universal fallback with good per-page citation. DJVU is accepted only when it is the only format (a *scan* format, without structure, dependent on embedded/own OCR). Since the source viewer is read-only (§11), EPUB's reflow has no downside — no pixel-faithful typeset page is needed.
- **Normalization**: regardless of the source format, ingestion normalizes everything to one internal representation (extracted text + HTML in the app dialect). EPUB arrives nearly ready; PDF goes through extraction; DJVU through OCR/conversion. After ingestion, the source format becomes an acquisition detail, transparent to the rest of the system.
- **Ingestion into the corpus**: the downloaded book joins the immutable source corpus and becomes available for citation (§11) and for the retrieval/embeddings layer (§10). The same book is downloaded once and reused across nodes and living documents.
- The download is the agent's decision, not a manual user action — it is part of the generation flow, triggered when the material already in the corpus does not cover what the node needs to ground.

## 12. AI provider integration (bring your own AI)

- The user brings their own AI provider — the application does not pay for usage.
- **Direct Anthropic and OpenAI**: API key only (BYOK). Subscription OAuth tokens (Claude Pro/Max, ChatGPT Plus/Pro) in third-party tools are outside Anthropic's terms of use (actively banned since April 2026) and are not a generic, publicly available OAuth flow on OpenAI's side (limited to Codex) — the integration is not built on top of those flows.
- **OpenRouter**: the main/default path — an official, documented OAuth PKCE flow, made specifically for third-party apps, connects the user's account in one click, without copy/pasting a key. It gives access to Anthropic, OpenAI, and dozens of other providers behind a single integration, including free models (`:free` suffix, rate-limited).
- **OpenCode Zen**: an additional free/paid provider option — a simple API key (no credit card for the free tier), an OpenAI-format-compatible endpoint.
- **Direct BYOK** (Anthropic Console, OpenAI Platform, OpenCode Zen) as a secondary advanced option, with immediate key validation before saving and a direct link to each provider's key-generation page.
- Key storage: local-first architecture — the key is kept in the operating system keychain, not in a centralized database.

### 12.1 Model configuration (model tiering) for non-technical users

The system uses **two model levels** (§14): a light/fast one for frequent, cheap tasks (generate an exercise, grade against the rubric, summaries, embeddings, cross-ref decisions) and a robust one for the explanatory prose and adversarial confrontation. But the user **does not pick two models by name** on the common path — tiering is the system's concern, not a setup step.

- **App-recommended and maintained pairings**, per provider/tier (fast + robust), updated as the model landscape changes. OpenRouter one-click: connect the account and done, zero model choice. Direct BYOK: the app auto-selects that provider's fast/robust pair. Free tier: a `:free` pair.
- **The user expresses intent, not models.** A high-level question at setup — "free models (slower, with usage limits) or your paid account (faster, costs per use)?" — derives *both tiers* from a single choice.
- **Minimal explanation with examples on the setup screen**: it shows which pair will be used by default and a one-line why (fast for checks, strong for teaching); editable, but **not required**.
- **Manual two-model config is an advanced, optional override** — for the user who knows what they want. It is accepted that this user needs the knowledge; the non-technical user never sees a model name.
- **Graceful degradation with a single model**: if only one model is available/configured, it serves both tiers. **Tiering is an optimization, not a requirement** — it is never a barrier to start.
- **The free tier respects rate limits**: the `:free` pair must handle usage limits (queue/fallback/degradation), without breaking the study session.

### 12.2 Cost control and visibility

Since it is BYOK and the app is *deliberately* token-heavy (atomic nodes, speculative prefetch, confrontation, retrieval), the user on a paid key must not be surprised by the bill.

- **Spend configuration/visualization screen**: the user sets **daily/weekly/monthly** limits and sees how much they have already spent in each window.
- **Enforcing the limits**: as the cap is approached/reached, the system throttles **speculative prefetch first** (§14), then warns/pauses generation — degrading responsiveness before blocking study.
- **Running cost always visible** so the token-heavy design is never opaque.
- **The free tier** shows rate-limit status instead of a monetary value.

## 13. Implementation approach

- **Full loop first, depth later.** Development starts with a vertical slice that closes the central cycle end to end (topic → node generated on demand → check with a locked rubric → grading fires the next node), and only then is each subsystem deepened to the quality of this spec. The reason: almost all of the project's risk is *calibration* (assessment quality, profile fidelity, cross-ref sensitivity), and that is only learned by using — building the elaborate machinery before knowing which signal predicts learning is expensive and probably wrong.
- The final goal remains the complete application (not just the isolated central engine); the phasing is about the *order* of construction, not about delivering less.
- **The LibGen crawl (§11.1) is part of the loop from the start** — grounding in real sources is not deferred; what is deepened later is the quality (format preference, normalization, viewer).
- The detailed, living phase plan is in `PLAN.md` (phase 1: minimum loop; phase 2: complete application with a single living document; phase 3: multiple living documents with cross-referencing).

## 14. Latency and responsiveness

The application must be **pleasant to use now**, on autoregressive API models with real latency — a study session where most of the time is spent watching a spinner is a failure. This is an **architecture/UX problem, not a model problem**: the faster model is a future lever that cannot be controlled while the stack is not self-hosted, so it **cannot be load-bearing**. The goal is not to eliminate latency, it is to make the user **almost never blocked**. The target metric is **time-to-first-token / time-to-reading** (~1s), not time-to-node-complete.

Levers, from biggest to smallest:

- **Streaming + optimizing TTFT (already in §3).** Human reading is slower than the prose generation rate; if the node streams token by token while the user reads, they never catch up to the spinner. Perceived latency becomes just the time-to-first-token.
- **Predictive prefetch over the outline (§6).** While the user reads node N and does the exercise (seconds to minutes of human time), the likely next node(s) are generated in the background. Separate **"what comes next"** (predictable from the outline → generate ahead) from **"how to calibrate"** (depends on the assessment → a small delta post-grade). The prefetch depth/breadth is **cost-aware/adjustable**, because under BYOK wasted speculative work is the user's money.
- **Pipeline within the node.** Prose streams first; exercise + rubric are generated in parallel while the user reads. §8 requires the rubric **locked before submission**, not in the same LLM call — so this preserves the invariant without serializing the wait. Grading overlaps with the prefetch. **Interactive visualizations/widgets (§4.4) hydrate after the prose** — they never delay TTFT.
- **Model tiering (§12.1).** A light/fast model for the frequent tasks (exercise, grading against the rubric, summaries, embeddings, cross-ref); a robust model only for prose and adversarial confrontation. Since most of the atomic loop's rounds are the small ones, this directly attacks the "constant waiting" — and it is a knob available today via BYOK.
- **Optimistic UI.** The user's action (submit an answer, ask) reflects in the document immediately; "thinking" happens in the document flow, never in a blocking modal.

**The model as a swappable knob, not a dependency.** Diffusion LLMs (e.g. the Mercury / Gemini Diffusion line) promise much better TTFT/throughput and are a plausible future lever once the stack is self-hosted — but the experience must not depend on them. The durable decision is a **model layer routed per sub-task and swappable** (which BYOK already forces); a faster model is a multiplier over an architecture that already hides latency, not the rescue of one that blocks.

**Synthesis with the atomic nodes (§6):** atomic nodes = more LLM rounds = risk of more spinner. The levers above (prefetch + tiering + streaming) are exactly what makes the atomic-node density affordable — the short-loop principle and responsiveness are resolved by the same architecture.

## 15. Endgame — hosting and portability

- The first incarnation is a **desktop application**, local-first (§3, §4). Meanwhile, the model stack is BYOK (§12) and there is no own infrastructure.
- When the desktop application has enough traction, the plan is to **host it and monetize with Monero donations** — in the spirit of how The Pirate Bay operated and LibGen operates. At that point the application is **100% portable**.
- Design consequences that already hold now: keep everything **local-first and portable** (files as the source of truth §4, key in the keychain §12, rebuildable indexes §10), and keep the **source-acquisition module swappable** (§11.1) so the business model is not welded to a specific source.

## 16. Open decisions / recorded risks

Known items, not yet resolved or to revisit — they do not block the start, but must not be forgotten.

- **Concurrency/resumability (baseline to revisit):** initial proposal — a single active study session per document; idempotent/resumable generation (a node carries generation state so an SSE stream interrupted by sleep/network resumes or regenerates deterministically, without corruption); multiple tabs = one authoritative session, the rest mirror read-only. To be defined with real usage.
- **LibGen legal/operational risk:** automatically downloading copyrighted works has different exposure from manual downloading, especially in the hosting endgame (§15). Structural mitigation: the swappable acquisition module (§11.1). The final legal stance is outside the scope of this spec.
- **Grounding in non-exact areas:** grounding exercises in the original material (§8) works well in exact sciences; in less deterministic domains it is looser and the assessment quality depends more on the §8.1 rubrics. A recognized, not solved, calibration risk.
- **Evaluator-failure detection:** the thesis depends on the assessment quality; there is no explicit telemetry/correction mechanism (a "this is wrong/useless" affordance that feeds back) to *detect* leniency, hallucinated grounding, or confrontation against a strawman. To be designed during Phase 1, when the loop exists to be observed.
- **Backup/sync across machines** (the polymath with a laptop + desktop): post-Phase-1; the file layout (§4) must preserve that possibility.
