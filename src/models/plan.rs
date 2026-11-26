use serde::{Deserialize, Serialize};

/// The plan in force for a principal: its own while that lasts, otherwise its issuer's.
#[derive(Debug, Clone)]
pub struct Plan {
    pub name: String,
    pub imports_per_hour: i64,
    pub max_rows_per_import: i64,
    /// `None` = unlimited.
    pub max_collections: Option<i64>,
    /// Time to finish mapping after upload; `None` = no deadline.
    pub mapping_window_secs: Option<i64>,
    /// Uploaded files are purged after this.
    pub source_retention_secs: i64,
    /// `None` = kept until deleted.
    pub dataset_ttl_secs: Option<i64>,
    /// filter, sort, search, custom_schema, links, private_endpoints ...
    pub features: Vec<String>,
}

impl Plan {
    pub fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }
}

/// What a caller is shown: their plan and what is left of it this hour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Allowance {
    /// The plan's name.
    pub plan: String,
    pub imports_left_hour: i64,
    pub max_rows: i64,
    pub credits: i64,
    pub features: Vec<String>,
}
