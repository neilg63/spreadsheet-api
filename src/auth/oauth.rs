//! GitHub/Google login for the demo (authorization code flow with PKCE).
//!
//!   GET  /auth/{provider}/login?return_to=/datasets   -> redirect to the provider
//!   GET  /auth/{provider}/callback                      -> session cookie, redirect back
//!   POST /auth/logout
//!   GET  /auth/me                                       -> account type, profile, allowance

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl,
    Scope, TokenResponse, TokenUrl, basic::BasicClient,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AccountKind, AuthMode, AuthSettings, Caller, Provider, session};
use crate::{
    db::{run, run_one},
    error::{ApiError, ApiResult},
    models::Me,
    routes::AppState,
    store,
};

const LOGIN_COOKIE: &str = "sheet_login";

fn provider<'a>(state: &'a AppState, name: &str) -> ApiResult<&'a Provider> {
    if state.auth.mode != AuthMode::Demo {
        return Err(ApiError::NotFound("logins are handled by the primary API in service mode".into()));
    }
    state.auth.provider(name).ok_or_else(|| ApiError::NotFound(format!("'{name}' sign-in is not configured")))
}

fn redirect_url(auth: &AuthSettings, provider: &Provider) -> ApiResult<RedirectUrl> {
    RedirectUrl::new(format!("{}/auth/{}/callback", auth.public_url, provider.name))
        .map_err(|e| ApiError::Internal(format!("PUBLIC_URL is not a valid URL: {e}")))
}

/// Where a login may send the browser back to: a path on the frontend, never another site.
pub fn safe_return_to(auth: &AuthSettings, requested: Option<&str>) -> String {
    let base = auth.frontend_url.clone().unwrap_or_default();
    let root = if base.is_empty() { "/".to_string() } else { format!("{base}/") };
    match requested.map(str::trim) {
        Some(path) if path.starts_with('/') && !path.starts_with("//") && !path.contains('\\') => format!("{base}{path}"),
        Some(url) if !base.is_empty() && (url == base || url.starts_with(&root)) => url.to_string(),
        _ => root,
    }
}

#[derive(Debug, Deserialize)]
pub struct LoginParams {
    pub return_to: Option<String>,
}

pub async fn login(
    State(state): State<AppState>,
    caller: Caller,
    Path(name): Path<String>,
    Query(params): Query<LoginParams>,
) -> ApiResult<Response> {
    let provider = provider(&state, &name)?;
    let client = BasicClient::new(ClientId::new(provider.client_id.clone()))
        .set_auth_uri(AuthUrl::new(provider.auth_url.clone()).map_err(|e| ApiError::Internal(e.to_string()))?)
        .set_redirect_uri(redirect_url(&state.auth, provider)?);
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, csrf) = client
        .authorize_url(CsrfToken::new_random)
        .add_scopes(provider.scopes.iter().cloned().map(Scope::new))
        .set_pkce_challenge(challenge)
        .url();
    let guest = caller.is_guest().then(|| caller.me.key());
    run(
        &state.db,
        "CREATE type::record('auth_states', $state) CONTENT {
            provider: $provider, pkce_verifier: $verifier, return_to: $return_to,
            guest: IF $guest { type::record('principals', $guest) } ELSE { NONE }
        } RETURN NONE;",
        json!({
            "state": csrf.secret(), "provider": provider.name, "verifier": verifier.secret(),
            "return_to": safe_return_to(&state.auth, params.return_to.as_deref()), "guest": guest,
        }),
    )
    .await?;
    // ties the callback to this browser, so a forged callback link can't log someone in
    let cookie = session::set_cookie(&state.auth, LOGIN_COOKIE, csrf.secret(), 600);
    Ok(([(header::SET_COOKIE, cookie)], Redirect::to(url.as_str())).into_response())
}

#[derive(Debug, Deserialize)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LoginState {
    provider: String,
    pkce_verifier: String,
    return_to: String,
    guest: Option<Vec<String>>,
    live: bool,
}

/// What a provider tells us about the person.
#[derive(Debug, Default, PartialEq)]
pub struct Profile {
    pub id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub avatar: Option<String>,
    pub locale: Option<String>,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

pub fn parse_profile(provider: &str, value: &Value) -> Option<Profile> {
    if provider == "github" {
        let id = match &value["id"] {
            Value::Number(n) => n.to_string(),
            Value::String(s) if !s.is_empty() => s.clone(),
            _ => return None,
        };
        Some(Profile {
            id,
            email: text(value, "email"),
            name: text(value, "name").or_else(|| text(value, "login")),
            avatar: text(value, "avatar_url"),
            locale: None,
        })
    } else {
        // OpenID Connect userinfo
        let verified = value.get("email_verified").and_then(Value::as_bool).unwrap_or(true);
        Some(Profile {
            id: text(value, "sub")?,
            email: text(value, "email").filter(|_| verified),
            name: text(value, "name"),
            avatar: text(value, "picture"),
            locale: text(value, "locale"),
        })
    }
}

async fn fetch_json(state: &AppState, url: &str, token: &str) -> ApiResult<Value> {
    let response = state
        .http
        .get(url)
        .bearer_auth(token)
        .header(header::ACCEPT, "application/json")
        .header(header::USER_AGENT, "spreadsheet-api")
        .send()
        .await
        .map_err(|e| ApiError::BadGateway(format!("the sign-in provider did not respond: {e}")))?;
    if !response.status().is_success() {
        return Err(ApiError::BadGateway(format!("the sign-in provider returned {}", response.status())));
    }
    response.json().await.map_err(|e| ApiError::BadGateway(format!("unexpected profile response: {e}")))
}

async fn fetch_profile(state: &AppState, provider: &Provider, token: &str) -> ApiResult<Profile> {
    let value = fetch_json(state, &provider.profile_url, token).await?;
    let mut profile =
        parse_profile(&provider.name, &value).ok_or_else(|| ApiError::BadGateway("the provider sent no user id".into()))?;
    if profile.email.is_none()
        && let Some(url) = &provider.emails_url {
            // GitHub hides private emails from /user; /user/emails lists them
            if let Ok(Value::Array(emails)) = fetch_json(state, url, token).await {
                profile.email = emails
                    .iter()
                    .find(|e| e["primary"] == true && e["verified"] == true)
                    .and_then(|e| text(e, "email"));
            }
        }
    Ok(profile)
}

fn back_with_error(return_to: &str, reason: &str) -> Response {
    let glue = if return_to.contains('?') { '&' } else { '?' };
    let reason: String = reason.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    Redirect::to(&format!("{return_to}{glue}login_error={reason}")).into_response()
}

pub async fn callback(
    State(state): State<AppState>,
    caller: Caller,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(params): Query<CallbackParams>,
) -> ApiResult<Response> {
    let provider = provider(&state, &name)?;
    let state_token = params.state.clone().filter(|s| !s.is_empty()).ok_or_else(|| ApiError::BadRequest("missing state".into()))?;
    if session::cookie(&headers, LOGIN_COOKIE).as_deref() != Some(state_token.as_str()) {
        return Err(ApiError::BadRequest("this sign-in was started in another browser or has expired; try again".into()));
    }
    // single use: read and delete in one go
    let row = run_one(
        &state.db,
        "LET $s = type::record('auth_states', $state);
        LET $row = (SELECT provider, pkce_verifier, return_to, IF guest { record::id(guest) } AS guest,
            expires_at > time::now() AS live FROM ONLY $s);
        DELETE $s;
        RETURN $row;",
        json!({ "state": state_token }),
    )
    .await?;
    let login: LoginState = row
        .and_then(|r| serde_json::from_value(r).ok())
        .filter(|s: &LoginState| s.live && s.provider == provider.name)
        .ok_or_else(|| ApiError::BadRequest("this sign-in link has expired; try again".into()))?;
    let clear_login = session::set_cookie(&state.auth, LOGIN_COOKIE, "", 0);
    if let Some(error) = &params.error {
        let mut response = back_with_error(&login.return_to, error);
        response.headers_mut().append(header::SET_COOKIE, clear_login);
        return Ok(response);
    }
    let code = params.code.clone().ok_or_else(|| ApiError::BadRequest("missing code".into()))?;

    let client = BasicClient::new(ClientId::new(provider.client_id.clone()))
        .set_client_secret(ClientSecret::new(provider.client_secret.clone()))
        .set_token_uri(TokenUrl::new(provider.token_url.clone()).map_err(|e| ApiError::Internal(e.to_string()))?)
        .set_redirect_uri(redirect_url(&state.auth, provider)?);
    let token = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(PkceCodeVerifier::new(login.pkce_verifier))
        .request_async(&state.http)
        .await
        .map_err(|e| ApiError::BadGateway(format!("the sign-in provider rejected the code: {e}")))?;
    let profile = fetch_profile(&state, provider, token.access_token().secret()).await?;

    let me = Me { issuer: provider.name.clone(), external_id: profile.id.clone() };
    store::touch_principal(&state.db, &me).await?;
    save_social_user(&state, &me, &provider.name, &profile).await?;
    if let Some([issuer, external_id]) = login.guest.as_deref() {
        let guest = Me { issuer: issuer.clone(), external_id: external_id.clone() };
        let moved = store::claim_guest_data(&state.db, &guest, &me).await?;
        tracing::info!("{}/{} signed in and kept {moved} guest collections", me.issuer, me.external_id);
    }
    let token = session::create(&state.db, &state.auth, &me, caller.user_agent.as_deref(), caller.ip.as_deref()).await?;
    let session_cookie = session::set_cookie(&state.auth, &state.auth.cookie_name, &token, state.auth.session_days * 86_400);
    let mut response = Redirect::to(&login.return_to).into_response();
    response.headers_mut().append(header::SET_COOKIE, session_cookie);
    response.headers_mut().append(header::SET_COOKIE, clear_login);
    Ok(response)
}

async fn save_social_user(state: &AppState, me: &Me, provider: &str, profile: &Profile) -> ApiResult<()> {
    run(
        &state.db,
        "LET $p = type::record('principals', $me);
        LET $existing = (SELECT VALUE id FROM social_users WHERE principal = $p LIMIT 1)[0];
        LET $details = {
            provider: $provider,
            email: IF $email { $email } ELSE { NONE },
            name: IF $name { $name } ELSE { NONE },
            avatar_uri: IF $avatar { $avatar } ELSE { NONE },
            locale: IF $locale { $locale } ELSE { NONE },
            last_login_at: time::now()
        };
        IF $existing { UPDATE $existing MERGE $details RETURN NONE }
        ELSE { CREATE social_users CONTENT $details.extend({ principal: $p }) RETURN NONE };",
        json!({
            "me": me.key(), "provider": provider,
            "email": profile.email.clone().unwrap_or_default(), "name": profile.name.clone().unwrap_or_default(),
            "avatar": profile.avatar.clone().unwrap_or_default(), "locale": profile.locale.clone().unwrap_or_default(),
        }),
    )
    .await?;
    Ok(())
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = session::cookie(&headers, &state.auth.cookie_name) {
        session::delete(&state.db, &token).await?;
    }
    let clear = session::set_cookie(&state.auth, &state.auth.cookie_name, "", 0);
    Ok(([(header::SET_COOKIE, clear)], StatusCode::NO_CONTENT).into_response())
}

/// The caller's account type, profile and allowance, plus how to sign in.
pub async fn me(State(state): State<AppState>, caller: Caller) -> ApiResult<Json<Value>> {
    let allowance = store::touch_principal(&state.db, &caller.me).await?;
    let profile = if caller.kind == AccountKind::Registered {
        run_one(
            &state.db,
            "SELECT provider, email, name, avatar_uri, locale, last_login_at FROM social_users
                WHERE principal = type::record('principals', $me) LIMIT 1;",
            json!({ "me": caller.me.key() }),
        )
        .await?
    } else {
        None
    };
    let login: Vec<Value> = if state.auth.mode == AuthMode::Demo && caller.kind == AccountKind::Guest {
        state
            .auth
            .providers
            .iter()
            .map(|p| json!({ "provider": p.name, "url": format!("{}/auth/{}/login", state.auth.public_url, p.name) }))
            .collect()
    } else {
        vec![]
    };
    Ok(Json(json!({
        "account": caller.kind,
        "user_ref": format!("{}/{}", caller.me.issuer, caller.me.external_id),
        "profile": profile,
        "allowance": allowance,
        "login": login,
        "max_upload_bytes": state.max_upload_bytes(&caller),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_paths_stay_on_the_frontend() {
        let mut auth = AuthSettings::from_env();
        auth.frontend_url = Some("https://sheets.example.com".into());
        assert_eq!(safe_return_to(&auth, Some("/datasets?x=1")), "https://sheets.example.com/datasets?x=1");
        assert_eq!(safe_return_to(&auth, Some("https://sheets.example.com/a")), "https://sheets.example.com/a");
        for hostile in ["//evil.com", "https://evil.com", "https://sheets.example.com.evil.com/", "/\\evil.com", "javascript:x"] {
            assert_eq!(safe_return_to(&auth, Some(hostile)), "https://sheets.example.com/", "{hostile}");
        }
        auth.frontend_url = None;
        assert_eq!(safe_return_to(&auth, Some("/app")), "/app");
        assert_eq!(safe_return_to(&auth, Some("https://evil.com")), "/");
    }

    #[test]
    fn reads_github_and_google_profiles() {
        let github = json!({ "id": 583231, "login": "octocat", "name": null, "email": null, "avatar_url": "https://a/x.png" });
        assert_eq!(
            parse_profile("github", &github),
            Some(Profile { id: "583231".into(), name: Some("octocat".into()), avatar: Some("https://a/x.png".into()), ..Default::default() })
        );
        let google = json!({ "sub": "1098", "email": "a@b.co", "email_verified": false, "name": "A B", "locale": "en" });
        let profile = parse_profile("google", &google).unwrap();
        assert_eq!((profile.id.as_str(), profile.email), ("1098", None), "unverified emails are dropped");
        assert_eq!(parse_profile("google", &json!({ "email": "x" })), None);
    }
}
