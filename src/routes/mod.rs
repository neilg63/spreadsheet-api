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
        .route("/datasets/{id}/import", patch(datasets::import));
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
    Json(json!({
        "title": "Spreadsheet API",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Upload spreadsheets, map each worksheet to JSON with a versioned schema, and query the imported rows.",
        "max_upload_bytes": config().max_upload_bytes,
        "preview_rows": config().preview_rows,
        "auth": auth,
        "routes": [
            { "method": "POST", "path": "/datasets/upload", "body": "multipart: file, source (optional JSON); service mode also JSON {\"url\" | \"path\", \"filename\"?, \"source\"?}", "description": "Validate and store a spreadsheet; one dataset per worksheet with a 30-row preview each" },
            { "method": "GET", "path": "/datasets/{id}/preview", "description": "Re-read the source file with the saved mapping" },
            { "method": "PATCH", "path": "/datasets/{id}/configure", "query": { "dry_run": "true to preview without saving" }, "body": "mapping document (see /mapping/options)", "description": "Save the field mapping as a schema version" },
            { "method": "PATCH", "path": "/datasets/{id}/import", "body": "{\"mode\": \"replace\" | \"upsert\"} (optional)", "description": "Import every row; the source file is deleted once all its worksheets are imported" },
            { "method": "GET", "path": "/datasets/{id}", "query": { "filter": "JSON, MongoDB style or rule tree", "sort": "price,-name", "q": "full-text search", "limit": "page size", "start": "offset", "page": "1-based page", "include_invalid": "true to include rows that failed validation" }, "description": "Query imported rows" },
            { "method": "GET", "path": "/datasets/{id}/export", "query": { "format": "json (default) or jsonl", "include_invalid": "true to include invalid rows" }, "description": "Download every imported row" },
            { "method": "GET", "path": "/datasets", "description": "The caller's datasets (not for guests)" },
            { "method": "GET", "path": "/auth/me", "description": "Account type, profile, allowance and sign-in links" },
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
