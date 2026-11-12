use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::RecordLink;

/// `dataset_rows`: one imported row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatasetRow {
    pub id: RecordLink,
    pub dataset: RecordLink,
    /// Position in the import, unique per dataset.
    pub row_num: i64,
    /// The primary-key value(s) joined with `|`, when the collection has a primary key.
    pub pk: Option<String>,
    /// The mapped JSON row that queries return.
    pub data: Value,
    pub source: Value,
    /// Invalid rows (e.g. a required value missing) are hidden from queries by default.
    pub valid: bool,
    pub errors: Vec<RowError>,
    /// Text of the `searchable` fields, indexed for `?q=`.
    pub search_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowError {
    pub path: String,
    pub message: String,
}
