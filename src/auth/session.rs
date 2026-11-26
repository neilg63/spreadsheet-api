//! Browser sessions after a social login: an opaque random token in an HttpOnly cookie,
//! stored only as its SHA-256.

use axum::http::{HeaderMap, HeaderValue, header};
use sha2::{Digest, Sha256};
use sqlx::Row;

use super::AuthSettings;
use crate::{
    db::{Database, NOW, after},
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
    let sql = format!("INSERT INTO auth_sessions (principal, token_hash, user_agent, ip, expires_at) VALUES (?, ?, ?, ?, {})", after("?"));
    sqlx::query(&sql)
        .bind(me.id())
        .bind(sha256_hex(&[&token]))
        .bind(user_agent.map(|ua| ua.chars().take(300).collect::<String>()))
        .bind(ip)
        .bind(auth.session_days * 86_400)
        .execute(db)
        .await?;
    Ok(token)
}

/// The signed-in principal for a session token, if the session is live.
pub async fn lookup(db: &Database, token: &str) -> ApiResult<Option<Me>> {
    let hash = sha256_hex(&[token]);
    let row = sqlx::query(&format!(
        "SELECT principal, last_seen_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1 hour') AS stale
           FROM auth_sessions WHERE token_hash = ? AND expires_at > {NOW}"
    ))
    .bind(&hash)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else { return Ok(None) };
    if row.try_get::<bool, _>("stale")? {
        sqlx::query(&format!("UPDATE auth_sessions SET last_seen_at = {NOW} WHERE token_hash = ?")).bind(&hash).execute(db).await?;
    }
    Ok(Me::parse(&row.try_get::<String, _>("principal")?).ok())
}

pub async fn delete(db: &Database, token: &str) -> ApiResult<()> {
    sqlx::query("DELETE FROM auth_sessions WHERE token_hash = ?")
        .bind(sha256_hex(&[token]))
        .execute(db)
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
