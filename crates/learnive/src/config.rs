//! User configuration (§12, §12.1) — persisted, human-readable, no secrets.
//!
//! The provider choice and the single free/paid *intent* live here (in
//! `<data>/config.json`); the API key lives in the [`SecretStore`](crate::secret)
//! (never in this file). From the intent the app derives BOTH model tiers via
//! shipped, maintained pairings (§12.1) — a non-technical user never types a model
//! name. Manual per-tier overrides are optional.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ai::Models;

/// Which provider to talk to (§12). BYOK — no subscription OAuth tokens.
/// There is no "demo" variant here on purpose: offline demo mode is a dev/test
/// escape hatch (`LEARNIVE_DEMO` env var, checked in `build_ai`), never a
/// choice persisted from the settings window — a real user config always
/// names a real provider, even before a key has been saved for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ProviderKind {
    /// OpenRouter (default BYOK path). Key secret name: `openrouter`.
    #[default]
    OpenRouter,
    /// Any OpenAI-compatible endpoint (`chat/completions`-shaped): a named
    /// preset (OpenAI, Groq, OpenCode Zen, …) or a fully custom one — both
    /// resolve to the same shape, just a different `base_url`. Key secret
    /// name: `api`.
    OpenAiCompatible { base_url: String },
}

/// The single high-level setup question (§12.1): free or paid. Derives both tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Intent {
    #[default]
    Free,
    Paid,
}

/// Persisted, non-secret configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub provider: ProviderKind,
    #[serde(default)]
    pub intent: Intent,
    /// Optional manual per-tier overrides (advanced, §12.1) — else derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_fast: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_robust: Option<String>,
}

impl AppConfig {
    const FILE: &'static str = "config.json";

    /// Loads config from the data dir, or a `Demo`/`Free` default if absent/bad.
    pub fn load(data_dir: impl AsRef<Path>) -> Self {
        fs::read(data_dir.as_ref().join(Self::FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Persists config (atomic write). Never contains a key.
    pub fn save(&self, data_dir: impl AsRef<Path>) -> io::Result<()> {
        let dir = data_dir.as_ref();
        fs::create_dir_all(dir)?;
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let path = dir.join(Self::FILE);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// The secret name under which this provider's key is stored. Every
    /// remaining `ProviderKind` always has one — a key may still be absent
    /// from the store (a keyless endpoint, or not saved yet), but there is
    /// always a name to look it up under.
    pub fn key_name(&self) -> &'static str {
        match self.provider {
            ProviderKind::OpenRouter => "openrouter",
            ProviderKind::OpenAiCompatible { .. } => "api",
        }
    }

    /// Resolves the fast/robust model pair (§12.1): manual overrides win, else the
    /// maintained recommended pairing for (provider, intent).
    pub fn models(&self) -> Models {
        let (fast, robust) = recommended_pair(&self.provider, self.intent);
        Models::new(
            self.model_fast.clone().unwrap_or_else(|| fast.to_string()),
            self.model_robust
                .clone()
                .unwrap_or_else(|| robust.to_string()),
        )
    }
}

/// Maintained recommended (fast, robust) model pairings per provider/intent
/// (§12.1). Curated app defaults — the user picks an intent, not model names.
/// Update these as providers change; they are not a stable contract.
pub fn recommended_pair(provider: &ProviderKind, intent: Intent) -> (&'static str, &'static str) {
    match (provider, intent) {
        // OpenRouter free tier: capable free models for both tiers.
        (ProviderKind::OpenRouter, Intent::Free) => (
            "nvidia/nemotron-nano-9b-v2:free",
            "nvidia/nemotron-3-ultra-550b-a55b:free",
        ),
        (ProviderKind::OpenRouter, Intent::Paid) => {
            ("openai/gpt-4o-mini", "anthropic/claude-sonnet-4")
        }
        // OpenAI-compatible endpoints vary wildly; sensible generic names the user
        // can override. A single-model endpoint just reuses one for both tiers.
        (ProviderKind::OpenAiCompatible { .. }, _) => ("gpt-4o-mini", "gpt-4o"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::Tier;

    #[test]
    fn load_default_when_absent() {
        let dir = std::env::temp_dir().join(format!("learnive-cfg-none-{}", std::process::id()));
        let cfg = AppConfig::load(&dir);
        assert_eq!(cfg.provider, ProviderKind::OpenRouter);
        assert_eq!(cfg.intent, Intent::Free);
    }

    #[test]
    fn save_load_roundtrip_and_key_name() {
        let dir = std::env::temp_dir().join(format!("learnive-cfg-{}", std::process::id()));
        let cfg = AppConfig {
            provider: ProviderKind::OpenRouter,
            intent: Intent::Paid,
            model_fast: None,
            model_robust: None,
        };
        cfg.save(&dir).unwrap();
        let back = AppConfig::load(&dir);
        assert_eq!(back.provider, ProviderKind::OpenRouter);
        assert_eq!(back.intent, Intent::Paid);
        assert_eq!(back.key_name(), "openrouter");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn intent_derives_both_tiers() {
        let free = AppConfig {
            provider: ProviderKind::OpenRouter,
            intent: Intent::Free,
            ..Default::default()
        };
        assert!(free.models().for_tier(Tier::Fast).ends_with(":free"));
        assert!(free.models().for_tier(Tier::Robust).ends_with(":free"));

        let paid = AppConfig {
            provider: ProviderKind::OpenRouter,
            intent: Intent::Paid,
            ..Default::default()
        };
        assert!(!paid.models().for_tier(Tier::Robust).ends_with(":free"));
    }

    #[test]
    fn manual_override_wins() {
        let cfg = AppConfig {
            provider: ProviderKind::OpenRouter,
            intent: Intent::Free,
            model_fast: Some("my/custom-model".into()),
            model_robust: None,
        };
        assert_eq!(cfg.models().for_tier(Tier::Fast), "my/custom-model");
    }
}
