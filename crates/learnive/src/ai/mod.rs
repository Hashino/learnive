//! Camada de IA: provedor trocável (§12), OAuth PKCE do OpenRouter e tiering de
//! modelo por intenção (§12.1).
//!
//! Consumido pelo loop (Fase 1, Task #5); daí os `allow` temporários.
#![allow(dead_code, unused_imports)]

pub mod pkce;
pub mod provider;

pub use provider::{
    ChatMessage, ChatRequest, MockProvider, OpenAiCompat, Provider, ProviderError, Role,
    TokenStream,
};

/// Tier de modelo (§12.1, §14). O sistema roteia por *intenção da sub-tarefa*,
/// não por nome de modelo escolhido pelo usuário.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Leve/rápido: exercício, correção contra rubric, resumos, cross-ref.
    Fast,
    /// Robusto: prosa explicativa e confrontação adversarial.
    Robust,
}

/// Par de modelos fast/robusto. Derivado de uma única escolha de intenção
/// (gratuito vs pago) no setup — o usuário não escolhe modelos por nome (§12.1).
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

    /// Degradação graciosa (§12.1): um só modelo serve os dois tiers. Tiering é
    /// otimização, nunca barreira para começar.
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

/// Fachada que junta provedor + tiering: o resto do app pede geração por
/// *intenção* (tier), sem saber de nomes de modelo.
pub struct Ai {
    provider: Provider,
    models: Models,
}

impl Ai {
    pub fn new(provider: Provider, models: Models) -> Self {
        Self { provider, models }
    }

    /// Streama uma completion no tier pedido.
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
            Provider::Mock(MockProvider::new("resposta gerada")),
            Models::single("mock"),
        );
        let stream = ai
            .stream(Tier::Robust, vec![ChatMessage::user("oi")])
            .await
            .unwrap();
        let out: String = stream
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat();
        assert_eq!(out, "resposta gerada");
    }
}
