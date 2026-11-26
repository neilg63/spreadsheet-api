//! The records the service reads and writes, as it uses them. data/schema.sql is the
//! full description of every table; these are the shapes the routes work with, such as
//! [`DatasetInfo`], which flattens a dataset with its upload, collection and schema
//! version.
//!
//! ```text
//!   issuers ─< principals ─┬─ social_users, auth_sessions
//!                          ├─< usage_events
//!                          ├─< uploads
//!                          ├─< api_schemas ─< api_schema_versions ─< mappings
//!                          └─< collections ─< datasets ─< dataset_rows
//!                                   (current_id = live dataset)
//!   plans: referenced by issuers (default plan) and principals (plan override)
//! ```
//!
//! Ids are plain text, times are RFC 3339 UTC text and durations are whole seconds.

pub mod api_schema_version;
pub mod collection;
pub mod dataset;
pub mod mapping;
pub mod plan;
pub mod principal;

pub use api_schema_version::{FieldSpec, VersionInfo, VersionStatus};
pub use collection::{CollectionInfo, Visibility};
pub use dataset::{DatasetInfo, DatasetStatus};
pub use mapping::Mapping;
pub use plan::{Allowance, Plan};
pub use principal::Me;
