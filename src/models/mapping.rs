use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sheet::mapping::{FieldMapping, MappingDoc};

/// What `mappings` stores beside a schema version: a flat summary of the mapping
/// document's columns. The whole document is in the version's definition, under
/// `x_mapping`.
#[derive(Debug, Clone, PartialEq)]
pub struct Mapping {
    pub name: String,
    pub sheet: Option<String>,
    pub header_row: i64,
    pub columns: Vec<MappingColumn>,
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
    pub fn content(doc: &MappingDoc, sheet: Option<&str>, header_row: Option<usize>) -> Self {
        Mapping {
            name: "default".to_string(),
            sheet: sheet.map(str::to_string),
            header_row: header_row.unwrap_or(1) as i64,
            columns: doc.fields.iter().map(MappingColumn::from_field).collect(),
        }
    }
}
