use super::*;
use crate::ai::ChatMessage;

// ---------------------------------------------------------------------------
// Setup (§12): configure the provider + key (in-app), key in the secret store.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SetupReq {
    /// `openrouter` | `openai_compatible`.
    provider: String,
    /// `free` | `paid` — the single intent that derives both tiers (§12.1).
    intent: String,
    base_url: Option<String>,
    api_key: Option<String>,
    model_fast: Option<String>,
    model_robust: Option<String>,
}

#[derive(Serialize)]
pub struct SetupStatus {
    provider: String,
    intent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    /// Whether a key is stored — the key itself is NEVER returned (§12).
    has_key: bool,
    /// True when there is no working provider right now — only possible for
    /// OpenRouter with no key stored yet (`build_ai`'s `Provider::Unconfigured`
    /// path, §22). An `OpenAiCompatible` provider is always used for real
    /// once saved, keyed or not (keyless endpoints are a legitimate BYOK
    /// shape), so it's never this. Named `unconfigured`, not `demo`: the app
    /// never shows a real user demo content (§22), so nothing here should
    /// use that word — this just means "nothing to generate with yet".
    unconfigured: bool,
    /// The derived active model pair, for display only.
    model_fast: String,
    model_robust: String,
    /// True when there is no working provider yet — drives the settings
    /// window auto-opening straight to the Provider section on boot.
    /// Currently identical to `unconfigured` (the only "not working yet"
    /// case is the OpenRouter-without-a-key fallthrough), kept as a separate
    /// field since the two questions ("is this real" vs "should we nag")
    /// could diverge later without becoming the same computation again.
    needs_setup: bool,
}

fn status_of(config: &AppConfig, secret: &SecretStore) -> SetupStatus {
    let intent = match config.intent {
        Intent::Free => "free",
        Intent::Paid => "paid",
    }
    .to_string();

    // Bug found live 2026-09-01: this used to always report
    // `config.models()` — the settings-configured pairing — even when
    // `build_ai` (this module's sibling, §12) was actually using an env
    // override instead. CLAUDE.md is explicit that "the real environment
    // wins", but the Settings panel kept showing the shadowed
    // settings-derived pair regardless, so a `.env`-configured model (the
    // ordinary dev setup, and this project's own) never showed up here at
    // all — live-caught when a QA session's Settings panel read
    // "nemotron-3.5-lightning-free / hy3-free" while `.env` actually named
    // `gpt-oss-120b` for both tiers. Mirrors `build_ai`'s own precedence
    // (env override wins over settings) so this never lies about which
    // pair is actually in use.
    if let Ok(base_url) = std::env::var("LEARNIVE_API_BASE_URL")
        && !base_url.is_empty()
    {
        let has_key = std::env::var("LEARNIVE_API_KEY").is_ok_and(|k| !k.is_empty());
        let models = super::provider::models_from_env();
        return SetupStatus {
            provider: "openai_compatible".to_string(),
            intent,
            base_url: Some(base_url),
            has_key,
            unconfigured: false,
            model_fast: models.for_tier(Tier::Fast).to_string(),
            model_robust: models.for_tier(Tier::Robust).to_string(),
            needs_setup: false,
        };
    }
    if std::env::var("LEARNIVE_OPENROUTER_KEY").is_ok_and(|k| !k.is_empty()) {
        let models = super::provider::models_from_env();
        return SetupStatus {
            provider: "openrouter".to_string(),
            intent,
            base_url: None,
            has_key: true,
            unconfigured: false,
            model_fast: models.for_tier(Tier::Fast).to_string(),
            model_robust: models.for_tier(Tier::Robust).to_string(),
            needs_setup: false,
        };
    }

    let (provider, base_url) = match &config.provider {
        ProviderKind::OpenRouter => ("openrouter".to_string(), None),
        ProviderKind::OpenAiCompatible { base_url } => {
            ("openai_compatible".to_string(), Some(base_url.clone()))
        }
    };
    let has_key = secret.get(config.key_name()).is_some();
    // Only OpenRouter ever leaves `Provider::Unconfigured` in play (§22) when unkeyed.
    let unconfigured = matches!(config.provider, ProviderKind::OpenRouter) && !has_key;
    let models = config.models();
    SetupStatus {
        provider,
        intent,
        base_url,
        has_key,
        unconfigured,
        model_fast: models.for_tier(Tier::Fast).to_string(),
        model_robust: models.for_tier(Tier::Robust).to_string(),
        needs_setup: unconfigured,
    }
}

/// Current setup, for prefilling the form. Never leaks the key.
pub async fn setup_status(State(state): State<AppState>) -> Json<SetupStatus> {
    let config = state.config.read().await.clone();
    Json(status_of(&config, &state.secret))
}

/// Saves the provider/intent (config file) + key (secret store) and hot-swaps the
/// live AI (§12) — no restart. State-changing → POST only, token-guarded (§3.1).
pub async fn save_setup(
    State(state): State<AppState>,
    Json(req): Json<SetupReq>,
) -> Result<Json<SetupStatus>, ApiError> {
    let provider = match req.provider.as_str() {
        "openrouter" => ProviderKind::OpenRouter,
        "openai_compatible" => {
            let base = req
                .base_url
                .clone()
                .filter(|b| !b.trim().is_empty())
                .ok_or_else(|| ApiError::BadRequest("base_url required".into()))?;
            ProviderKind::OpenAiCompatible {
                base_url: base.trim().to_string(),
            }
        }
        other => return Err(ApiError::BadRequest(format!("unknown provider: {other}"))),
    };
    let intent = if req.intent == "paid" {
        Intent::Paid
    } else {
        Intent::Free
    };
    let config = AppConfig {
        provider,
        intent,
        model_fast: req.model_fast.filter(|s| !s.trim().is_empty()),
        model_robust: req.model_robust.filter(|s| !s.trim().is_empty()),
    };

    // Validate before writing anything to disk: a bad key/model/endpoint
    // would otherwise only surface later, mid-generation, far from where the
    // mistake was made. The key is optional here — a keyless endpoint (e.g. a
    // provider's free tier) is a legitimate BYOK shape; the round trip itself,
    // not a client-side requirement, is what decides if it actually works.
    let existing = state.config.read().await.clone();
    let key = key_for_validation(
        req.api_key.as_deref(),
        &config.provider,
        &existing,
        &state.secret,
    );
    let candidate_provider = match &config.provider {
        ProviderKind::OpenRouter => Provider::OpenAiCompat(OpenAiCompat::openrouter(key)),
        ProviderKind::OpenAiCompatible { base_url } => {
            Provider::OpenAiCompat(OpenAiCompat::new(base_url.clone(), key))
        }
    };
    let models = config.models();
    let candidate_ai = Ai::new(candidate_provider, models.clone());
    validate_provider(&candidate_ai, &models).await?;

    // Persist config (no secret) and store the key separately (§12).
    config
        .save(&*state.data_dir)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if let Some(key) = req.api_key.as_ref().filter(|k| !k.trim().is_empty()) {
        state
            .secret
            .set(config.key_name(), key.trim())
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    }

    // Apply live: swap config + rebuild the provider (§12 hot-swap).
    *state.config.write().await = config.clone();
    let ai = build_ai(&config, &state.secret);
    state.ai.store(std::sync::Arc::new(ai));

    let status = status_of(&config, &state.secret);
    Ok(Json(status))
}

/// Which key to validate the candidate provider with: the freshly typed key
/// wins; otherwise reuse the already-stored key, but ONLY when the candidate
/// provider is identical (kind + base URL) to what's already configured —
/// never probe one provider's endpoint with another provider's secret.
fn key_for_validation(
    typed: Option<&str>,
    candidate: &ProviderKind,
    existing: &AppConfig,
    secret: &SecretStore,
) -> Option<String> {
    if let Some(k) = typed.map(str::trim).filter(|k| !k.is_empty()) {
        return Some(k.to_string());
    }
    if &existing.provider == candidate {
        secret.get(existing.key_name())
    } else {
        None
    }
}

/// A minimal round trip against the candidate provider/key/model(s) — proof
/// the endpoint is reachable, the key is accepted, and the model name is
/// valid, before anything lands on disk. Content is ignored; only whether
/// the call errors matters here. Validates both tiers when they differ (a
/// manual override could break just one of them).
async fn validate_provider(ai: &Ai, models: &Models) -> Result<(), ApiError> {
    let ping = || {
        vec![
            ChatMessage::system(
                "Connectivity check for a newly configured AI provider. \
                 Reply with a single short word.",
            ),
            ChatMessage::user("ping"),
        ]
    };
    ai.complete(Tier::Fast, ping())
        .await
        .map_err(|e| ApiError::BadRequest(format!("Provider validation failed: {e}")))?;
    if models.for_tier(Tier::Fast) != models.for_tier(Tier::Robust) {
        ai.complete(Tier::Robust, ping()).await.map_err(|e| {
            ApiError::BadRequest(format!("Provider validation failed (robust model): {e}"))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_secret() -> (tempfile::TempDir, SecretStore) {
        let dir = tempfile::tempdir().unwrap();
        let secret = SecretStore::open(dir.path());
        (dir, secret)
    }

    #[test]
    fn key_for_validation_prefers_typed_key() {
        let (_dir, secret) = tmp_secret();
        let existing = AppConfig::default();
        let got = key_for_validation(
            Some("  fresh-key  "),
            &ProviderKind::OpenRouter,
            &existing,
            &secret,
        );
        assert_eq!(got, Some("fresh-key".to_string()));
    }

    #[test]
    fn key_for_validation_reuses_stored_key_for_same_provider() {
        let (_dir, secret) = tmp_secret();
        secret.set("openrouter", "stored-key").unwrap();
        let existing = AppConfig {
            provider: ProviderKind::OpenRouter,
            ..Default::default()
        };
        let got = key_for_validation(None, &ProviderKind::OpenRouter, &existing, &secret);
        assert_eq!(got, Some("stored-key".to_string()));
    }

    #[test]
    fn key_for_validation_refuses_to_leak_a_different_providers_key() {
        let (_dir, secret) = tmp_secret();
        secret.set("openrouter", "stored-key").unwrap();
        let existing = AppConfig {
            provider: ProviderKind::OpenRouter,
            ..Default::default()
        };
        let candidate = ProviderKind::OpenAiCompatible {
            base_url: "https://example.com/v1".into(),
        };
        let got = key_for_validation(None, &candidate, &existing, &secret);
        assert_eq!(got, None);
    }

    #[test]
    fn key_for_validation_none_when_nothing_typed_or_stored() {
        let (_dir, secret) = tmp_secret();
        let existing = AppConfig::default();
        let got = key_for_validation(None, &ProviderKind::OpenRouter, &existing, &secret);
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn validate_provider_passes_on_a_working_mock() {
        let ai = Ai::new(
            Provider::Mock(MockProvider::new("ok")),
            Models::new("fast", "robust"),
        );
        let models = Models::new("fast", "robust");
        assert!(validate_provider(&ai, &models).await.is_ok());
    }

    #[test]
    fn needs_setup_true_until_openrouter_is_keyed() {
        let (_dir, secret) = tmp_secret();
        let unkeyed = AppConfig::default(); // OpenRouter, no key stored yet.
        assert!(status_of(&unkeyed, &secret).needs_setup);
        assert!(status_of(&unkeyed, &secret).unconfigured);

        secret.set("openrouter", "k").unwrap();
        assert!(!status_of(&unkeyed, &secret).needs_setup);
    }

    #[test]
    fn needs_setup_false_for_a_saved_keyless_custom_endpoint() {
        let (_dir, secret) = tmp_secret();
        let custom = AppConfig {
            provider: ProviderKind::OpenAiCompatible {
                base_url: "https://example.com/v1".into(),
            },
            ..Default::default()
        };
        // A keyless OpenAI-compatible endpoint (e.g. a free tier) is a
        // legitimate, already-validated BYOK shape — it must never nag.
        assert!(!status_of(&custom, &secret).needs_setup);
        assert!(!status_of(&custom, &secret).unconfigured);
    }
}
