use serde::{Deserialize, Serialize};

use super::{Datetime, RecordLink};

/// `collections`: a stable, named data source, `<file> / <sheet>` by default. Each import
/// is a dataset in it, and endpoints and links point at the collection, so a new import
/// replaces the data behind them all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub id: RecordLink,
    pub owner: RecordLink,
    /// Unique per owner.
    pub name: String,
    pub schema: Option<RecordLink>,
    /// The live dataset.
    pub current: Option<RecordLink>,
    /// Output paths that identify a row, e.g. `["sku"]`. Needs the `links` feature.
    pub primary_key: Option<Vec<String>>,
    /// A finished import becomes `current`.
    pub auto_publish: bool,
    pub expires_at: Option<Datetime>,
    pub created_at: Datetime,
    pub updated_at: Datetime,
}

/// The parts of a collection the upload and configure flows need.
#[derive(Debug, Clone, Deserialize)]
pub struct CollectionInfo {
    pub id: String,
    pub name: String,
    pub schema_id: Option<String>,
}
