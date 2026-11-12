//! Typed records for every table in data/schema.surql, one module per table.
//!
//! ```text
//!   issuers ─< principals ─┬─ social_users, auth_sessions
//!                          ├─< usage_events, credit_ledger
//!                          ├─< uploads
//!                          ├─< api_schemas ─< api_schema_versions ─< mappings
//!                          └─< collections ─< datasets ─< dataset_rows
//!                                   │  (current = live dataset)
//!                                   ├── links_to ──> collections
//!                                   └─< endpoints
//!   plans: referenced by issuers (default plan) and principals (plan override)
//!   auth_states: logins in progress (demo mode)
//! ```
//!
//! Each record struct mirrors its table as `SELECT *` returns it through `db::run`:
//! record ids and links are `"table:key"` strings, datetimes are RFC 3339 strings and
//! durations are SurrealQL duration strings such as `"15m"`. Absent `option<…>` fields
//! deserialize as `None`.
//!
//! Next to the records are the projections the service reads in practice, such as
//! [`DatasetInfo`], which flattens a dataset with its upload, collection and schema
//! version. docs/models.md describes the same models for API clients, and
//! `tests::models_match_the_schema` keeps these structs in step with the schema.

// Not every table is read by the service yet; the records document them all.
#![allow(dead_code, unused_imports)]

pub mod api_schema;
pub mod auth_session;
pub mod auth_state;
pub mod api_schema_version;
pub mod collection;
pub mod credit_ledger;
pub mod dataset;
pub mod dataset_row;
pub mod endpoint;
pub mod issuer;
pub mod link;
pub mod mapping;
pub mod plan;
pub mod principal;
pub mod social_user;
pub mod upload;
pub mod usage_event;

pub use api_schema::ApiSchema;
pub use auth_session::AuthSession;
pub use auth_state::AuthState;
pub use api_schema_version::{ApiSchemaVersion, FieldSpec, VersionInfo, VersionStatus};
pub use collection::{Collection, CollectionInfo};
pub use credit_ledger::{CreditEntry, CreditReason};
pub use dataset::{Dataset, DatasetInfo, DatasetStatus};
pub use dataset_row::{DatasetRow, RowError};
pub use endpoint::{Endpoint, EndpointKind, EndpointStatus, Output, Visibility};
pub use issuer::{Issuer, Trust};
pub use link::{Cardinality, Link};
pub use mapping::{Mapping, MappingColumn};
pub use plan::{Allowance, Feature, Plan, PlanCosts, PlanLimits};
pub use principal::{Me, Principal, PrincipalStatus};
pub use social_user::SocialUser;
pub use upload::{SourceKind, Upload};
pub use usage_event::{Metric, UsageEvent};

/// A record id or link, e.g. `"datasets:0199…"` or `"principals:['google', '1098…']"`.
pub type RecordLink = String;
/// An RFC 3339 timestamp, e.g. `"2026-10-02T10:54:27.596Z"`.
pub type Datetime = String;
/// A SurrealQL duration, e.g. `"15m"`, `"1h"`, `"7d"`.
pub type Duration = String;
