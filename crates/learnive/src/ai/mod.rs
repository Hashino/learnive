//! AI layer: swappable provider (§12), OpenRouter OAuth PKCE, and model tiering
//! by intent (§12.1).
//!
//! Consumed by the loop (Phase 1, Task #5); hence the temporary `allow`s.
#![allow(dead_code, unused_imports)]

pub mod pkce;
pub mod provider;

pub use provider::{
    ChatMessage, ChatRequest, MockProvider, OpenAiCompat, Provider, ProviderError, Role,
    TokenStream,
};

/// Model tier (§12.1, §14). The system routes by *sub-task intent*, not by a
/// model name chosen by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Light/fast: exercises, grading against the rubric, summaries, cross-ref.
    Fast,
    /// Robust: explanatory prose and adversarial confrontation.
    Robust,
}

/// A fast/robust model pair. Derived from a single intent choice (free vs paid)
/// at setup — the user does not pick models by name (§12.1).
#[derive(Debug, Clone)]
pub struct Models {
    pub fast: String,
    pub robust: String,
}

impl Models {
    pub fn new(fast: impl Into<String>, robust: impl Into<String>) -> Self {
        Self {
            fast: fast.into(),
            robust: robust.into(),
        }
    }

    /// Graceful degradation (§12.1): a single model serves both tiers. Tiering is
    /// an optimization, never a barrier to start.
    pub fn single(model: impl Into<String>) -> Self {
        let model = model.into();
        Self {
            fast: model.clone(),
            robust: model,
        }
    }

    pub fn for_tier(&self, tier: Tier) -> &str {
        match tier {
            Tier::Fast => &self.fast,
            Tier::Robust => &self.robust,
        }
    }
}

/// Facade that joins provider + tiering: the rest of the app asks for generation
/// by *intent* (tier), without knowing model names.
pub struct Ai {
    provider: Provider,
    models: Models,
}

impl Ai {
    pub fn new(provider: Provider, models: Models) -> Self {
        Self { provider, models }
    }

    /// Streams a completion in the requested tier.
    pub async fn stream(
        &self,
        tier: Tier,
        messages: Vec<ChatMessage>,
    ) -> Result<TokenStream, ProviderError> {
        self.provider
            .stream(ChatRequest {
                model: self.models.for_tier(tier).to_string(),
                messages,
                temperature: None,
            })
            .await
    }

    /// Non-streamed completion in the requested tier — for callers that
    /// never render the response live (`engine::collect`, §14).
    pub async fn complete(
        &self,
        tier: Tier,
        messages: Vec<ChatMessage>,
    ) -> Result<String, ProviderError> {
        self.provider
            .complete(ChatRequest {
                model: self.models.for_tier(tier).to_string(),
                messages,
                temperature: None,
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_model_serves_both_tiers() {
        let models = Models::single("free-model");
        assert_eq!(models.for_tier(Tier::Fast), "free-model");
        assert_eq!(models.for_tier(Tier::Robust), "free-model");
    }

    #[test]
    fn distinct_tiers_route_distinct_models() {
        let models = Models::new("fast-1", "robust-1");
        assert_eq!(models.for_tier(Tier::Fast), "fast-1");
        assert_eq!(models.for_tier(Tier::Robust), "robust-1");
    }

    #[tokio::test]
    async fn ai_streams_via_provider() {
        use futures_util::StreamExt;
        let ai = Ai::new(
            Provider::Mock(MockProvider::new("generated answer")),
            Models::single("mock"),
        );
        let stream = ai
            .stream(Tier::Robust, vec![ChatMessage::user("hi")])
            .await
            .unwrap();
        let out: String = stream
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();
        assert_eq!(out, "generated answer");
    }

    #[tokio::test]
    async fn ai_completes_via_provider_without_streaming() {
        let ai = Ai::new(
            Provider::Mock(MockProvider::new("generated answer")),
            Models::single("mock"),
        );
        let out = ai
            .complete(Tier::Robust, vec![ChatMessage::user("hi")])
            .await
            .unwrap();
        assert_eq!(out, "generated answer");
    }
}
