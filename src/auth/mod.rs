//! Who is calling. One extractor, two deployments:
//!
//! - **demo**: a session cookie from a GitHub/Google login makes a *registered* caller
//!   (`github/<id>`, `google/<sub>`); anyone else is a *guest*, keyed by a salted hash of
//!   client address and user agent (`guest/<hash>`). Identity headers are ignored.
//! - **service**: nginx and the primary API authenticate users and forward a tenant or
//!   user reference (`X-User-Ref: acme/42` -> `service/acme/42`), optionally alongside a
//!   shared service token.

pub mod oauth;
pub mod rate_limit;
pub mod session;
mod settings;

use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, FromRequestParts, Request, State},
    http::{HeaderMap, header, request::Parts},
    middleware::Next,
    response::Response,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub use settings::{AuthMode, AuthSettings, Provider};

use crate::{error::ApiError, models::Me, routes::AppState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Guest,
    Registered,
    Service,
}

pub struct Caller {
    pub me: Me,
    pub kind: AccountKind,
    /// The client address, when known (demo mode).
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl Caller {
    pub fn is_guest(&self) -> bool {
        self.kind == AccountKind::Guest
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &header::HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty())
}

/// The client address: the configured proxy header, else the socket peer.
pub fn client_ip(parts_headers: &HeaderMap, extensions: &axum::http::Extensions, auth: &AuthSettings) -> Option<String> {
    if let Some(name) = &auth.client_ip_header
        && let Some(value) = header_str(parts_headers, name) {
            // X-Forwarded-For style lists: the first entry is the client
            return value.split(',').next().map(|ip| ip.trim().to_string());
        }
    extensions.get::<ConnectInfo<SocketAddr>>().map(|ConnectInfo(addr)| addr.ip().to_string())
}

/// `guest/<32 hex chars>`: stable for one browser on one network, not reversible
/// without the salt.
pub fn guest_me(auth: &AuthSettings, ip: Option<&str>, user_agent: Option<&str>) -> Me {
    let hash = session::sha256_hex(&[&auth.guest_salt, ip.unwrap_or("unknown"), user_agent.unwrap_or("")]);
    Me { issuer: "guest".into(), external_id: hash[..32].to_string() }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A forwarded tenant/user reference: printable, without whitespace, at most 200 chars.
fn valid_ref(value: &str) -> bool {
    !value.is_empty() && value.len() <= 200 && value.chars().all(|c| c.is_ascii_graphic())
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let auth = &state.auth;
        let user_agent = header_str(&parts.headers, &header::USER_AGENT).map(str::to_string);
        match auth.mode {
            AuthMode::Service => {
                if let Some(expected) = &auth.service_token {
                    let bearer = header_str(&parts.headers, &header::AUTHORIZATION).and_then(|v| v.strip_prefix("Bearer "));
                    let token = bearer.or_else(|| header_str(&parts.headers, &header::HeaderName::from_static("x-service-token")));
                    if !token.is_some_and(|t| constant_time_eq(t.trim(), expected)) {
                        return Err(ApiError::Unauthorized("a valid service token is required".into()));
                    }
                }
                let reference = header_str(&parts.headers, &auth.user_header)
                    .map(str::to_string)
                    .or_else(|| auth.default_ref.clone())
                    .ok_or_else(|| ApiError::Unauthorized(format!("the {} header is required", auth.user_header)))?;
                if !valid_ref(&reference) {
                    return Err(ApiError::BadRequest(format!("{} must be printable ASCII without spaces, at most 200 characters", auth.user_header)));
                }
                Ok(Caller {
                    me: Me { issuer: "service".into(), external_id: reference },
                    kind: AccountKind::Service,
                    ip: None,
                    user_agent,
                })
            }
            AuthMode::Demo => {
                let ip = client_ip(&parts.headers, &parts.extensions, auth);
                if let Some(token) = session::cookie(&parts.headers, &auth.cookie_name)
                    && let Some(me) = session::lookup(&state.db, &token).await? {
                        return Ok(Caller { me, kind: AccountKind::Registered, ip, user_agent });
                    }
                let me = guest_me(auth, ip.as_deref(), user_agent.as_deref());
                Ok(Caller { me, kind: AccountKind::Guest, ip, user_agent })
            }
        }
    }
}

/// Demo-mode request rate limit per client address (uploads cost more).
pub async fn rate_limit(State(state): State<AppState>, request: Request, next: Next) -> Result<Response, ApiError> {
    if state.auth.mode == AuthMode::Demo && state.limiter.enabled()
        && let Some(ip) = client_ip(request.headers(), request.extensions(), &state.auth) {
            let cost = if request.uri().path().ends_with("/upload") || request.uri().path().ends_with("/import") { 10 } else { 1 };
            if let Err(retry_after) = state.limiter.check(&ip, cost) {
                return Err(ApiError::TooManyRequests(retry_after));
            }
        }
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_ids_depend_on_salt_address_and_browser() {
        let mut auth = AuthSettings::from_env();
        auth.guest_salt = "salt-a".into();
        let a = guest_me(&auth, Some("1.2.3.4"), Some("Firefox"));
        assert_eq!(a.issuer, "guest");
        assert_eq!(a.external_id.len(), 32);
        assert_eq!(a.external_id, guest_me(&auth, Some("1.2.3.4"), Some("Firefox")).external_id);
        assert_ne!(a.external_id, guest_me(&auth, Some("1.2.3.5"), Some("Firefox")).external_id);
        assert_ne!(a.external_id, guest_me(&auth, Some("1.2.3.4"), Some("Safari")).external_id);
        auth.guest_salt = "salt-b".into();
        assert_ne!(a.external_id, guest_me(&auth, Some("1.2.3.4"), Some("Firefox")).external_id);
    }

    #[test]
    fn validates_forwarded_refs_and_tokens() {
        assert!(valid_ref("acme/42"));
        assert!(!valid_ref("acme 42"));
        assert!(!valid_ref(""));
        assert!(!valid_ref(&"x".repeat(201)));
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secreT"));
    }
}
