use std::path::PathBuf;

use axum::http::HeaderName;

use crate::config::parse_size;

/// How callers are identified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// Public demo: guests by IP + user agent, registered users by GitHub/Google login,
    /// request rate limits and smaller guest uploads.
    Demo,
    /// Behind nginx and the primary API, which authenticate users and forward a
    /// tenant/user reference. No login routes; no rate limits of its own.
    Service,
}

/// An OAuth 2 provider for social login. Endpoints default to the public ones and can
/// be overridden (e.g. GitHub Enterprise, or a mock provider in tests).
#[derive(Debug, Clone)]
pub struct Provider {
    /// Also the issuer: principals are `<name>/<provider user id>`.
    pub name: String,
    pub client_id: String,
    pub client_secret: String,
    pub auth_url: String,
    pub token_url: String,
    pub profile_url: String,
    /// GitHub only: where to find a verified email when the profile hides it.
    pub emails_url: Option<String>,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AuthSettings {
    pub mode: AuthMode,

    // --- service mode
    /// Header the primary API sets to the caller's tenant or user reference.
    pub user_header: HeaderName,
    /// Used when the header is missing (e.g. local development); otherwise such requests are rejected.
    pub default_ref: Option<String>,
    /// When set, every request must carry `Authorization: Bearer <token>` or `X-Service-Token`.
    pub service_token: Option<String>,
    /// Directory shared with the primary API, for `{"path": ...}` uploads.
    pub shared_dir: Option<PathBuf>,
    /// Hosts `{"url": ...}` uploads may download from; empty = any host.
    pub url_hosts: Vec<String>,

    // --- demo mode
    /// This service's public base URL, for OAuth redirect URIs, e.g. https://sheets.example.com/api
    pub public_url: String,
    /// Where to send people after login when `return_to` is relative, e.g. https://sheets.example.com
    pub frontend_url: Option<String>,
    /// Header nginx sets to the client address (e.g. X-Real-IP). Only set this behind a proxy
    /// that overwrites it: otherwise clients could spoof it. Unset = the socket peer address.
    pub client_ip_header: Option<HeaderName>,
    /// Mixed into guest identities so they can't be computed from an IP and user agent alone.
    pub guest_salt: String,
    pub cookie_name: String,
    pub cookie_secure: bool,
    pub session_days: i64,
    pub providers: Vec<Provider>,
    /// Requests per minute per client address; 0 disables the limit.
    pub rate_limit_per_minute: u32,
    pub guest_max_upload_bytes: usize,
    /// Origins allowed to call with cookies (a frontend on another origin). Empty = any origin, no cookies.
    pub cors_origins: Vec<String>,
}

fn var(key: &str) -> Option<String> {
    dotenvy::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn list(key: &str) -> Vec<String> {
    var(key).map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
}

fn header(key: &str, default: Option<&str>) -> Option<HeaderName> {
    var(key).or(default.map(str::to_string)).and_then(|h| HeaderName::try_from(h.to_lowercase()).ok())
}

impl AuthSettings {
    pub fn from_env() -> Self {
        let mode = match var("AUTH_MODE").as_deref() {
            Some("service") => AuthMode::Service,
            Some("demo") | None => AuthMode::Demo,
            Some(other) => panic!("AUTH_MODE must be 'demo' or 'service', not '{other}'"),
        };
        let public_url = var("PUBLIC_URL").unwrap_or_else(|| format!("http://localhost:{}", var("PORT").unwrap_or("3000".into())));
        let guest_salt = var("GUEST_SALT").unwrap_or_else(|| {
            if mode == AuthMode::Demo {
                tracing::warn!("GUEST_SALT is not set: guest identities will change when the service restarts");
            }
            uuid::Uuid::new_v4().to_string()
        });
        let mut providers = vec![];
        for (name, auth_url, token_url, profile_url, emails_url, scopes) in [
            (
                "github",
                "https://github.com/login/oauth/authorize",
                "https://github.com/login/oauth/access_token",
                "https://api.github.com/user",
                Some("https://api.github.com/user/emails"),
                "read:user,user:email",
            ),
            (
                "google",
                "https://accounts.google.com/o/oauth2/v2/auth",
                "https://oauth2.googleapis.com/token",
                "https://openidconnect.googleapis.com/v1/userinfo",
                None,
                "openid,email,profile",
            ),
        ] {
            let prefix = name.to_uppercase();
            let (Some(client_id), Some(client_secret)) =
                (var(&format!("{prefix}_CLIENT_ID")), var(&format!("{prefix}_CLIENT_SECRET")))
            else {
                continue;
            };
            providers.push(Provider {
                name: name.to_string(),
                client_id,
                client_secret,
                auth_url: var(&format!("{prefix}_AUTH_URL")).unwrap_or(auth_url.into()),
                token_url: var(&format!("{prefix}_TOKEN_URL")).unwrap_or(token_url.into()),
                profile_url: var(&format!("{prefix}_PROFILE_URL")).unwrap_or(profile_url.into()),
                emails_url: var(&format!("{prefix}_EMAILS_URL")).or(emails_url.map(str::to_string)),
                scopes: scopes.split(',').map(str::to_string).collect(),
            });
        }
        AuthSettings {
            mode,
            user_header: header("SERVICE_USER_HEADER", Some("x-user-ref")).expect("valid SERVICE_USER_HEADER"),
            default_ref: var("DEFAULT_USER_REF"),
            service_token: var("SERVICE_TOKEN"),
            shared_dir: var("SHARED_UPLOAD_DIR").map(PathBuf::from),
            url_hosts: list("URL_UPLOAD_HOSTS").into_iter().map(|h| h.to_lowercase()).collect(),
            cookie_secure: var("COOKIE_SECURE").map(|v| v == "true").unwrap_or(public_url.starts_with("https://")),
            public_url: public_url.trim_end_matches('/').to_string(),
            frontend_url: var("FRONTEND_URL").map(|u| u.trim_end_matches('/').to_string()),
            client_ip_header: header("CLIENT_IP_HEADER", None),
            guest_salt,
            cookie_name: var("SESSION_COOKIE").unwrap_or("sheet_session".into()),
            session_days: var("SESSION_DAYS").and_then(|d| d.parse().ok()).unwrap_or(30),
            providers,
            rate_limit_per_minute: var("RATE_LIMIT_PER_MINUTE").and_then(|r| r.parse().ok()).unwrap_or(120),
            guest_max_upload_bytes: var("GUEST_MAX_UPLOAD_SIZE").and_then(|s| parse_size(&s)).unwrap_or(2 * 1024 * 1024),
            cors_origins: list("CORS_ORIGINS"),
        }
    }

    pub fn provider(&self, name: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.name == name)
    }
}
