use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    Datetime, RecordLink,
    api_schema_version::{FieldSpec, VersionStatus, mapping_from_definition},
};
use crate::{
    config::config,
    error::{ApiError, ApiResult},
    sheet::mapping::MappingDoc,
};

/// `datasets`: one import of one worksheet into a collection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dataset {
    pub id: RecordLink,
    pub owner: RecordLink,
    pub collection: RecordLink,
    pub upload: Option<RecordLink>,
    /// Worksheet name; `None` for CSV-like files.
    pub sheet: Option<String>,
    pub schema_version: RecordLink,
    pub mapping: Option<RecordLink>,
    /// Import number within the collection: 1, 2, 3 ...
    pub series: i64,
    pub status: DatasetStatus,
    pub error: Option<String>,
    pub num_rows: i64,
    pub num_invalid: i64,
    /// For anonymous rate limits.
    pub client_ip: Option<String>,
    /// Mapping must finish by then (plan's `mapping_window`).
    pub mapping_deadline: Option<Datetime>,
    pub imported_at: Option<Datetime>,
    pub expires_at: Option<Datetime>,
    pub created_at: Datetime,
    pub updated_at: Datetime,
}

/// `mapping` → `importing` → `imported` → `superseded`, or `failed` / `expired`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetStatus {
    /// Uploaded; the mapping can still change.
    Mapping,
    Importing,
    Imported,
    /// A later import replaced it as the collection's `current`.
    Superseded,
    Failed,
    /// The mapping window closed before import.
    Expired,
}

/// Everything the dataset endpoints need about one dataset, in one read: the dataset with
/// its upload, collection and schema version flattened in.
#[derive(Debug, Clone, Deserialize)]
pub struct DatasetInfo {
    pub id: String,
    pub owner: Vec<String>,
    pub status: DatasetStatus,
    pub sheet: Option<String>,
    pub series: i64,
    pub error: Option<String>,
    pub num_rows: i64,
    pub num_invalid: i64,
    pub imported_at: Option<String>,
    pub created_at: String,
    pub mapping_deadline: Option<String>,
    pub expires_at: Option<String>,
    pub upload_id: Option<String>,
    pub storage_key: Option<String>,
    pub purged_at: Option<String>,
    pub sheets: Option<Vec<String>>,
    pub filename: Option<String>,
    pub collection_id: String,
    pub collection_name: String,
    pub primary_key: Option<Vec<String>>,
    pub current_id: Option<String>,
    pub version_id: String,
    pub version: i64,
    pub version_status: VersionStatus,
    pub definition: Value,
    pub field_spec: Option<Vec<FieldSpec>>,
    pub schema_id: String,
}

impl DatasetInfo {
    pub fn sheet_index(&self) -> u32 {
        match (&self.sheet, &self.sheets) {
            (Some(sheet), Some(sheets)) => sheets.iter().position(|s| s == sheet).unwrap_or(0) as u32,
            _ => 0,
        }
    }

    pub fn doc(&self) -> MappingDoc {
        mapping_from_definition(&self.definition)
    }

    pub fn field_spec(&self) -> &[FieldSpec] {
        self.field_spec.as_deref().unwrap_or_default()
    }

    /// The uploaded source file, while it still exists.
    pub fn source_path(&self) -> Option<PathBuf> {
        if self.purged_at.is_some() {
            return None;
        }
        let path = config().upload_dir.join(self.storage_key.as_deref()?);
        path.is_file().then_some(path)
    }

    pub fn require_source(&self) -> ApiResult<PathBuf> {
        self.source_path().ok_or_else(|| {
            ApiError::Conflict("the source file for this dataset is no longer available; upload the spreadsheet again".into())
        })
    }

    pub fn summary(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.collection_name,
            "sheet": self.sheet,
            "status": self.status,
            "collection": { "id": self.collection_id, "name": self.collection_name, "current": self.current_id },
            "series": self.series,
            "schema_version": { "id": self.version_id, "version": self.version, "status": self.version_status },
            "primary_key": self.primary_key.clone().unwrap_or_default(),
            "num_rows": self.num_rows,
            "num_invalid": self.num_invalid,
            "error": self.error,
            "filename": self.filename,
            "source_available": self.source_path().is_some(),
            "created_at": self.created_at,
            "imported_at": self.imported_at,
            "mapping_deadline": self.mapping_deadline,
            "expires_at": self.expires_at,
        })
    }
}
