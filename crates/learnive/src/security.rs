//! Local-server security (§3.1).
//!
//! The server holds the user's API keys and is reachable by any browser tab, so
//! it must defend against CSRF and DNS-rebinding:
//!
//! - Bind only to 127.0.0.1 (done in `main`).
//! - Mandatory session token on every request (Jupyter style): accepted in the
//!   `X-Learnive-Token` header or the `?token=` query (for the initial
//!   navigation from the address bar). The token never goes in a cookie —
//!   cookies are sent cross-site and would reopen the CSRF vector.
//! - `Origin` validated against an allowlist when present (never `*`).
//! - `Host` validated against an allowlist (DNS-rebinding defense: the attack
//!   resolves an attacker domain to 127.0.0.1, but the `Host` stays the
//!   attacker's domain).
//! - Mutations never respond to GET — that is guaranteed by routing (routes that
//!   change state exist only as POST), not by this layer.

use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use rand::{Rng, distributions::Alphanumeric};

use crate::app::AppState;

/// Generates a random session token (43 alphanumeric chars, ~256 bits), in the
/// same spirit as Jupyter's token.
pub fn generate_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(43)
        .map(char::from)
        .collect()
}

/// Middleware applied to every route. Order: Host → Origin → token.
pub async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    // Host allowlist (DNS-rebinding).
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| state.allowed_hosts.contains(h))
        .unwrap_or(false);
    if !host_ok {
        return (StatusCode::FORBIDDEN, "host not allowed").into_response();
    }

    // Origin allowlist when present (CSRF / rebinding). Top-level navigation does
    // not send Origin — that is why we only validate when the header exists.
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let ok = origin
            .to_str()
            .ok()
            .map(|o| state.allowed_origins.contains(o))
            .unwrap_or(false);
        if !ok {
            return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
        }
    }

    // Session token (header or query).
    if !token_valid(&req, &state) {
        return (StatusCode::UNAUTHORIZED, "invalid or missing session token").into_response();
    }

    // Restrictive CSP (§3.1) as defense in depth beyond the client sanitizer:
    // `connect-src 'self'` prevents exfiltrating the token to another origin;
    // `object-src`/`base-uri 'none'` close classic vectors. Interactive/exercise
    // blocks run in their own sandboxed frame served by a dedicated route (§4.4)
    // that sets its own, deliberately more permissive CSP — this default is
    // only applied when a handler hasn't already set one, so that route's
    // policy is never clobbered on the way out.
    // `'unsafe-inline'` is still needed because the page's script and styles are
    // inline — externalizing them to drop it is a later hardening.
    let mut response = next.run(req).await;
    if !response
        .headers()
        .contains_key(header::CONTENT_SECURITY_POLICY)
    {
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        );
    }
    response
}

/// Content-security policy applied to every response.
const CSP: &str = "default-src 'self'; base-uri 'none'; object-src 'none'; \
     img-src 'self' data: https:; style-src 'self' 'unsafe-inline'; \
     script-src 'self' 'unsafe-inline'; connect-src 'self'; frame-src 'self'";

/// Checks the token from the `X-Learnive-Token` header or the `?token=` query.
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
                // The token is alphanumeric, so there is no percent-encoding to undo.
                if constant_time_eq(value, &state.token) {
                    return true;
                }
            }
        }
    }

    false
}

/// (Near-)constant-time comparison to avoid leaking the token by timing. The
/// length is fixed and known, so revealing it is not a problem.
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
