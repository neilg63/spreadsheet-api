use serde::{Deserialize, Serialize};

use super::{Datetime, RecordLink};

/// `api_schemas`: the named, versioned description of a collection's rows. Its content
/// lives in `api_schema_versions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiSchema {
    pub id: RecordLink,
    pub owner: RecordLink,
    /// Unique per owner; defaults to the collection name.
    pub name: String,
    pub description: Option<String>,
    /// Inferred from a sheet rather than supplied as JSON Schema.
    pub inferred: bool,
    pub created_at: Datetime,
    pub updated_at: Datetime,
}
