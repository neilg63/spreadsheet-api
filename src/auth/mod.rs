//! Who is calling. One extractor, two deployments:
//!
//! - **demo**: a session cookie from a GitHub/Google login makes a *registered* caller
//!   (`github/<id>`, `google/<sub>`); anyone else is a *guest*, keyed by a salted hash of
//!   client address and user agent (`guest/<hash>`). Identity headers are ignored.
//! - **service**: nginx and the primary API authenticate users and forward a tenant or
//!   user reference (`X-User-Ref: acme/42` -> `service/acme/42`), optionally alongside a
//!   shared service token.
//!
//! In either deployment, a request may instead carry an access token signed by a
//! registered application (`Authorization: Bearer <jwt>`, see [`token`]). The caller is
//! then that application's principal, and may do only what the token grants.

pub mod oauth;
pub mod rate_limit;
pub mod session;
mod settings;
pub mod token;

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
pub use token::{Access, Action, Constraints};

use crate::{
    error::{ApiError, ApiResult},
    models::{DatasetInfo, Me, Visibility},
    routes::AppState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Guest,
    Registered,
    Service,
    /// Acting through an access token signed by a registered application.
    Token,
}

pub struct Caller {
    pub me: Me,
    pub kind: AccountKind,
    /// Full rights over what it owns, or only what an access token grants.
    pub access: Access,
    /// The client address, when known (demo mode).
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl Caller {
    pub fn is_guest(&self) -> bool {
        self.kind == AccountKind::Guest
    }

    /// Whether the caller may upload new datasets, or list its own (`action`).
    pub fn require_own(&self, action: Action) -> ApiResult<()> {
        if self.access.allows_own(action) {
            return Ok(());
        }
        Err(ApiError::Forbidden("this access token does not grant that on 'datasets'".into()))
    }
}

/// Decides whether a request may do `action` on `dataset`, and under which constraints.
///
/// The owner may do anything. An access token may do what a grant of its allows, on data
/// of its own issuer. Anyone may read data that is not private. Everything else is
/// answered as if the dataset did not exist.
pub fn authorise(caller: Option<&Caller>, action: Action, dataset: &DatasetInfo) -> ApiResult<Constraints> {
    if let Some(constraints) = caller.and_then(|c| c.access.permit(&c.me, action, dataset)) {
        return Ok(constraints);
    }
    if matches!(action, Action::Read | Action::Export) && dataset.visibility != Visibility::Private {
        return Ok(Constraints::default());
    }
    Err(ApiError::NotFound(format!("dataset {} not found", dataset.id)))
}

/// The caller of a route anyone may use: `None` when the request carries no identity.
pub struct MaybeCaller(pub Option<Caller>);

impl FromRequestParts<AppState> for MaybeCaller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        // an access token that fails to verify is an error; a missing identity is not
        let presented_token = bearer(&parts.headers).is_some_and(token::looks_like_jwt);
        match Caller::from_request_parts(parts, state).await {
            Ok(caller) => Ok(MaybeCaller(Some(caller))),
            Err(ApiError::Unauthorized(_)) if !presented_token => Ok(MaybeCaller(None)),
            Err(err) => Err(err),
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    header_str(headers, &header::AUTHORIZATION).and_then(|v| v.strip_prefix("Bearer ")).map(str::trim)
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
        if let Some(jwt) = bearer(&parts.headers).filter(|b| token::looks_like_jwt(b)) {
            let (me, grants) = token::verify(&state.db, jwt).await?;
            let ip = client_ip(&parts.headers, &parts.extensions, auth);
            return Ok(Caller { me, kind: AccountKind::Token, access: Access::Grants(grants), ip, user_agent });
        }
        match auth.mode {
            AuthMode::Service => {
                if let Some(expected) = &auth.service_token {
                    let token = bearer(&parts.headers).or_else(|| header_str(&parts.headers, &header::HeaderName::from_static("x-service-token")));
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
                    access: Access::Owner,
                    ip: None,
                    user_agent,
                })
            }
            AuthMode::Demo => {
                let ip = client_ip(&parts.headers, &parts.extensions, auth);
                if let Some(token) = session::cookie(&parts.headers, &auth.cookie_name)
                    && let Some(me) = session::lookup(&state.db, &token).await? {
                        return Ok(Caller { me, kind: AccountKind::Registered, access: Access::Owner, ip, user_agent });
                    }
                let me = guest_me(auth, ip.as_deref(), user_agent.as_deref());
                Ok(Caller { me, kind: AccountKind::Guest, access: Access::Owner, ip, user_agent })
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
