use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Datetime, RecordLink};

/// `endpoints`: a published, queryable view of a collection's current dataset, queried
/// with `fn::find`. Not served by the HTTP routes yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub id: RecordLink,
    pub owner: RecordLink,
    pub collection: RecordLink,
    /// Globally unique; random unless the plan has `custom_slug`.
    pub slug: String,
    pub kind: EndpointKind,
    pub status: EndpointStatus,
    pub visibility: Visibility,
    /// Field paths exposed as filters; empty = the schema's `filterable` fields.
    pub filterable: Vec<String>,
    /// Empty = the schema's `sortable` fields.
    pub sortable: Vec<String>,
    /// `links_to` records whose parent rows are joined in.
    pub include_links: Vec<RecordLink>,
    pub output: Output,
    /// Output settings, e.g. lat/lng paths or chart axes.
    pub config: Value,
    /// 1 to 1000.
    pub max_page_size: i64,
    pub expires_at: Option<Datetime>,
    pub created_at: Datetime,
    pub updated_at: Datetime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    /// Expires after the plan's `endpoint_ttl`.
    Temporary,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointStatus {
    Active,
    Expired,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Unlisted,
    Public,
    Private,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    Json,
    Csv,
    Geojson,
    Series,
}
