use super::*;

// ---------------------------------------------------------------------------
// Setup (§12): configure the provider + key (in-app), key in the secret store.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SetupReq {
    /// `demo` | `openrouter` | `openai_compatible`.
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
    demo: bool,
    /// The derived active model pair, for display only.
    model_fast: String,
    model_robust: String,
}

fn status_of(config: &AppConfig, secret: &SecretStore) -> SetupStatus {
    let (provider, base_url) = match &config.provider {
        ProviderKind::Demo => ("demo".to_string(), None),
        ProviderKind::OpenRouter => ("openrouter".to_string(), None),
        ProviderKind::OpenAiCompatible { base_url } => {
            ("openai_compatible".to_string(), Some(base_url.clone()))
        }
    };
    let has_key = config
        .key_name()
        .map(|n| secret.get(n).is_some())
        .unwrap_or(false);
    let models = config.models();
    SetupStatus {
        provider,
        intent: match config.intent {
            Intent::Free => "free",
            Intent::Paid => "paid",
        }
        .to_string(),
        base_url,
        has_key,
        demo: matches!(config.provider, ProviderKind::Demo),
        model_fast: models.for_tier(Tier::Fast).to_string(),
        model_robust: models.for_tier(Tier::Robust).to_string(),
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
        "demo" => ProviderKind::Demo,
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

    // Persist config (no secret) and store the key separately (§12).
    config
        .save(&*state.data_dir)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if let (Some(name), Some(key)) = (
        config.key_name(),
        req.api_key.as_ref().filter(|k| !k.trim().is_empty()),
    ) {
        state
            .secret
            .set(name, key.trim())
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    }

    // Apply live: swap config + rebuild the provider + rung (§12 hot-swap).
    // Both come from the same `build_ai` call — see its doc comment on why
    // they must never be derived separately.
    *state.config.write().await = config.clone();
    let (ai, policy) = build_ai(&config, &state.secret);
    state.ai.store(std::sync::Arc::new(ai));
    state.policy.store(std::sync::Arc::new(policy));

    let status = status_of(&config, &state.secret);
    Ok(Json(status))
}
