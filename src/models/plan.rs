use serde::{Deserialize, Serialize};

use super::{Duration, RecordLink};

/// `plans`: what a principal may do (quotas, retention, paid features) and what usage costs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: RecordLink,
    pub name: String,
    /// 0 anonymous, 1 basic, 2+ paid.
    pub rank: i64,
    pub limits: PlanLimits,
    pub features: Vec<Feature>,
    pub monthly_credits: i64,
    pub costs: PlanCosts,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanLimits {
    pub imports_per_hour: i64,
    pub max_rows_per_import: i64,
    /// `None` = unlimited.
    pub max_collections: Option<i64>,
    /// `None` = unlimited.
    pub max_endpoints: Option<i64>,
    /// Time to finish mapping after upload; `None` = no deadline.
    pub mapping_window: Option<Duration>,
    /// Uploaded files are purged after this.
    pub source_retention: Duration,
    /// `None` = kept until deleted.
    pub dataset_ttl: Option<Duration>,
    pub endpoint_ttl: Option<Duration>,
    /// Superseded imports kept per collection.
    pub keep_revisions: i64,
}

/// Credits charged per 1,000 units of each usage metric (0 = free).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanCosts {
    pub import: i64,
    pub rows_imported: i64,
    pub api_request: i64,
    pub storage_mb_day: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    Filter,
    Sort,
    Search,
    CustomEndpoints,
    CustomSlug,
    CustomSchema,
    /// Primary keys and `links_to`.
    Links,
    AutoRefresh,
    /// `geojson` and `series` endpoint outputs.
    ChartsMaps,
    PrivateEndpoints,
}

/// What `fn::allowance` returns: the caller's plan and what is left of it this hour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Allowance {
    /// The plan's name.
    pub plan: String,
    pub imports_left_hour: i64,
    pub max_rows: i64,
    pub credits: i64,
    pub features: Vec<Feature>,
}
