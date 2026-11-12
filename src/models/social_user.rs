use serde::{Deserialize, Serialize};

use super::{Datetime, RecordLink};

/// `social_users`: the limited profile kept for social logins, one per principal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SocialUser {
    pub id: RecordLink,
    pub principal: RecordLink,
    /// `google`, `microsoft`, `github` ...
    pub provider: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub avatar_uri: Option<String>,
    pub locale: Option<String>,
    pub last_login_at: Datetime,
}
