//! HTTP transport authentication: a shared bearer token plus `Origin`
//! validation, applied as an axum middleware layer over the `/mcp` and
//! `/upload` routes.
//!
//! Auth is optional and disabled by default; the middleware is always attached
//! to the protected routes so that `Origin` validation (DNS-rebinding defence)
//! runs even when the bearer check is off. Health and readiness routes are left
//! unauthenticated for liveness probes.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rand::RngCore;
use secrecy::{ExposeSecret, SecretString};
use subtle::ConstantTimeEq;

use crate::mcp::AppState;

/// Resolved authentication state held by [`AppState`]. Cheap to clone.
#[derive(Clone, Default)]
pub struct AuthState {
    /// When true, a valid bearer token is required on protected routes.
    pub enabled: bool,
    /// The expected bearer token, resolved at startup. `None` when auth is off.
    pub token: Option<SecretString>,
    /// Browser `Origin` values permitted to reach the protected routes.
    pub allowed_origins: Arc<Vec<String>>,
}

impl AuthState {
    /// Build a disabled auth state with no token and no origin allow-list.
    pub fn disabled() -> Self {
        Self::default()
    }
}

/// Generate a fresh 256-bit bearer token, rendered as 64 lowercase hex chars.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Axum middleware enforcing `Origin` validation (always) and bearer auth (when
/// enabled). Returns `403` for a disallowed origin and `401` (with a
/// `WWW-Authenticate: Bearer` challenge) for a missing or invalid token.
pub async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(rejection) = authorize(request.headers(), state.auth.as_ref()) {
        return rejection;
    }
    next.run(request).await
}

/// Pure authorization decision over request headers. Returns `Some(response)`
/// with the rejection to send, or `None` when the request is allowed.
///
/// `Origin` is validated regardless of whether the bearer check is enabled
/// (DNS-rebinding defence): a browser always sends `Origin`, while non-browser
/// MCP clients (Claude Code, curl) omit it and pass through.
fn authorize(headers: &HeaderMap, auth: &AuthState) -> Option<Response> {
    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or_default();
        let allowed = auth
            .allowed_origins
            .iter()
            .any(|candidate| candidate == origin);
        if !allowed {
            return Some((StatusCode::FORBIDDEN, "origin not allowed").into_response());
        }
    }

    if auth.enabled && !bearer_matches(headers, auth.token.as_ref()) {
        let mut response =
            (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return Some(response);
    }

    None
}

/// Constant-time comparison of the presented bearer token against the expected
/// value. Returns false if either is absent or they differ.
fn bearer_matches(headers: &HeaderMap, expected: Option<&SecretString>) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    let Some(presented) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(extract_bearer)
    else {
        return false;
    };
    presented
        .as_bytes()
        .ct_eq(expected.expose_secret().as_bytes())
        .into()
}

/// Extract the token from an `Authorization: Bearer <token>` header value,
/// case-insensitively on the scheme.
fn extract_bearer(header_value: &str) -> Option<&str> {
    let header_value = header_value.trim();
    let (scheme, token) = header_value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        let token = token.trim();
        if token.is_empty() {
            None
        } else {
            Some(token)
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_bearer_parses_scheme_case_insensitively() {
        assert_eq!(extract_bearer("Bearer abc"), Some("abc"));
        assert_eq!(extract_bearer("bearer abc"), Some("abc"));
        assert_eq!(extract_bearer("BEARER   abc  "), Some("abc"));
        assert_eq!(extract_bearer("Basic abc"), None);
        assert_eq!(extract_bearer("Bearer "), None);
        assert_eq!(extract_bearer("abc"), None);
    }

    #[test]
    fn generate_token_is_64_hex_chars() {
        let token = generate_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(generate_token(), generate_token());
    }

    fn enabled_auth(token: &str) -> AuthState {
        AuthState {
            enabled: true,
            token: Some(SecretString::new(token.to_string())),
            allowed_origins: Arc::new(Vec::new()),
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn disabled_auth_allows_unauthenticated_requests() {
        assert!(authorize(&headers(&[]), &AuthState::disabled()).is_none());
    }

    #[test]
    fn enabled_auth_rejects_missing_token_with_401_and_challenge() {
        let response = authorize(&headers(&[]), &enabled_auth("secret")).expect("rejection");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer")
        );
    }

    #[test]
    fn enabled_auth_rejects_wrong_token() {
        let response = authorize(
            &headers(&[("authorization", "Bearer wrong")]),
            &enabled_auth("secret"),
        )
        .expect("rejection");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn enabled_auth_accepts_matching_token() {
        assert!(authorize(
            &headers(&[("authorization", "Bearer secret")]),
            &enabled_auth("secret"),
        )
        .is_none());
    }

    #[test]
    fn origin_without_allowlist_is_rejected_even_when_auth_disabled() {
        let response = authorize(
            &headers(&[("origin", "https://evil.example")]),
            &AuthState::disabled(),
        )
        .expect("rejection");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn allowed_origin_passes_with_valid_token() {
        let auth = AuthState {
            enabled: true,
            token: Some(SecretString::new("secret".to_string())),
            allowed_origins: Arc::new(vec!["https://app.example".to_string()]),
        };
        assert!(authorize(
            &headers(&[
                ("origin", "https://app.example"),
                ("authorization", "Bearer secret"),
            ]),
            &auth,
        )
        .is_none());
    }

    #[test]
    fn absent_origin_is_allowed_for_non_browser_clients() {
        // No Origin header + valid token => allowed (Claude Code / curl path).
        assert!(authorize(
            &headers(&[("authorization", "Bearer secret")]),
            &enabled_auth("secret"),
        )
        .is_none());
    }
}
