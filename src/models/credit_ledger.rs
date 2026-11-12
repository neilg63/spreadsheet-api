use serde::{Deserialize, Serialize};

use super::{Datetime, RecordLink};

/// One `credit_ledger` entry. The ledger is append-only and `principals.credits` is its
/// running sum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreditEntry {
    pub id: RecordLink,
    pub principal: RecordLink,
    /// Positive for grants and purchases, negative for usage.
    pub delta: i64,
    pub reason: CreditReason,
    /// The usage metric, for `usage` entries.
    pub metric: Option<String>,
    /// The usage event or other record behind the entry.
    #[serde(rename = "ref")]
    pub reference: Option<RecordLink>,
    pub note: Option<String>,
    pub at: Datetime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreditReason {
    MonthlyGrant,
    Purchase,
    Usage,
    Refund,
    Adjustment,
}
