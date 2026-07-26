//! Segurança do servidor local (§3.1).
//!
//! O servidor guarda as chaves de API do usuário e é alcançável por qualquer
//! aba do navegador, então precisa se defender de CSRF e DNS-rebinding:
//!
//! - Bind só em 127.0.0.1 (feito em `main`).
//! - Token de sessão obrigatório em toda requisição (estilo Jupyter): aceito no
//!   cabeçalho `X-Learnive-Token` ou na query `?token=` (para a navegação
//!   inicial pela barra de endereço). O token nunca vai em cookie — cookies são
//!   enviados cross-site e reabririam o vetor de CSRF.
//! - `Origin` validado contra uma allowlist quando presente (nunca `*`).
//! - `Host` validado contra uma allowlist (defesa de DNS-rebinding: o ataque
//!   resolve um domínio do atacante para 127.0.0.1, mas o `Host` continua sendo
//!   o domínio do atacante).
//! - Mutações nunca respondem a GET — isso é garantido pelo roteamento (rotas
//!   que mudam estado só existem como POST), não por esta camada.

use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use rand::{Rng, distributions::Alphanumeric};

use crate::app::AppState;

/// Gera um token de sessão aleatório (43 chars alfanuméricos, ~256 bits),
/// no mesmo espírito do token do Jupyter.
pub fn generate_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(43)
        .map(char::from)
        .collect()
}

/// Middleware aplicado a todas as rotas. Ordem: Host → Origin → token.
pub async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    // Host allowlist (DNS-rebinding).
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| state.allowed_hosts.contains(h))
        .unwrap_or(false);
    if !host_ok {
        return (StatusCode::FORBIDDEN, "host não permitido").into_response();
    }

    // Origin allowlist quando presente (CSRF / rebinding). Navegação de topo não
    // envia Origin — por isso só validamos quando o cabeçalho existe.
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let ok = origin
            .to_str()
            .ok()
            .map(|o| state.allowed_origins.contains(o))
            .unwrap_or(false);
        if !ok {
            return (StatusCode::FORBIDDEN, "origin não permitido").into_response();
        }
    }

    // Token de sessão (cabeçalho ou query).
    if !token_valid(&req, &state) {
        return (
            StatusCode::UNAUTHORIZED,
            "token de sessão inválido ou ausente",
        )
            .into_response();
    }

    next.run(req).await
}

/// Confere o token vindo do cabeçalho `X-Learnive-Token` ou da query `?token=`.
fn token_valid(req: &Request, state: &AppState) -> bool {
    if let Some(header_token) = req
        .headers()
        .get("x-learnive-token")
        .and_then(|v| v.to_str().ok())
        && constant_time_eq(header_token, &state.token)
    {
        return true;
    }

    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("token=") {
                // O token é alfanumérico, então não há percent-encoding a desfazer.
                if constant_time_eq(value, &state.token) {
                    return true;
                }
            }
        }
    }

    false
}

/// Comparação em tempo (quase) constante para não vazar o token por timing.
/// O comprimento é fixo e conhecido, então revelá-lo não é problema.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}
