//! OAuth PKCE for OpenRouter — the default provider path (§12).
//!
//! Flow: we generate a random `verifier` and its `challenge` (S256), open the
//! authorization URL in the user's browser, and at the callback exchange the
//! `code` (plus the `verifier`) for an API key that the user controls. This way
//! the default path does not require copy/pasting a key.
//!
//! Generation/exchange is pure and testable here; the browser round-trip and
//! storing the key in the keychain come with the setup screen (§12).
#![allow(dead_code)]

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{Rng, distributions::Alphanumeric};
use sha2::{Digest, Sha256};

/// PKCE verifier/challenge pair.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Generates a new PKCE pair (64-char verifier, S256 challenge in base64url).
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

/// Derives the S256 code challenge from a verifier.
pub fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Builds the OpenRouter authorization URL. `callback_url` is an endpoint on the
/// local server itself that receives the `?code=`.
pub fn authorize_url(challenge: &str, callback_url: &str) -> String {
    reqwest::Url::parse_with_params(
        "https://openrouter.ai/auth",
        &[
            ("callback_url", callback_url),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ],
    )
    .expect("valid base URL")
    .to_string()
}

/// Exchanges the `code` received at the callback for the user's API key.
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
        // The challenge must be reproducible from the verifier.
        assert_eq!(challenge_for(&pkce.verifier), pkce.challenge);
        // base64url without padding: no '+', '/' or '='.
        assert!(!pkce.challenge.contains(['+', '/', '=']));
    }

    #[test]
    fn authorize_url_has_params() {
        let url = authorize_url("chal123", "http://127.0.0.1:7420/oauth/callback");
        assert!(url.starts_with("https://openrouter.ai/auth?"));
        assert!(url.contains("code_challenge=chal123"));
        assert!(url.contains("code_challenge_method=S256"));
        // callback must be percent-encoded.
        assert!(url.contains("callback_url=http%3A%2F%2F127.0.0.1%3A7420"));
    }
}
