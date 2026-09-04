use super::*;

// ---------------------------------------------------------------------------
// Provider selection (§12) — any OpenAI-compatible endpoint is swappable. Order:
// a custom base URL (generic BYOK), else OpenRouter (default), else settings,
// else `Provider::Unconfigured` (never offline demo — that's dev-only, §22).
// ---------------------------------------------------------------------------

/// Builds the `Ai` from the environment (§12). Precedence:
/// 1. `LEARNIVE_DEMO` (any non-empty value) — dev/test escape hatch, forces
///    offline demo mode regardless of what's configured. Never a UI choice,
///    never reachable from anything a real user does in the app itself.
/// 2. `LEARNIVE_API_BASE_URL` (+ optional `LEARNIVE_API_KEY`) — any OpenAI-compatible
///    `chat/completions` endpoint: a paid provider, OpenCode Zen, a local model.
/// 3. `LEARNIVE_OPENROUTER_KEY` — the default OpenRouter path.
/// 4. The provider configured in the settings window.
/// 5. Nothing configured at all → `Provider::Unconfigured` (§22, user
///    decision 2026-08-21): every call fails with `ProviderError::Unconfigured`
///    instead of silently generating demo content for a real user. The
///    settings window already auto-opens on boot for this case
///    (`SetupStatus::needs_setup`, api/setup.rs).
///
/// S33: this used to also derive the policy-ladder rung from the free/paid
/// intent (§12.1); the ladder is deleted — move choice is deterministic, so
/// the intent question now only feeds model tiering (`config.models`).
pub fn build_ai(config: &AppConfig, secret: &SecretStore) -> Ai {
    // 0. Dev/test override: force demo mode regardless of any other config.
    if std::env::var("LEARNIVE_DEMO").is_ok_and(|v| !v.is_empty()) {
        return demo_ai();
    }

    // 1. Environment override wins (dev / `.env`; CLAUDE.md: the real env wins).
    if let Ok(base_url) = std::env::var("LEARNIVE_API_BASE_URL")
        && !base_url.is_empty()
    {
        let key = std::env::var("LEARNIVE_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());
        return Ai::new(
            Provider::OpenAiCompat(OpenAiCompat::new(base_url, key)),
            models_from_env(),
        );
    }
    if let Ok(key) = std::env::var("LEARNIVE_OPENROUTER_KEY")
        && !key.is_empty()
    {
        return Ai::new(
            Provider::OpenAiCompat(OpenAiCompat::openrouter(Some(key))),
            models_from_env(),
        );
    }

    // 4. The provider configured in the settings window, with its key from
    //    the secret store (§12). Models are derived from the free/paid
    //    intent (§12.1).
    match &config.provider {
        ProviderKind::OpenRouter => {
            if let Some(key) = secret.get("openrouter") {
                return Ai::new(
                    Provider::OpenAiCompat(OpenAiCompat::openrouter(Some(key))),
                    config.models(),
                );
            }
        }
        ProviderKind::OpenAiCompatible { base_url } => {
            return Ai::new(
                Provider::OpenAiCompat(OpenAiCompat::new(base_url.clone(), secret.get("api"))),
                config.models(),
            );
        }
    }

    // Nothing configured, and LEARNIVE_DEMO not set → fail clearly instead
    // of silently serving demo content to a real user (§22, user decision
    // 2026-08-21: "o modo demo não deve nunca ser visível ao usuário").
    // `needs_setup` (api/setup.rs) already opens the settings window on
    // boot for this exact case; this just stops anything from generating
    // behind that gate.
    eprintln!(
        "No provider configured. Open the app and use the settings (⚙) \
         button to configure one."
    );
    Ai::new(Provider::Unconfigured, Models::single("unconfigured"))
}

/// Reads the fast/robust model pair from the environment (§12.1). Defaults are
/// OpenRouter model ids; for other providers set both explicitly (e.g. `mercury-2`).
/// `pub(super)`: `api::setup::status_of` also needs this, to report the
/// pair that's ACTUALLY active when an env override shadows the
/// settings-configured provider (bug found live 2026-09-01 — see its call
/// site for the full story).
pub(super) fn models_from_env() -> Models {
    let fast = std::env::var("LEARNIVE_MODEL_FAST").unwrap_or_else(|_| "openai/gpt-4o-mini".into());
    let robust = std::env::var("LEARNIVE_MODEL_ROBUST").unwrap_or_else(|_| "openai/gpt-4o".into());
    Models::new(fast, robust)
}

/// Demo-mode `Ai`: a mock that answers differently per sub-task, closing the
/// whole loop offline.
pub fn demo_ai() -> Ai {
    Ai::new(
        Provider::Mock(MockProvider::scripted(demo_responder)),
        Models::single("demo"),
    )
}

pub(crate) fn demo_responder(req: &crate::ai::ChatRequest) -> String {
    let text = req
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // Branch on distinctive phrases from each prompt (engine::prompt /
    // movement::prompt). Keep these in sync with the prompt wording. The
    // movement.rs (S2) checks come FIRST: its structured generate_move
    // prompt embeds EXERCISE_HTML_CONTRACT for `test`, whose text literally
    // contains "exercise_html" and would otherwise be caught by the legacy
    // branch below with the wrong JSON shape.
    if text.contains("cold start of a living curriculum") {
        // engine::prompt::propose_objective (§6.1/§S4) contract.
        return r#"{"text":"Learn the essentials of the requested topic, well enough to explain and apply it","title":"Demo document"}"#.to_string();
    }
    if text.contains("propose the initial READING LIST") {
        // engine::prompt::propose_outline (S27e) contract — the schema is
        // exactly `source::ProposedItem`'s shape, one flat array, no
        // `children` (see `parse::outline_tree`'s doc comment): a small,
        // deterministic two-item list — one foundational work, then the
        // work most directly covering the objective — so demo mode
        // exercises the toggle-confirmation screen with a real,
        // multi-item, sequentially-gated outline. The direct-API fallback
        // (`create_document`'s empty-`nodes` branch, S27e) now
        // auto-confirms the WHOLE list, in order, not just the last
        // element (there's no more separate "unreviewed prerequisite"
        // category to drop — PLAN.md §27 decision 3), so both items land
        // in the materialized outline either way.
        // Titles/authors come from `source::mock::{DEMO_BOOK_1, DEMO_BOOK_2}` —
        // S27i (PLAN.md, 2026-08-30) made them the one shared source of truth
        // so this scripted list and the PDF fixtures `app::AppState::new`
        // eagerly seeds into `<data>/library/` under `LEARNIVE_DEMO` can never
        // drift apart into naming two different "books".
        let (t1, a1) = crate::source::mock::DEMO_BOOK_1;
        let (t2, a2) = crate::source::mock::DEMO_BOOK_2;
        return format!(
            r#"[{{"title":"{t1}","authors":["{a1}"],"year":2020,"edition":null,"identifier":null,"kind":"book"}},{{"title":"{t2}","authors":["{a2}"],"year":2024,"edition":null,"identifier":null,"kind":"book"}}]"#
        );
    }
    if text.contains("Move JSON contract") {
        // movement::generate_move (structured path only — test/profile/plan/
        // other) contract. Branch by the move-type marker embedded in its
        // system prompt ("generating a \"test\" move").
        if text.contains("\"test\" move") {
            return r#"{"html":"<form><p>Apply the concept to a new case:</p><textarea name=\"answer\" rows=\"4\" required></textarea><p><button type=\"submit\">Submit answer</button></p></form>","interactive":false,"graded":true,"tactics":["worked-example"],"objectives":[{"id":"o1","kind":"application","description":"Apply the concept to a new case","criteria":"The answer transfers the concept to a scenario not covered in the text","transfer":true}]}"#.to_string();
        }
        return r#"{"html":"<h2>Core concept</h2><p>This is a structured move generated in <strong>demo mode</strong> via the move ABI.</p>","interactive":false,"graded":false,"tactics":["analogy"],"objectives":[]}"#.to_string();
    }
    if text.contains("Decide how to answer it: INLINE") {
        // engine::prompt::decide_ask_response (§7/§S8) contract — demo mode
        // never has real signal to justify spawning a new section, so it
        // always answers inline, same behavior as before this slice.
        return r#"{"spawn":false,"title":""}"#.to_string();
    }
    if text.contains("<!--tactics:") {
        // movement::generate_move_stream (streamed path — explain/
        // integrate/revisit/respond) prompt: plain HTML, no JSON envelope,
        // with a trailing tactics sentinel per the contract. The sentinel
        // itself is legacy (S33 dropped the tactics instructions from the
        // prompts) but a free model might still emit one, and this branch
        // keeps demo mode exercising `strip_tactics_sentinel` either way.
        return "<h2>Core concept</h2><p>This is explanatory prose generated in \
                <strong>demo mode</strong> via the move ABI.</p>\n\
                <!--tactics: analogy-->"
            .to_string();
    }
    if text.contains("JSON array of strings") {
        r#"["Introduction to the topic", "Core concept", "Practical application"]"#.to_string()
    } else if text.contains("exercise_html") {
        r#"{"exercise_html":"<form><p>Explain the concept in your own words and apply it to a new case:</p><textarea name=\"answer\" rows=\"4\" required></textarea><p><button type=\"submit\">Submit answer</button></p></form>","objectives":[{"id":"o1","kind":"application","description":"Apply the concept to a new case","criteria":"The answer transfers the concept to a scenario not covered in the text","transfer":true}]}"#.to_string()
    } else if text.contains("locked rubric") {
        // Demo: a blank/empty answer fails (so the "fail on purpose" flow reaches
        // remediation §8.2 keyless); any real content is graded as demonstrated so
        // the loop still advances end to end.
        let blank = text.contains("\"answer\":\"\"") || text.contains("Student's answer: {}");
        if blank {
            r#"{"grades":[{"objective_id":"o1","grade":"not_demonstrated","feedback":"No answer given — nothing to assess."}]}"#.to_string()
        } else {
            r#"{"grades":[{"objective_id":"o1","grade":"demonstrated","feedback":"Good transfer of the concept to a new case."}]}"#.to_string()
        }
    } else if text.contains("Remediation session") {
        // Explanation only — a worked solution of the missed problem. The NEW
        // practice problem is generated separately (matches the "exercise_html"
        // branch above) and rendered in its own sandbox.
        "<p><strong>Worked solution.</strong> Let's redo the problem you missed, step by step: first identify what's given, then apply the core idea, then check the result. The slip was in the middle step — the core idea applies before the final comparison, not after.</p>".to_string()
    } else {
        // Prose (default).
        "<h2>Core concept</h2><p>This is an explanatory paragraph generated in <strong>demo mode</strong> (no AI key). Configure a provider for real content, grounded in sources.</p><p>Each node is atomic and ends in a comprehension check.</p>".to_string()
    }
}
