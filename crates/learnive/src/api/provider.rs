use super::*;

// ---------------------------------------------------------------------------
// Provider selection (§12) — any OpenAI-compatible endpoint is swappable. Order:
// a custom base URL (generic BYOK), else OpenRouter (default), else offline demo.
// ---------------------------------------------------------------------------

/// Builds the `Ai` from the environment (§12), together with the policy-ladder
/// rung (§14) that goes with it — the two must never be derived separately:
/// deriving the rung from `config` alone would desync from which `Ai` this
/// function actually returns whenever an env-var override is active (a real
/// provider while `config.provider` is still `Demo`). Precedence:
/// 1. `LEARNIVE_API_BASE_URL` (+ optional `LEARNIVE_API_KEY`) — any OpenAI-compatible
///    `chat/completions` endpoint: Inception's Mercury, OpenCode Zen, a local model.
/// 2. `LEARNIVE_OPENROUTER_KEY` — the default OpenRouter path.
/// 3. Otherwise, offline demo mode.
///
/// Rung: a real provider (any of the paths above) derives L1/L2 from the
/// free/paid intent (§12.1); the demo fallback is always L0 (demo mode = L0,
/// PLAN.md) — deterministic content, no AI call for `decide_move`.
pub fn build_ai(config: &AppConfig, secret: &SecretStore) -> (Ai, AgentPolicy) {
    let real_provider_policy = match config.intent {
        Intent::Free => AgentPolicy::L1,
        Intent::Paid => AgentPolicy::L2,
    };

    // 1. Environment override wins (dev / `.env`; CLAUDE.md: the real env wins).
    if let Ok(base_url) = std::env::var("LEARNIVE_API_BASE_URL")
        && !base_url.is_empty()
    {
        let key = std::env::var("LEARNIVE_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());
        return (
            Ai::new(
                Provider::OpenAiCompat(OpenAiCompat::new(base_url, key)),
                models_from_env(),
            ),
            real_provider_policy,
        );
    }
    if let Ok(key) = std::env::var("LEARNIVE_OPENROUTER_KEY")
        && !key.is_empty()
    {
        return (
            Ai::new(
                Provider::OpenAiCompat(OpenAiCompat::openrouter(Some(key))),
                models_from_env(),
            ),
            real_provider_policy,
        );
    }

    // 2. The provider configured in /setup, with its key from the secret store
    //    (§12). Models are derived from the free/paid intent (§12.1).
    match &config.provider {
        ProviderKind::OpenRouter => {
            if let Some(key) = secret.get("openrouter") {
                return (
                    Ai::new(
                        Provider::OpenAiCompat(OpenAiCompat::openrouter(Some(key))),
                        config.models(),
                    ),
                    real_provider_policy,
                );
            }
        }
        ProviderKind::OpenAiCompatible { base_url } => {
            return (
                Ai::new(
                    Provider::OpenAiCompat(OpenAiCompat::new(base_url.clone(), secret.get("api"))),
                    config.models(),
                ),
                real_provider_policy,
            );
        }
        ProviderKind::Demo => {}
    }

    // 3. Nothing configured → demo.
    eprintln!("No provider configured — DEMO MODE. Open /setup to configure a provider.");
    (demo_ai(), AgentPolicy::L0)
}

/// Reads the fast/robust model pair from the environment (§12.1). Defaults are
/// OpenRouter model ids; for other providers set both explicitly (e.g. `mercury-2`).
fn models_from_env() -> Models {
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
    if text.contains("choosing the next move") {
        // movement::decide_move (L1/L2) contract.
        return r#"{"move_type":"explain","rationale":"demo: start with an explanation"}"#
            .to_string();
    }
    if text.contains("Move JSON contract") {
        // movement::generate_move (structured path only — test/profile/plan/
        // other) contract. Branch by the move-type marker embedded in its
        // system prompt ("generating a \"test\" move").
        if text.contains("\"test\" move") {
            return r#"{"html":"<form><p>Apply the concept to a new case:</p><textarea name=\"answer\" rows=\"4\" required></textarea><p><button type=\"submit\">Submit answer</button></p></form>","interactive":false,"graded":true,"tactics":["worked-example"],"objectives":[{"id":"o1","kind":"application","description":"Apply the concept to a new case","criteria":"The answer transfers the concept to a scenario not covered in the text","transfer":true}]}"#.to_string();
        }
        if text.contains("\"plan\" move") {
            // No structural change proposed — demo mode never has enough
            // signal to justify one, so the loop just continues (§S4: "no
            // proposal, no approval needed").
            return r#"{"html":"<p>No structural changes needed yet in <strong>demo mode</strong>.</p>","interactive":false,"graded":false,"tactics":[],"outline":[]}"#.to_string();
        }
        return r#"{"html":"<h2>Core concept</h2><p>This is a structured move generated in <strong>demo mode</strong> via the move ABI.</p>","interactive":false,"graded":false,"tactics":["analogy"],"objectives":[]}"#.to_string();
    }
    if text.contains("distill a learner's evidence profile") {
        // profile::prompt::distill (§7/§S7) contract — checked before the
        // generic branches below since it's also a JSON-envelope call.
        return r#"{"traits":["demo mode: not enough graded evidence yet for a real trait"],"hypotheses":["would a more concrete worked example change the outcome?"]}"#.to_string();
    }
    if text.contains("Decide how to answer it: INLINE") {
        // engine::prompt::decide_ask_response (§7/§S8) contract — demo mode
        // never has real signal to justify spawning a new section, so it
        // always answers inline, same behavior as before this slice.
        return r#"{"spawn":false,"title":""}"#.to_string();
    }
    if text.contains("<!--tactics:") {
        // movement::generate_move_stream (streamed path — explain/ask/
        // confront/integrate/revisit) prompt: plain HTML, no JSON envelope,
        // with a trailing tactics sentinel per the contract.
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
