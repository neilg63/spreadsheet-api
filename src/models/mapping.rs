use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Datetime, RecordLink};
use crate::sheet::mapping::{FieldMapping, MappingDoc};

/// `mappings`: the source-column view of a schema version's mapping document, one per
/// version. The full document (`sheet::mapping::MappingDoc`) is kept in the version's
/// `definition["x-mapping"]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mapping {
    pub id: RecordLink,
    pub schema_version: RecordLink,
    /// Unique per version; the service writes `default`.
    pub name: String,
    pub sheet: Option<String>,
    /// 1-based.
    pub header_row: i64,
    pub columns: Vec<MappingColumn>,
    pub created_at: Datetime,
    pub updated_at: Datetime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MappingColumn {
    /// The source column as the mapping names it.
    pub column: String,
    /// The output path.
    pub path: String,
    /// A spreadsheet-to-json format or field type, e.g. `d2`, `integer`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

impl MappingColumn {
    fn from_field(field: &FieldMapping) -> Self {
        MappingColumn {
            column: field.source.clone(),
            path: field.path.clone().unwrap_or_else(|| field.source.clone()),
            transform: field.format.clone().or_else(|| field.field_type.map(|t| format!("{t:?}").to_lowercase())),
            default: field.default.clone(),
        }
    }
}

impl Mapping {
    /// Content for a new mapping row; the caller adds `schema_version`.
    pub fn content(doc: &MappingDoc, sheet: Option<&str>, header_row: Option<usize>) -> Value {
        let columns: Vec<MappingColumn> = doc.fields.iter().map(MappingColumn::from_field).collect();
        let mut row = json!({ "name": "default", "columns": columns, "header_row": header_row.unwrap_or(1) });
        if let Some(sheet) = sheet {
            row["sheet"] = json!(sheet);
        }
        row
    }
}
