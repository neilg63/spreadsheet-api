use crate::error::{ApiError, ApiResult};

/// A reference to a principal, `<issuer>/<external id>`, e.g. `github/583231` or
/// `service/acme/42`: a guest (`guest/<hash>`), a social login, a tenant forwarded in
/// service mode, or a user of an application that signs its own access tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// The principal's id in the database: the whole reference.
    pub fn id(&self) -> String {
        format!("{}/{}", self.issuer, self.external_id)
    }

    pub fn owns(&self, owner: &str) -> bool {
        owner.strip_prefix(self.issuer.as_str()).and_then(|rest| rest.strip_prefix('/')) == Some(self.external_id.as_str())
    }
}

/// The issuer part of a principal id.
pub fn issuer_of(principal_id: &str) -> &str {
    principal_id.split_once('/').map_or(principal_id, |(issuer, _)| issuer)
}
