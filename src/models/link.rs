use serde::{Deserialize, Serialize};

use super::{Datetime, RecordLink};

/// `links_to`: a foreign key from one collection (`in`) to another's primary key (`out`).
/// Both collections must have the same owner, and links need the `links` feature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub id: RecordLink,
    /// The child collection.
    #[serde(rename = "in")]
    pub from: RecordLink,
    /// The parent collection, which must have a primary key.
    #[serde(rename = "out")]
    pub to: RecordLink,
    /// The key under which linked data appears in output; unique per child.
    pub name: String,
    /// FK paths in the child, in the same order as the parent's primary key.
    pub from_path: Vec<String>,
    pub cardinality: Cardinality,
    pub created_at: Datetime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    ManyToOne,
    OneToOne,
}
