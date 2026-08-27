# learnive

**Generative learning, grounded in real sources.** learnive turns any topic, idea, or problem you want to explore into an adaptive curriculum, built progressively as a **living document** — a book that grows and adjusts as your real understanding is assessed, instead of handing you fixed material up front. It's for personal use, not a hosted product.

A living document is a sequence of **chapter/section-sized nodes**, each generated from your own PDF library — every claim is a rewrite of real source material, cited, never invented by the model. Each node ends in a comprehension check graded against a rubric **locked at generation time**, and the result decides what comes next (advance, or open a remediation conversation). It runs as a **local Rust HTTP server** rendered in your own browser; the backend does all file I/O and holds your API keys, the browser only talks to it over `127.0.0.1`.

> `SPEC.md` is the authoritative specification (§1–§16). `PLAN.md` is the living build plan — work lands when a real need shows up, not on a phase schedule. `CLAUDE.md` guides AI coding agents working in this repo.

## Status

The core loop works: cold start → outline → streamed node generation → locked-rubric grading → remediation/advance, usable in the browser. A 2026-08-23/26 architecture pivot is in progress (see `PLAN.md`'s S27/S28): node granularity moved from atomic concept to book chapter/section, sources moved to PDF-only with the browser's native viewer, and source acquisition became two-tier: an automatic backend when one is configured (none is, today — the slot is deliberately empty), falling back to a local file library you supply for anything it doesn't resolve. Parts of this are still mid-migration; `PLAN.md` tracks exactly what's built vs. planned.

## Requirements

- A recent Rust toolchain (**edition 2024**, Rust 1.85+).
- A monospace font for the intended look (Ubuntu Mono / JetBrains Mono / Fira Code); it falls back to the system monospace.

## Quick start

```sh
cargo run
```

It opens your default browser at the token-authenticated URL automatically. The token is required on every request (§3.1); if the browser doesn't open, use the URL printed to the console:

```
http://127.0.0.1:7420/?token=<generated-token>
```

Set `LEARNIVE_NO_OPEN=1` to skip auto-opening (useful for headless/dev runs).

Type a topic into "What are we learning?" and the loop begins — but you need an AI provider configured first (below); without one, generation fails clearly and points you at setup rather than producing fake content.

## Using a real AI provider

learnive is bring-your-own-AI (§12). The default path is **OpenRouter**. Copy the example env file and fill in your key:

```sh
cp .env.example .env
# edit .env and set LEARNIVE_OPENROUTER_KEY=sk-or-v1-...
cargo run
```

`.env` is gitignored — your key never gets committed. The server loads it at startup; the real environment always wins over the file.

### Environment variables

| Variable | Default | Purpose |
| --- | --- | --- |
| `LEARNIVE_API_BASE_URL` | *(unset)* | Any OpenAI-compatible endpoint (before `/chat/completions`), e.g. Mercury/Inception, OpenCode Zen, a local model. Takes precedence over OpenRouter (§12). |
| `LEARNIVE_API_KEY` | *(unset)* | API key for `LEARNIVE_API_BASE_URL`. |
| `LEARNIVE_OPENROUTER_KEY` | *(unset)* | OpenRouter API key, the default path (§12). |
| `LEARNIVE_MODEL_FAST` | `openai/gpt-4o-mini` | Light/fast tier: exercises, grading, summaries (§12.1). |
| `LEARNIVE_MODEL_ROBUST` | `openai/gpt-4o` | Robust tier: explanatory prose, confrontation (§12.1). |
| `LEARNIVE_PORT` | `7420` | Port to bind on `127.0.0.1`. |
| `LEARNIVE_DATA_DIR` | `learnive-data` | Where living documents are stored (§4). |
| `LEARNIVE_NO_OPEN` | *(unset → auto-open)* | Set to any value to skip opening the browser on startup. |
| `LEARNIVE_DEMO` | *(unset)* | **Development tool only, never a product path** — routes to a keyless offline mock provider. Never set this to try learnive for real; use a real provider above. |

With no provider configured, generation fails with a clear error and points at setup — it never silently falls back to canned content (§12).

Tiering is an optimization, never a barrier: a single model can serve both tiers.

## Architecture at a glance

- **`crates/core` (`learnive-core`)** — the §4.3 node data contract, HTML-dialect parse/serialize, and anchoring. Deliberately free of tokio/axum so it *could* compile to wasm (the wasm build itself was dropped, §3 — the client never resolves anchors, so there's no consumer); the constraint stays as cheap hygiene.
- **`crates/learnive`** — the axum binary: `security` (§3.1 local-server hardening), `store` (files as the source of truth, §4), `ai` (swappable provider + tiering + OpenRouter PKCE, §12), `engine` (outline/grading/remediation), `movement` (the move ABI inside a node — §6.3), `events` (append-only event log + aggregation, §7.1), `objective`/`profile` (versioned objective, evidence-based profile, §5/§7), `retrieval`/`source` (embeddings index + source acquisition, §10/§11), `api` (the loop endpoints, streamed over POST), `app` (state + router), and the embedded `assets/` UI.

Storage is **human-readable files** (Obsidian-style), one HTML file per node — a node is a book chapter/section or article, not an atomic concept (§6) — no binary database. Sources are PDFs from a local library you supply (§11.1); the original PDF stays canonical, extracted text is index-only. Generated interactive exercises run in a sandboxed `<iframe>` and return their answer via a narrow `postMessage` channel; non-interactive prose is sanitized before entering the app origin (§3.1/§4.4).

## Development

```sh
cargo build                 # build
cargo run                   # run the server
cargo test --workspace      # all tests (plain `cargo test` skips learnive-core)
cargo test <name>           # a single test
cargo clippy --all-targets  # lint
cargo fmt                   # format
```

## License

Not yet specified.
