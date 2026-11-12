//! Browser sessions after a social login: an opaque random token in an HttpOnly cookie,
//! stored only as its SHA-256.

use axum::http::{HeaderMap, HeaderValue, header};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::AuthSettings;
use crate::{
    db::{Database, run, run_last},
    error::{ApiError, ApiResult},
    models::Me,
};

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]); // separator, so ("ab","c") and ("a","bc") differ
    }
    hex(&hasher.finalize())
}

/// 244 random bits from the OS generator (two v4 UUIDs).
pub fn new_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `Set-Cookie` for `name=value`; `max_age` 0 deletes it.
pub fn set_cookie(auth: &AuthSettings, name: &str, value: &str, max_age: i64) -> HeaderValue {
    let secure = if auth.cookie_secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"))
        .expect("cookie values are hex")
}

pub async fn create(db: &Database, auth: &AuthSettings, me: &Me, user_agent: Option<&str>, ip: Option<&str>) -> ApiResult<String> {
    let token = new_token();
    run(
        db,
        "CREATE auth_sessions CONTENT {
            principal: type::record('principals', $me), token_hash: $hash,
            user_agent: IF $ua { $ua } ELSE { NONE }, ip: IF $ip { $ip } ELSE { NONE },
            expires_at: time::now() + duration::from_days($days)
        } RETURN NONE;",
        json!({
            "me": me.key(), "hash": sha256_hex(&[&token]), "days": auth.session_days,
            "ua": user_agent.unwrap_or_default().chars().take(300).collect::<String>(), "ip": ip.unwrap_or_default(),
        }),
    )
    .await?;
    Ok(token)
}

/// The signed-in principal for a session token, if the session is live.
pub async fn lookup(db: &Database, token: &str) -> ApiResult<Option<Me>> {
    // run_last, not run_one: the key itself is an array ([issuer, id])
    let key = run_last(
        db,
        "LET $s = (SELECT * FROM auth_sessions WHERE token_hash = $hash AND expires_at > time::now() LIMIT 1)[0];
        IF $s AND $s.last_seen_at < time::now() - 1h { UPDATE $s.id SET last_seen_at = time::now() RETURN NONE };
        RETURN IF $s { record::id($s.principal) } ELSE { NONE };",
        json!({ "hash": sha256_hex(&[token]) }),
    )
    .await?;
    let key: Option<Vec<String>> = serde_json::from_value(key).ok();
    Ok(match key.as_deref() {
        Some([issuer, external_id]) => Some(Me { issuer: issuer.clone(), external_id: external_id.clone() }),
        _ => None,
    })
}

pub async fn delete(db: &Database, token: &str) -> ApiResult<()> {
    run(db, "DELETE auth_sessions WHERE token_hash = $hash;", json!({ "hash": sha256_hex(&[token]) }))
        .await
        .map(|_| ())
        .map_err(|e| ApiError::Internal(format!("could not end the session: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cookies_and_hashes_unambiguously() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_static("a=1; sheet_session=abc123 ; b=2"));
        assert_eq!(cookie(&headers, "sheet_session").as_deref(), Some("abc123"));
        assert_eq!(cookie(&headers, "missing"), None);
        assert_ne!(sha256_hex(&["ab", "c"]), sha256_hex(&["a", "bc"]));
        assert_eq!(new_token().len(), 64);
    }
}
