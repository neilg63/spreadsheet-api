use serde::{Deserialize, Serialize};

use super::{Datetime, RecordLink};

/// `auth_states`: a login in progress, keyed by its OAuth `state`. Single use, ten minutes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthState {
    pub id: RecordLink,
    /// `github` or `google`
    pub provider: String,
    pub pkce_verifier: String,
    /// Where the browser goes after the callback (already checked against FRONTEND_URL).
    pub return_to: String,
    /// The guest who started the login; their uploads move into the account.
    pub guest: Option<RecordLink>,
    pub created_at: Datetime,
    pub expires_at: Datetime,
}
