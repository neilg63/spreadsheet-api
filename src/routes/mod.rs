pub mod datasets;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequestParts},
    http::{HeaderName, Method, request::Parts},
    routing::{get, patch, post},
};
use serde_json::{Value, json};
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

use crate::{config::config, db::Database, error::ApiError, store::Me};

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
}

/// The calling principal, from the `X-User-Ref` header (`<issuer>/<id>`). Until the
/// external account system is wired in, requests without one act as DEFAULT_USER_REF.
pub struct Caller(pub Me);

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let header = parts.headers.get("x-user-ref").and_then(|v| v.to_str().ok()).filter(|v| !v.trim().is_empty());
        let user_ref = header.map(str::to_string).unwrap_or_else(|| config().default_user_ref.clone());
        Ok(Caller(Me::parse(&user_ref)?))
    }
}

pub fn router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::OPTIONS])
        .allow_headers([axum::http::header::CONTENT_TYPE, HeaderName::from_static("x-user-ref")]);
    Router::new()
        .route("/", get(welcome))
        .route("/mapping/options", get(mapping_options))
        .route("/datasets", get(datasets::list))
        .route("/datasets/upload", post(datasets::upload))
        .route("/datasets/{id}", get(datasets::show))
        .route("/datasets/{id}/preview", get(datasets::preview))
        .route("/datasets/{id}/configure", patch(datasets::configure))
        .route("/datasets/{id}/import", patch(datasets::import))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(config().max_upload_bytes + 1024 * 1024))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn not_found() -> ApiError {
    ApiError::NotFound("no such route; GET / lists them".into())
}

async fn welcome() -> Json<Value> {
    Json(json!({
        "title": "Spreadsheet API",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Upload spreadsheets, map each worksheet to JSON with a versioned schema, and query the imported rows.",
        "max_upload_bytes": config().max_upload_bytes,
        "preview_rows": config().preview_rows,
        "auth": "Send X-User-Ref: <issuer>/<id> to act as a principal",
        "routes": [
            { "method": "POST", "path": "/datasets/upload", "body": "multipart: file, source (optional JSON)", "description": "Validate and store a spreadsheet; one dataset per worksheet with a 30-row preview each" },
            { "method": "GET", "path": "/datasets/{id}/preview", "description": "Re-read the source file with the saved mapping" },
            { "method": "PATCH", "path": "/datasets/{id}/configure", "query": { "dry_run": "true to preview without saving" }, "body": "mapping document (see /mapping/options)", "description": "Save the field mapping as a schema version" },
            { "method": "PATCH", "path": "/datasets/{id}/import", "body": "{\"mode\": \"replace\" | \"upsert\"} (optional)", "description": "Import every row; the source file is deleted once all its worksheets are imported" },
            { "method": "GET", "path": "/datasets/{id}", "query": { "filter": "JSON, MongoDB style or rule tree", "sort": "price,-name", "q": "full-text search", "limit": "page size", "start": "offset", "page": "1-based page", "include_invalid": "true to include rows that failed validation" }, "description": "Query imported rows" },
            { "method": "GET", "path": "/datasets", "description": "The caller's datasets" },
            { "method": "GET", "path": "/mapping/options", "description": "Option values for building mapping and filter widgets" }
        ]
    }))
}

/// Everything a mapping UI needs to populate its controls.
async fn mapping_options() -> Json<Value> {
    Json(json!({
        "field_types": [
            { "value": "auto", "label": "Automatic" },
            { "value": "string", "label": "Text" },
            { "value": "integer", "label": "Whole number" },
            { "value": "number", "label": "Number", "options": ["precision"] },
            { "value": "boolean", "label": "Yes / no" },
            { "value": "date", "label": "Date" },
            { "value": "datetime", "label": "Date and time" },
            { "value": "time", "label": "Time of day" },
            { "value": "array", "label": "List (comma-separated)" }
        ],
        "formats": {
            "description": "Advanced: any spreadsheet-to-json format string overrides the field type",
            "examples": [
                { "value": "d2", "label": "Decimal, 2 places" },
                { "value": "truthy(si,no)", "label": "Custom yes/no words" },
                { "value": "dt:%d/%m/%Y", "label": "Custom date pattern" },
                { "value": "int[](|)", "label": "List of whole numbers split on |" },
                { "value": "text[](;)", "label": "List of text split on ;" }
            ]
        },
        "datetime_styles": [
            { "value": "full", "example": "2026-06-15T10:17:00.000Z" },
            { "value": "simple", "example": "2026-06-15T10:17:00" },
            { "value": "date", "example": "2026-06-15" },
            { "value": "time", "example": "10:17:00" },
            { "value": "hm", "example": "10:17" }
        ],
        "column_names": [
            { "value": "header", "label": "Header text (letters for blank headers)" },
            { "value": "header_numbered", "label": "Header text (c01, c02 for blank headers)" },
            { "value": "a1", "label": "Column letters: a, b, c" },
            { "value": "numbered", "label": "Column numbers: c01, c02" }
        ],
        "unmapped": [
            { "value": "keep", "label": "Keep other columns" },
            { "value": "drop", "label": "Only output mapped columns" }
        ],
        "import_modes": [
            { "value": "replace", "label": "Replace all rows" },
            { "value": "upsert", "label": "Update rows by primary key, keep the rest" }
        ],
        "path_syntax": {
            "nested": "size.width",
            "append_to_list": "tags[]",
            "patterns": [
                { "source": "file_[n]", "target": "files[]", "label": "Numbered columns into a list" },
                { "source": "sales_{region}", "target": "sales.{region}", "label": "Columns into an object keyed by suffix" },
                { "source": "sales_[year]", "target": "sales[].{year}{amount}", "label": "Columns into a list of objects" }
            ]
        },
        "filter_ops": [
            { "value": "eq", "label": "equals" },
            { "value": "ne", "label": "does not equal" },
            { "value": "gt", "label": "greater than" },
            { "value": "gte", "label": "at least" },
            { "value": "lt", "label": "less than" },
            { "value": "lte", "label": "at most" },
            { "value": "between", "label": "between", "value_shape": "[low, high]" },
            { "value": "not_between", "label": "not between", "value_shape": "[low, high]" },
            { "value": "starts_with", "label": "starts with" },
            { "value": "ends_with", "label": "ends with" },
            { "value": "contains", "label": "contains" },
            { "value": "in", "label": "is one of", "value_shape": "list" },
            { "value": "not_in", "label": "is none of", "value_shape": "list" },
            { "value": "any", "label": "list includes any of", "value_shape": "list" },
            { "value": "not_empty", "label": "has a value", "value_shape": "none" },
            { "value": "empty", "label": "is empty", "value_shape": "none" }
        ],
        "filter_tree": { "all": [{ "field": "price", "op": "gte", "value": 10 }, { "any": [{ "field": "name", "op": "starts_with", "value": "wid" }] }] },
        "query_only_ops": ["regex", "all", "has", "size", "exists"]
    }))
}
