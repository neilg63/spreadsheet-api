//! Access tokens signed by another application, and what they allow.
//!
//! An application is registered as an *issuer* with one or more Ed25519 public keys
//! (`ISSUERS_FILE`). It then controls access by signing short-lived JWTs:
//!
//! ```json
//! { "iss": "mainusers", "sub": "acme/42", "aud": "spreadsheet-api",
//!   "iat": 1795729200, "exp": 1795732800,
//!   "grants": [{ "resource": "dataset/0199…", "actions": ["read", "export"],
//!                "constraints": { "filter": { "region": "north" }, "fields": ["sku", "price"] } }] }
//! ```
//!
//! The caller is the principal `<iss>/<sub>`, and may do only what the grants list. A
//! token never reaches beyond its issuer: grants apply only to data owned by principals
//! of the same issuer. The service holds no secret; it verifies with the public key.

use std::path::Path;

use base64::Engine;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;

use crate::{
    config::config,
    db::{self, Database, NOW},
    error::{ApiError, ApiResult},
    models::{DatasetInfo, Me, principal::issuer_of},
};

/// What a grant can allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Query rows: GET /datasets/{id}, and the list of datasets.
    Read,
    /// Download every row: GET /datasets/{id}/export.
    Export,
    /// Create datasets: POST /datasets/upload.
    Upload,
    /// Preview, change the mapping and set who may read.
    Configure,
    Import,
}

/// Limits a grant puts on reading.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Constraints {
    /// A filter always applied to the rows, in either form GET /datasets/{id} accepts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
    /// The only fields returned: top-level keys or dotted paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
    /// The most rows one request may return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_page_size: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    /// `datasets` (everything the token's own principal owns), `dataset/<id>` or
    /// `collection/<id>`.
    pub resource: String,
    pub actions: Vec<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<Constraints>,
}

impl Grant {
    fn valid(&self) -> bool {
        match self.resource.split_once('/') {
            None => self.resource == "datasets",
            Some((kind, id)) => matches!(kind, "dataset" | "collection") && !id.is_empty(),
        }
    }

    /// Whether this grant covers `dataset` for a token whose principal is `me`.
    fn covers(&self, me: &Me, dataset: &DatasetInfo) -> bool {
        match self.resource.split_once('/') {
            None => me.owns(&dataset.owner),
            Some(("dataset", id)) => id == dataset.id,
            Some(("collection", id)) => id == dataset.collection_id,
            Some(_) => false,
        }
    }
}

/// How a caller came by its rights.
#[derive(Debug, Clone, PartialEq)]
pub enum Access {
    /// Signed in, a guest, or forwarded in service mode: full rights over what it owns.
    Owner,
    /// An access token: only what its grants list.
    Grants(Vec<Grant>),
}

impl Access {
    /// The constraints under which `me` may do `action` on `dataset`, or `None` if it may not.
    pub fn permit(&self, me: &Me, action: Action, dataset: &DatasetInfo) -> Option<Constraints> {
        match self {
            Access::Owner => me.owns(&dataset.owner).then(Constraints::default),
            // a token never reaches data owned outside its own issuer
            Access::Grants(_) if issuer_of(&dataset.owner) != me.issuer => None,
            Access::Grants(grants) => grants
                .iter()
                .find(|g| g.actions.contains(&action) && g.covers(me, dataset))
                .map(|g| g.constraints.clone().unwrap_or_default()),
        }
    }

    /// Whether the caller may act on everything its own principal owns: upload new
    /// datasets, or list them.
    pub fn allows_own(&self, action: Action) -> bool {
        match self {
            Access::Owner => true,
            Access::Grants(grants) => grants.iter().any(|g| g.resource == "datasets" && g.actions.contains(&action)),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    exp: u64,
    iat: Option<u64>,
    jti: Option<String>,
    #[serde(default)]
    grants: Vec<Grant>,
}

/// Whether a bearer credential is shaped like a JWT, as opposed to a shared service token.
pub fn looks_like_jwt(bearer: &str) -> bool {
    bearer.starts_with("eyJ") && bearer.split('.').count() == 3
}

fn invalid(reason: impl std::fmt::Display) -> ApiError {
    ApiError::Unauthorized(format!("invalid access token: {reason}"))
}

/// An Ed25519 public key: PEM, or its 32 raw bytes in base64.
fn decoding_key(text: &str) -> Result<DecodingKey, String> {
    let text = text.trim();
    if text.contains("BEGIN") {
        return DecodingKey::from_ed_pem(text.as_bytes()).map_err(|e| format!("not an Ed25519 public key in PEM: {e}"));
    }
    let compact: String = text.chars().filter(|c| !c.is_whitespace() && *c != '=').collect();
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(&compact)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&compact))
        .map_err(|e| format!("not base64: {e}"))?;
    if bytes.len() != 32 {
        return Err(format!("an Ed25519 public key is 32 bytes, not {}", bytes.len()));
    }
    Ok(DecodingKey::from_ed_der(&bytes))
}

/// The `iss` claim, read before the signature can be checked, to find the key to check it with.
fn unverified_issuer(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?.get("iss")?.as_str().map(str::to_string)
}

/// Verifies an access token and returns its principal and grants.
pub async fn verify(db: &Database, token: &str) -> ApiResult<(Me, Vec<Grant>)> {
    let header = decode_header(token).map_err(invalid)?;
    if header.alg != Algorithm::EdDSA {
        return Err(invalid("it must be signed with EdDSA (Ed25519)"));
    }
    let issuer = unverified_issuer(token).ok_or_else(|| invalid("it has no iss claim"))?;
    let row = sqlx::query("SELECT enabled, max_token_secs FROM issuers WHERE id = ? AND trust = 'managed'")
        .bind(&issuer)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| invalid(format!("'{issuer}' is not a registered issuer")))?;
    if !row.try_get::<bool, _>("enabled")? {
        return Err(invalid(format!("issuer '{issuer}' is disabled")));
    }
    let max_secs = row.try_get::<Option<i64>, _>("max_token_secs")?.map_or(config().max_token_secs, |s| s.max(0) as u64);

    // the key the header names, or else any of the issuer's current keys
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT public_key FROM issuer_keys WHERE issuer = ?1 AND revoked_at IS NULL AND (?2 IS NULL OR kid = ?2) ORDER BY created_at DESC",
    )
    .bind(&issuer)
    .bind(&header.kid)
    .fetch_all(db)
    .await?;
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_audience(&[config().token_audience.as_str()]);
    validation.set_issuer(&[issuer.as_str()]);
    validation.set_required_spec_claims(&["exp", "iss", "sub", "aud"]);
    validation.leeway = 30;
    let mut failure = "no current key of its issuer matches".to_string();
    let mut claims = None;
    for key in keys.iter().filter_map(|k| decoding_key(k).ok()) {
        match decode::<Claims>(token, &key, &validation) {
            Ok(data) => {
                claims = Some(data.claims);
                break;
            }
            Err(err) => failure = err.to_string(),
        }
    }
    let claims = claims.ok_or_else(|| invalid(failure))?;

    let issued = claims.iat.ok_or_else(|| invalid("it has no iat claim"))?;
    if claims.exp.saturating_sub(issued) > max_secs {
        return Err(invalid(format!("it is valid for longer than the {max_secs} seconds allowed")));
    }
    if claims.iss != issuer || claims.sub.is_empty() || claims.sub.len() > 200 || !claims.sub.chars().all(|c| c.is_ascii_graphic()) {
        return Err(invalid("sub must be printable ASCII without spaces, at most 200 characters"));
    }
    if let Some(bad) = claims.grants.iter().find(|g| !g.valid()) {
        return Err(invalid(format!("'{}' is not a resource; use datasets, dataset/<id> or collection/<id>", bad.resource)));
    }
    if let Some(jti) = &claims.jti {
        let revoked: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM revoked_tokens WHERE issuer = ? AND jti = ?").bind(&issuer).bind(jti).fetch_optional(db).await?;
        if revoked.is_some() {
            return Err(invalid("it has been revoked"));
        }
    }
    Ok((Me { issuer, external_id: claims.sub }, claims.grants))
}

/// One application in `ISSUERS_FILE`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuerConfig {
    /// The issuer id: the `iss` claim, and the first part of its principals' references.
    pub id: String,
    pub name: Option<String>,
    /// The plan its principals are on: `service` unless set.
    pub plan: Option<String>,
    /// Longest token lifetime in seconds; `MAX_TOKEN_SECS` unless set.
    pub max_token_secs: Option<u64>,
    #[serde(default = "enabled")]
    pub enabled: bool,
    pub keys: Vec<IssuerKey>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuerKey {
    pub kid: String,
    /// PEM, or the 32 raw bytes in base64.
    pub public_key: String,
}

fn enabled() -> bool {
    true
}

const BUILT_IN: [&str; 4] = ["guest", "github", "google", "service"];

/// Makes the registered applications exactly those listed: each is added or updated
/// with its keys, a key no longer listed is revoked, and an application no longer listed
/// is disabled.
pub async fn register_issuers(db: &Database, issuers: &[IssuerConfig]) -> ApiResult<()> {
    let problem = |message: String| ApiError::Internal(format!("issuers: {message}"));
    let mut tx = db::write(db).await?;
    for issuer in issuers {
        let id = issuer.id.as_str();
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
            return Err(problem(format!("'{id}' is not an id; use lower-case letters, digits, _ and -")));
        }
        if BUILT_IN.contains(&id) {
            return Err(problem(format!("'{id}' is one of the service's own issuers")));
        }
        for key in &issuer.keys {
            decoding_key(&key.public_key).map_err(|e| problem(format!("{id}, key '{}': {e}", key.kid)))?;
        }
        let plan = issuer.plan.as_deref().unwrap_or("service");
        let known: Option<i64> = sqlx::query_scalar("SELECT 1 FROM plans WHERE id = ?").bind(plan).fetch_optional(&mut *tx).await?;
        if known.is_none() {
            return Err(problem(format!("{id}: there is no plan '{plan}'")));
        }
        sqlx::query(
            "INSERT INTO issuers (id, name, trust, default_plan, max_token_secs, enabled) VALUES (?, ?, 'managed', ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET name = excluded.name, default_plan = excluded.default_plan,
                 max_token_secs = excluded.max_token_secs, enabled = excluded.enabled",
        )
        .bind(id)
        .bind(issuer.name.as_deref().unwrap_or(id))
        .bind(plan)
        .bind(issuer.max_token_secs.map(|s| s as i64))
        .bind(issuer.enabled)
        .execute(&mut *tx)
        .await?;
        let kids: Vec<&str> = issuer.keys.iter().map(|k| k.kid.as_str()).collect();
        sqlx::query(&format!(
            "UPDATE issuer_keys SET revoked_at = {NOW} WHERE issuer = ? AND revoked_at IS NULL AND kid NOT IN (SELECT value FROM json_each(?))"
        ))
        .bind(id)
        .bind(serde_json::to_string(&kids).unwrap_or_default())
        .execute(&mut *tx)
        .await?;
        for key in &issuer.keys {
            sqlx::query(
                "INSERT INTO issuer_keys (issuer, kid, public_key) VALUES (?, ?, ?)
                 ON CONFLICT (issuer, kid) DO UPDATE SET public_key = excluded.public_key, revoked_at = NULL",
            )
            .bind(id)
            .bind(&key.kid)
            .bind(key.public_key.trim())
            .execute(&mut *tx)
            .await?;
        }
    }
    let ids: Vec<&str> = issuers.iter().map(|i| i.id.as_str()).collect();
    sqlx::query("UPDATE issuers SET enabled = 0 WHERE trust = 'managed' AND id NOT IN (SELECT value FROM json_each(?))")
        .bind(serde_json::to_string(&ids).unwrap_or_default())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Reads `ISSUERS_FILE` (a JSON list of applications) and registers what it lists.
pub async fn register_from_file(db: &Database, path: &Path) -> ApiResult<usize> {
    let text = std::fs::read_to_string(path).map_err(|e| ApiError::Internal(format!("cannot read {}: {e}", path.display())))?;
    let issuers: Vec<IssuerConfig> =
        serde_json::from_str(&text).map_err(|e| ApiError::Internal(format!("{} is not a list of issuers: {e}", path.display())))?;
    register_issuers(db, &issuers).await?;
    Ok(issuers.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DatasetStatus, VersionStatus, Visibility};
    use serde_json::json;

    fn dataset(id: &str, owner: &str, collection: &str) -> DatasetInfo {
        DatasetInfo {
            id: id.into(),
            owner: owner.into(),
            status: DatasetStatus::Imported,
            sheet: None,
            series: 1,
            error: None,
            num_rows: 0,
            num_invalid: 0,
            imported_at: None,
            created_at: String::new(),
            mapping_deadline: None,
            expires_at: None,
            upload_id: None,
            storage_key: None,
            purged_at: None,
            sheets: None,
            filename: None,
            collection_id: collection.into(),
            collection_name: collection.into(),
            visibility: Visibility::Private,
            primary_key: None,
            current_id: None,
            version_id: String::new(),
            version: 1,
            version_status: VersionStatus::Published,
            definition: json!({}),
            field_spec: None,
            schema_id: String::new(),
        }
    }

    fn grant(resource: &str, actions: &[Action]) -> Grant {
        Grant { resource: resource.into(), actions: actions.to_vec(), constraints: None }
    }

    #[test]
    fn grants_cover_only_what_they_name_and_never_another_issuer() {
        let me = Me { issuer: "app".into(), external_id: "acme/1".into() };
        let own = dataset("d1", "app/acme/1", "c1");
        let colleague = dataset("d2", "app/acme/2", "c2");
        let foreign = dataset("d3", "other/acme/1", "c1");

        let access = Access::Grants(vec![grant("datasets", &[Action::Read]), grant("dataset/d2", &[Action::Read, Action::Export])]);
        assert!(access.permit(&me, Action::Read, &own).is_some());
        assert!(access.permit(&me, Action::Export, &own).is_none(), "the action must be granted");
        assert!(access.permit(&me, Action::Export, &colleague).is_some(), "a named dataset of the same issuer");
        assert!(access.permit(&me, Action::Import, &colleague).is_none());
        assert!(Access::Grants(vec![grant("collection/c1", &[Action::Read]), grant("dataset/d3", &[Action::Read])])
            .permit(&me, Action::Read, &foreign)
            .is_none());
        assert!(access.allows_own(Action::Read) && !access.allows_own(Action::Upload));

        assert!(Access::Owner.permit(&me, Action::Import, &own).is_some());
        assert!(Access::Owner.permit(&me, Action::Read, &colleague).is_none());
    }

    #[test]
    fn reads_keys_and_checks_resources() {
        assert!(decoding_key("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").is_ok());
        assert!(decoding_key("AAAA").is_err());
        assert!(grant("datasets", &[]).valid() && grant("dataset/x", &[]).valid() && grant("collection/x", &[]).valid());
        assert!(!grant("endpoint/x", &[]).valid() && !grant("dataset/", &[]).valid() && !grant("everything", &[]).valid());
        assert!(looks_like_jwt("eyJhbGciOiJFZERTQSJ9.eyJpc3MiOiJhIn0.c2ln") && !looks_like_jwt("s3cret"));
    }
}
