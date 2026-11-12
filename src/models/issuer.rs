use serde::{Deserialize, Serialize};

use super::{Duration, RecordLink};

/// `issuers`: the part before the slash in a user reference, e.g. `issuers:google`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issuer {
    pub id: RecordLink,
    pub name: String,
    pub trust: Trust,
    /// `plans` record given to principals without a paid plan.
    pub default_plan: RecordLink,
    /// Inactive principals are removed after this.
    pub principal_ttl: Option<Duration>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    Anonymous,
    Social,
    Managed,
    System,
}
