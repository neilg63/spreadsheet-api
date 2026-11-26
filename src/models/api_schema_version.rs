use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sheet::mapping::MappingDoc;
pub use crate::sheet::spec::FieldSpec;

/// A version stays a draft until its first import publishes it; published versions are
/// not changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionStatus {
    Draft,
    Published,
    Deprecated,
}

impl VersionStatus {
    pub fn parse(text: &str) -> Self {
        match text {
            "published" => Self::Published,
            "deprecated" => Self::Deprecated,
            _ => Self::Draft,
        }
    }
}

/// The parts of a version the upload and configure flows need.
#[derive(Debug, Clone)]
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

/// The mapping document saved in a definition's `x_mapping`. Versions saved before the
/// keys were snake-cased hold it under `x-mapping`, which is still read.
pub fn mapping_from_definition(definition: &Value) -> MappingDoc {
    definition
        .get("x_mapping")
        .or_else(|| definition.get("x-mapping"))
        .and_then(|m| serde_json::from_value(m.clone()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::mapping_from_definition;
    use serde_json::json;

    #[test]
    fn reads_the_mapping_under_its_current_and_its_earlier_key() {
        let current = mapping_from_definition(&json!({ "x_mapping": { "primary_key": ["sku"] } }));
        assert_eq!(current.primary_key, vec!["sku"]);
        let earlier = mapping_from_definition(&json!({ "x-mapping": { "primary_key": ["code"] } }));
        assert_eq!(earlier.primary_key, vec!["code"]);
        assert!(mapping_from_definition(&json!({})).primary_key.is_empty());
    }
}
