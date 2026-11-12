use serde::{Deserialize, Serialize};

use super::{Datetime, RecordLink};

/// `usage_events`: append-only metering. On a plan that charges for `metric`, each
/// event debits `credit_ledger`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageEvent {
    pub id: RecordLink,
    pub principal: RecordLink,
    pub metric: Metric,
    pub qty: i64,
    /// What was used: a dataset, endpoint ...
    #[serde(rename = "ref")]
    pub reference: Option<RecordLink>,
    /// For anonymous rate limits.
    pub ip: Option<String>,
    pub at: Datetime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    Import,
    RowsImported,
    ApiRequest,
    StorageMbDay,
}
