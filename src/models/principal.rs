use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Datetime, RecordLink};
use crate::error::{ApiError, ApiResult};

/// `principals`: a caller, keyed `[issuer, external_id]`, so `google/1098...` is
/// `principals:['google', '1098...']`: a guest (`guest/<hash>`), a registered social login
/// (`github/<id>`, `google/<sub>`) or a forwarded service tenant (`service/<ref>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Principal {
    pub id: RecordLink,
    pub issuer: RecordLink,
    pub external_id: String,
    pub status: PrincipalStatus,
    /// Plan override; `None` = the issuer's default plan.
    pub plan: Option<RecordLink>,
    /// When the override ends.
    pub plan_until: Option<Datetime>,
    /// Running sum of `credit_ledger` (unused: there are no paid accounts).
    pub credits: i64,
    pub last_seen_at: Datetime,
    /// Set for issuers with a `principal_ttl` (anonymous principals).
    pub expires_at: Option<Datetime>,
    pub created_at: Datetime,
    pub updated_at: Datetime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalStatus {
    Active,
    Blocked,
}

/// A reference to a principal, `<issuer>/<external id>`, e.g. `github/583231` or `service/acme/42`.
#[derive(Debug, Clone)]
pub struct Me {
    pub issuer: String,
    pub external_id: String,
}

impl Me {
    pub fn parse(user_ref: &str) -> ApiResult<Self> {
        let user_ref = user_ref.trim();
        match user_ref.split_once('/') {
            Some((issuer, external_id)) if !issuer.is_empty() && !external_id.is_empty() => Ok(Me {
                issuer: issuer.to_string(),
                external_id: external_id.to_string(),
            }),
            _ => Err(ApiError::BadRequest("user references look like <issuer>/<id>, e.g. google/10983".into())),
        }
    }

    pub fn key(&self) -> Value {
        json!([self.issuer, self.external_id])
    }

    pub fn owns(&self, owner: &[String]) -> bool {
        owner.len() == 2 && owner[0] == self.issuer && owner[1] == self.external_id
    }
}
