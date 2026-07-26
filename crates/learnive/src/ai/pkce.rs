//! OAuth PKCE para o OpenRouter — o caminho de provedor default (§12).
//!
//! Fluxo: geramos um `verifier` aleatório e seu `challenge` (S256), abrimos a
//! URL de autorização no navegador do usuário, e no callback trocamos o `code`
//! (mais o `verifier`) por uma chave de API que o próprio usuário controla.
//! Assim o default não pede copiar/colar chave.
//!
//! A geração/troca é pura e testável aqui; o round-trip pelo navegador e o
//! armazenamento da chave no keychain vêm com a tela de setup (§12).
#![allow(dead_code)]

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{Rng, distributions::Alphanumeric};
use sha2::{Digest, Sha256};

/// Par verifier/challenge PKCE.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Gera um novo par PKCE (verifier de 64 chars, challenge S256 em base64url).
pub fn generate() -> Pkce {
    let verifier: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();
    let challenge = challenge_for(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

/// Deriva o code challenge S256 de um verifier.
pub fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Monta a URL de autorização do OpenRouter. `callback_url` é um endpoint do
/// próprio servidor local que recebe o `?code=`.
pub fn authorize_url(challenge: &str, callback_url: &str) -> String {
    reqwest::Url::parse_with_params(
        "https://openrouter.ai/auth",
        &[
            ("callback_url", callback_url),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ],
    )
    .expect("URL base válida")
    .to_string()
}

/// Troca o `code` recebido no callback pela chave de API do usuário.
pub async fn exchange_code(
    http: &reqwest::Client,
    code: &str,
    verifier: &str,
) -> Result<String, super::provider::ProviderError> {
    use super::provider::ProviderError;

    #[derive(serde::Serialize)]
    struct Body<'a> {
        code: &'a str,
        code_verifier: &'a str,
        code_challenge_method: &'a str,
    }
    #[derive(serde::Deserialize)]
    struct KeyResp {
        key: String,
    }

    let resp = http
        .post("https://openrouter.ai/api/v1/auth/keys")
        .json(&Body {
            code,
            code_verifier: verifier,
            code_challenge_method: "S256",
        })
        .send()
        .await
        .map_err(|e| ProviderError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError::Api { status, body });
    }

    let parsed: KeyResp = resp
        .json()
        .await
        .map_err(|e| ProviderError::Http(e.to_string()))?;
    Ok(parsed.key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_s256_of_verifier() {
        let pkce = generate();
        assert_eq!(pkce.verifier.len(), 64);
        // O challenge deve ser reproduzível a partir do verifier.
        assert_eq!(challenge_for(&pkce.verifier), pkce.challenge);
        // base64url sem padding: sem '+', '/' ou '='.
        assert!(!pkce.challenge.contains(['+', '/', '=']));
    }

    #[test]
    fn authorize_url_has_params() {
        let url = authorize_url("chal123", "http://127.0.0.1:7420/oauth/callback");
        assert!(url.starts_with("https://openrouter.ai/auth?"));
        assert!(url.contains("code_challenge=chal123"));
        assert!(url.contains("code_challenge_method=S256"));
        // callback deve estar percent-encoded.
        assert!(url.contains("callback_url=http%3A%2F%2F127.0.0.1%3A7420"));
    }
}
