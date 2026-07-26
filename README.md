# learnive

**Generative learning.** learnive turns any topic, idea, or problem you want to explore into an adaptive curriculum, built progressively as a **living document** — a book that grows and adjusts as your real understanding is assessed, instead of handing you fixed material up front.

Each concept is generated on demand, ends in a comprehension check graded against a rubric **locked at generation time**, and the result decides what comes next (advance, or open a remediation conversation). It runs as a **local Rust HTTP server** rendered in your own browser; the backend does all file I/O and holds your API keys, the browser only talks to it over `127.0.0.1`.

> `SPEC.md` is the authoritative specification (§1–§16). `PLAN.md` is the phased build plan. `CLAUDE.md` guides AI coding agents working in this repo.

## Status

Phase 1 — the minimum end-to-end loop works: cold start → outline → streamed node generation → locked-rubric grading → remediation/advance, usable in the browser. It runs **keyless in demo mode** (a prompt-aware offline mock) so you can try the loop with no account. See `PLAN.md` for what is in/out of the current phase.

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

Type a topic into "What are we learning?" and the loop begins. With no API key configured, you get **demo mode** (canned content) so the loop still closes end to end.

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
| `LEARNIVE_OPENROUTER_KEY` | *(unset → demo mode)* | OpenRouter API key, the default path (§12). |
| `LEARNIVE_MODEL_FAST` | `openai/gpt-4o-mini` | Light/fast tier: exercises, grading, summaries (§12.1). |
| `LEARNIVE_MODEL_ROBUST` | `openai/gpt-4o` | Robust tier: explanatory prose, confrontation (§12.1). |
| `LEARNIVE_PORT` | `7420` | Port to bind on `127.0.0.1`. |
| `LEARNIVE_DATA_DIR` | `learnive-data` | Where living documents are stored (§4). |
| `LEARNIVE_NO_OPEN` | *(unset → auto-open)* | Set to any value to skip opening the browser on startup. |

Tiering is an optimization, never a barrier: a single model can serve both tiers, and demo mode needs no model at all.

## Architecture at a glance

- **`crates/core` (`learnive-core`)** — the §4.3 node data contract, HTML-dialect parse/serialize, and anchoring. Deliberately free of tokio/axum so it also compiles to wasm and is shared with the client.
- **`crates/learnive`** — the axum binary: `security` (§3.1 local-server hardening), `store` (files as the source of truth, §4), `ai` (swappable provider + tiering + OpenRouter PKCE, §12), `engine` (curriculum loop + locked-rubric assessment, §6/§8), `api` (the loop endpoints, streamed over POST), `app` (state + router), and the embedded `assets/index.html` UI.

Storage is **human-readable files** (Obsidian-style), one HTML file per concept node, no binary database. Generated interactive exercises run in a sandboxed `<iframe>` and return their answer via a narrow `postMessage` channel; non-interactive prose is sanitized before entering the app origin (§3.1/§4.4).

## Development

```sh
cargo build                 # build
cargo run                   # run the server
cargo test                  # all tests
cargo test <name>           # a single test
cargo clippy --all-targets  # lint
cargo fmt                   # format
```

## License

Not yet specified.
