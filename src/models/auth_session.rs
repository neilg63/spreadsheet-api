use serde::{Deserialize, Serialize};

use super::{Datetime, RecordLink};

/// `auth_sessions`: a browser session after a social login (demo mode). The cookie holds
/// a random token; only its SHA-256 is stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthSession {
    pub id: RecordLink,
    pub principal: RecordLink,
    pub token_hash: String,
    pub user_agent: Option<String>,
    pub ip: Option<String>,
    pub created_at: Datetime,
    /// Refreshed at most hourly.
    pub last_seen_at: Datetime,
    pub expires_at: Datetime,
}
