use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Datetime, RecordLink};
use crate::sheet::mapping::MappingDoc;
pub use crate::sheet::spec::FieldSpec;

/// `api_schema_versions`: one version of an api_schema. A version stays a draft until its
/// first import publishes it; published versions are immutable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiSchemaVersion {
    pub id: RecordLink,
    pub schema: RecordLink,
    /// 1, 2, 3 ... within the schema.
    pub version: i64,
    pub status: VersionStatus,
    /// JSON Schema (draft 2020-12) of one output row, with `x-source`, `x-primary-key`
    /// and `x-mapping` (the complete mapping document) extensions.
    pub definition: Value,
    /// The flat list of output paths with their type and query flags.
    pub field_spec: Vec<FieldSpec>,
    pub created_at: Datetime,
    pub updated_at: Datetime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionStatus {
    Draft,
    Published,
    Deprecated,
}

/// The parts of a version the upload and configure flows need.
#[derive(Debug, Clone, Deserialize)]
pub struct VersionInfo {
    pub id: String,
    pub version: i64,
    pub status: VersionStatus,
    pub definition: Value,
}

impl VersionInfo {
    pub fn doc(&self) -> MappingDoc {
        mapping_from_definition(&self.definition)
    }
}

/// The mapping document saved in a definition's `x-mapping`.
pub fn mapping_from_definition(definition: &Value) -> MappingDoc {
    definition.get("x-mapping").and_then(|m| serde_json::from_value(m.clone()).ok()).unwrap_or_default()
}
