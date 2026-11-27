pub mod datasets;

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderName, HeaderValue, Method, header},
    middleware,
    routing::{get, patch, post},
};
use serde_json::{Value, json};
use tower_http::{
    cors::{AllowOrigin, Any, CorsLayer},
    trace::TraceLayer,
};

use crate::{
    auth::{self, AccountKind, AuthMode, AuthSettings, Caller, oauth, rate_limit::RateLimiter},
    config::config,
    db::Database,
    error::ApiError,
};

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub auth: Arc<AuthSettings>,
    /// For OAuth token exchanges and profile reads (no redirects, as OAuth requires).
    pub http: reqwest::Client,
    /// For `{"url": ...}` uploads: redirects only to allowed hosts.
    pub fetch: reqwest::Client,
    pub limiter: Arc<RateLimiter>,
}

impl AppState {
    pub fn new(db: Database, auth: AuthSettings) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("HTTP client");
        let hosts = auth.url_hosts.clone();
        let fetch = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                let allowed = hosts.is_empty()
                    || attempt.url().host_str().is_some_and(|h| hosts.iter().any(|a| a.eq_ignore_ascii_case(h)));
                if attempt.previous().len() >= 5 || !allowed { attempt.stop() } else { attempt.follow() }
            }))
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("HTTP client");
        let limiter = Arc::new(RateLimiter::new(auth.rate_limit_per_minute));
        AppState { db, auth: Arc::new(auth), http, fetch, limiter }
    }

    /// Upload cap for this caller: guests get the smaller demo limit.
    pub fn max_upload_bytes(&self, caller: &Caller) -> usize {
        match caller.kind {
            AccountKind::Guest => self.auth.guest_max_upload_bytes.min(config().max_upload_bytes),
            _ => config().max_upload_bytes,
        }
    }
}

pub fn router(state: AppState) -> Router {
    let mut cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION, HeaderName::from_static("x-user-ref")])
        .expose_headers([header::RETRY_AFTER]);
    cors = if state.auth.cors_origins.is_empty() {
        cors.allow_origin(Any)
    } else {
        // a frontend on another origin sends the session cookie, which needs explicit origins
        let origins: Vec<HeaderValue> = state.auth.cors_origins.iter().filter_map(|o| o.parse().ok()).collect();
        cors.allow_origin(AllowOrigin::list(origins)).allow_credentials(true)
    };
    let mut routes = Router::new()
        .route("/", get(welcome))
        .route("/mapping/options", get(mapping_options))
        .route("/auth/me", get(oauth::me))
        .route("/datasets", get(datasets::list))
        .route("/datasets/upload", post(datasets::upload))
        .route("/datasets/{id}", get(datasets::show))
        .route("/datasets/{id}/export", get(datasets::export))
        .route("/datasets/{id}/preview", get(datasets::preview))
        .route("/datasets/{id}/configure", patch(datasets::configure))
        .route("/datasets/{id}/import", patch(datasets::import))
        .route("/datasets/{id}/access", patch(datasets::access));
    if state.auth.mode == AuthMode::Demo {
        routes = routes
            .route("/auth/{provider}/login", get(oauth::login))
            .route("/auth/{provider}/callback", get(oauth::callback))
            .route("/auth/logout", post(oauth::logout));
    }
    routes
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), auth::rate_limit))
        .layer(DefaultBodyLimit::max(config().max_upload_bytes + 1024 * 1024))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn not_found() -> ApiError {
    ApiError::NotFound("no such route; GET / lists them".into())
}

async fn welcome(State(state): State<AppState>) -> Json<Value> {
    let auth = match state.auth.mode {
        AuthMode::Demo => json!({
            "mode": "demo",
            "accounts": "guests are identified by network and browser; sign in for upload history and custom mappings",
            "login": state.auth.providers.iter().map(|p| format!("/auth/{}/login", p.name)).collect::<Vec<_>>(),
            "guest_max_upload_bytes": state.auth.guest_max_upload_bytes.min(config().max_upload_bytes),
            "rate_limit_per_minute": state.auth.rate_limit_per_minute,
        }),
        AuthMode::Service => json!({
            "mode": "service",
            "user_header": state.auth.user_header.as_str(),
            "service_token": state.auth.service_token.is_some(),
            "intake": ["multipart", "url", if state.auth.shared_dir.is_some() { "path" } else { "path (disabled)" }],
        }),
    };
    let id = json!({ "id": "The dataset's id, from the upload response" });
    let rows = "Rows in the preview that comes back";
    let mut routes = json!({
        "home": {
            "method": "GET",
            "path": "/",
            "access": "anyone",
            "description": "This document: the service's limits, how callers are identified, and every route"
        },
        "upload": {
            "method": "POST",
            "path": "/datasets/upload",
            "type": "multipart/form-data, or application/json in service mode",
            "query_params": { "rows": rows },
            "body_params": {
                "file": "multipart: the spreadsheet (xlsx, xlsm, xlsb, xls, ods, csv, tsv)",
                "source": "Optional JSON on how to read the file: header_row, data_start, sheet options (see /mapping/options)",
                "url": "JSON, service mode: download the file from this address instead",
                "path": "JSON, service mode: take the file from the shared upload directory instead",
                "filename": "JSON, service mode: the name to record and to take the format from"
            },
            "access": "anyone; an access token needs upload",
            "description": "Validate and store a spreadsheet. Returns one dataset per worksheet, each with its columns, suggested pattern rules and a preview"
        },
        "preview": {
            "method": "GET",
            "path": "/datasets/{id}/preview",
            "path_params": id,
            "query_params": { "rows": rows },
            "access": "the owner; an access token needs configure",
            "description": "Read the source file again with the saved mapping"
        },
        "configure": {
            "method": "PATCH",
            "path": "/datasets/{id}/configure",
            "type": "application/json",
            "path_params": id,
            "query_params": { "dry_run": "true to validate and preview without saving", "rows": rows },
            "body_params": {
                "source": "How the sheet is read: header row, first data row, column naming",
                "fields": "Columns to rename, nest, cast, default, require or leave out",
                "patterns": "Rules for columns that share part of their name: sales_{region} to sales.{region}",
                "exclude": "Columns to leave out: names, start..end ranges or patterns",
                "exclude_paths": "Nested output paths to leave out",
                "primary_key": "Fields that identify a row, for upserts",
                "filter": "Rows to keep at import",
                "collection": "The collection the rows are published under",
                "unmapped": "keep (default) or drop the columns nothing mentions",
                "omit_nulls": "true to leave empty values out of each row"
            },
            "access": "the owner; an access token needs configure",
            "description": "Save the mapping document as a schema version and return a fresh preview. GET /mapping/options lists the values each part accepts"
        },
        "import": {
            "method": "PATCH",
            "path": "/datasets/{id}/import",
            "type": "application/json",
            "path_params": id,
            "body_params": { "mode": "replace (default) or upsert, which needs a primary key" },
            "access": "the owner; an access token needs import",
            "description": "Import every row and publish the endpoint. The source file is deleted once all its worksheets are imported"
        },
        "access": {
            "method": "PATCH",
            "path": "/datasets/{id}/access",
            "type": "application/json",
            "path_params": id,
            "body_params": { "visibility": "unlisted (default: anyone with the id), public or private (the owner and access tokens only)" },
            "access": "the owner; an access token needs configure",
            "description": "Set who may read the imported rows"
        },
        "dataset": {
            "method": "GET",
            "path": "/datasets/{id}",
            "path_params": id,
            "query_params": {
                "filter": "JSON, MongoDB style {\"price\": {\"$gt\": 5}} or a rule tree {\"field\", \"op\", \"value\"}",
                "sort": "Fields to sort by: price,-name or price:desc,name:asc",
                "q": "Full-text search over the fields marked searchable",
                "limit": format!("Rows per page: {} by default, {} at most", config().default_page_size, config().max_page_size),
                "start": "Offset of the first row",
                "page": "1-based page, as an alternative to start",
                "include_invalid": "true to include rows that failed validation"
            },
            "access": "anyone with the id unless private; an access token needs read",
            "description": "Query the imported rows"
        },
        "export": {
            "method": "GET",
            "path": "/datasets/{id}/export",
            "path_params": id,
            "query_params": { "format": "json (one array, the default) or jsonl (one row per line)", "include_invalid": "true to include rows that failed validation" },
            "access": "anyone with the id unless private; an access token needs export",
            "description": "Download every imported row"
        },
        "datasets": {
            "method": "GET",
            "path": "/datasets",
            "query_params": { "limit": "Datasets per page", "start": "Offset of the first one", "page": "1-based page, as an alternative to start" },
            "access": "signed-in callers, not guests",
            "description": "The caller's datasets, newest first"
        },
        "me": {
            "method": "GET",
            "path": "/auth/me",
            "access": "anyone",
            "description": "The caller's account type, profile, allowance, grants and sign-in links"
        },
        "mapping_options": {
            "method": "GET",
            "path": "/mapping/options",
            "access": "anyone",
            "description": "The values a mapping document and a filter accept, for building forms"
        }
    });
    if state.auth.mode == AuthMode::Demo {
        let provider = json!({ "provider": state.auth.providers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(" or ") });
        routes["login"] = json!({
            "method": "GET",
            "path": "/auth/{provider}/login",
            "path_params": provider,
            "query_params": { "return_to": "Where to send the browser once signed in" },
            "access": "anyone",
            "description": "Start signing in: redirects to the provider"
        });
        routes["login_callback"] = json!({
            "method": "GET",
            "path": "/auth/{provider}/callback",
            "path_params": provider,
            "access": "the provider's redirect",
            "description": "Finish signing in: sets the session cookie and keeps the guest's uploads"
        });
        routes["logout"] = json!({ "method": "POST", "path": "/auth/logout", "access": "signed-in callers", "description": "End the session" });
    }
    Json(json!({
        "title": "Spreadsheet API",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Upload spreadsheets, map each worksheet to JSON with a versioned schema, and query the imported rows.",
        "max_upload_bytes": config().max_upload_bytes,
        "preview_rows": datasets::preview_rows(None),
        "max_preview_rows": config().max_preview_rows,
        "default_page_size": config().default_page_size,
        "max_page_size": config().max_page_size,
        "auth": auth,
        "access_tokens": "Authorization: Bearer <JWT signed by a registered application>; its grants name the actions (read, export, upload, configure, import) it allows on which datasets",
        "routes": routes
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
        "preview_rows": {
            "description": "How many rows a preview returns: pass ?rows= to upload, configure and preview",
            "default": datasets::preview_rows(None),
            "max": config().max_preview_rows,
            "options": datasets::PREVIEW_SIZES.iter().filter(|n| **n <= config().max_preview_rows).collect::<Vec<_>>(),
        },
        "import_modes": [
            { "value": "replace", "label": "Replace all rows" },
            { "value": "upsert", "label": "Update rows by primary key, keep the rest" }
        ],
        "path_syntax": {
            "nested": "size.width",
            "append_to_list": "tags[]",
            "patterns": [
                { "source": "file_[n]", "target": "files[]", "label": "Numbered columns into a list" },
                { "source": "sales_{region}", "target": "sales.{region}", "label": "Columns that start the same into an object" },
                { "source": "{region}_sales", "target": "sales.{region}", "label": "Columns that end the same into an object" },
                { "source": "sales_[year]", "target": "sales[].{year}{amount}", "label": "Columns into a list of objects" },
                { "source": "{fuel}_price_note_{n}", "target": "price_{fuel}", "label": "Rename columns, keeping part of the name" }
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
