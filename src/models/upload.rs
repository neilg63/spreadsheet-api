use serde::{Deserialize, Serialize};

use super::{Datetime, RecordLink};

/// `uploads`: a stored source file. The file is deleted once every worksheet in it is
/// imported, or when the plan's `source_retention` runs out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upload {
    pub id: RecordLink,
    pub owner: RecordLink,
    pub source_kind: SourceKind,
    /// File name or URI.
    pub filename: String,
    /// File name under UPLOAD_DIR.
    pub storage_key: String,
    pub content_type: Option<String>,
    pub size_bytes: i64,
    /// Worksheet names, in workbook order.
    pub sheets: Vec<String>,
    pub purge_at: Option<Datetime>,
    /// Set once the file has been deleted.
    pub purged_at: Option<Datetime>,
    pub created_at: Datetime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    File,
    Url,
    Transfer,
}
