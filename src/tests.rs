//! End-to-end: upload -> configure -> import -> query -> re-import with upsert, through
//! the real router and an in-memory SurrealDB with data/schema.surql applied.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{
    auth::{AuthMode, AuthSettings, Provider},
    config::config,
    db, models, routes,
};

async fn database() -> db::Database {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let uploads = std::env::temp_dir().join(format!("sheet-api-test-{}", std::process::id()));
        // SAFETY: set once, before any test reads the configuration
        unsafe {
            std::env::set_var("DB_PATH", "memory");
            std::env::set_var("UPLOAD_DIR", &uploads);
        }
    });
    db::connect(config()).await.expect("database")
}

/// Service mode, acting as `service/local` when no X-User-Ref is sent.
fn service_settings() -> AuthSettings {
    let mut auth = AuthSettings::from_env();
    auth.mode = AuthMode::Service;
    auth.default_ref = Some("local".into());
    auth.service_token = None;
    auth
}

/// Demo mode, with guests told apart by X-Real-IP and no rate limit.
fn demo_settings() -> AuthSettings {
    let mut auth = AuthSettings::from_env();
    auth.mode = AuthMode::Demo;
    auth.client_ip_header = Some(axum::http::HeaderName::from_static("x-real-ip"));
    auth.guest_salt = "test-salt".into();
    auth.cookie_secure = false;
    auth.rate_limit_per_minute = 0;
    auth.guest_max_upload_bytes = 2 * 1024 * 1024;
    auth.providers = vec![];
    auth
}

async fn app_with(auth: AuthSettings) -> Router {
    routes::router(routes::AppState::new(database().await, auth))
}

async fn app() -> Router {
    app_with(service_settings()).await
}

async fn call(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn multipart(fixture: &str) -> Vec<u8> {
    let bytes = std::fs::read(format!("tests/fixtures/{fixture}")).unwrap();
    let mut body = format!(
        "--XBOUNDARY\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{fixture}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend(bytes);
    body.extend(b"\r\n--XBOUNDARY--\r\n");
    body
}

/// Uploads a fixture with extra request headers; returns the status and body.
async fn upload_with(app: &Router, fixture: &str, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut request = Request::post("/datasets/upload").header("content-type", "multipart/form-data; boundary=XBOUNDARY");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    call(app, request.body(Body::from(multipart(fixture))).unwrap()).await
}

async fn upload(app: &Router, fixture: &str) -> Value {
    let (status, json) = upload_with(app, fixture, &[]).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    json
}

/// Any request with headers and an optional JSON body.
async fn send(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)], body: Option<Value>) -> (StatusCode, Value, axum::http::HeaderMap) {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = match body {
        Some(json) => {
            request = request.header("content-type", "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let response = app.clone().oneshot(request.body(body).unwrap()).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), headers)
}

async fn patch(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::patch(uri).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
    call(app, request).await
}

async fn get(app: &Router, uri: &str) -> Value {
    let (status, json) = call(app, Request::get(uri).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    json
}

fn skus(result: &Value) -> Vec<(String, f64)> {
    result["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["sku"].as_str().unwrap().to_string(), r["price"]["amount"].as_f64().unwrap()))
        .collect()
}

#[tokio::test]
async fn upload_configure_import_query_and_upsert() {
    let app = app().await;
    let uploaded = upload(&app, "catalogue.xlsx").await;
    let products = &uploaded["datasets"][0];
    assert_eq!(products["sheet"], "Products");
    assert_eq!(products["header_row"], 4, "title rows are skipped");
    assert_eq!(uploaded["skipped"][0]["sheet"], "Empty");
    let id = products["dataset_id"].as_str().unwrap();

    let mapping = json!({
        "source": { "header_row": 4 },
        "primary_key": ["sku"],
        "fields": [
            { "source": "SKU", "required": true },
            { "source": "name", "searchable": true },
            { "source": "Unit Price", "path": "price.amount", "type": "number", "precision": 2 },
            { "source": "D", "path": "in_stock", "type": "boolean" }
        ],
        "patterns": [{ "source": "sales_[year]", "target": "sales[].{year}{units}", "type": "integer" }],
        "exclude": ["colour"]
    });
    let (status, bad) = patch(&app, &format!("/datasets/{id}/configure"), json!({ "fields": [{ "source": "nope" }] })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad["errors"][0]["path"], "fields[0].source");

    let (status, configured) = patch(&app, &format!("/datasets/{id}/configure"), mapping.clone()).await;
    assert_eq!(status, StatusCode::OK, "{configured}");
    assert_eq!(
        configured["preview"]["rows"][0],
        json!({ "sku": "A-1", "name": "Red widget", "price": { "amount": 2.5 }, "in_stock": true,
                "sales": [{ "year": 2024, "units": 10 }, { "year": 2025, "units": 12 }] })
    );

    let (status, imported) = patch(&app, &format!("/datasets/{id}/import"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{imported}");
    assert_eq!((imported["mode"].as_str(), imported["num_rows"].as_i64()), (Some("replace"), Some(4)));

    let filter = urlencode(r#"{"price.amount":{"$gte":3}}"#);
    let result = get(&app, &format!("/datasets/{id}?filter={filter}&sort=-price.amount")).await;
    assert_eq!(skus(&result), vec![("B-8".into(), 19.99), ("B-7".into(), 12.0), ("A-2".into(), 3.75)]);
    assert_eq!(get(&app, &format!("/datasets/{id}?q=gadget")).await["total"], 2);

    // next month's file, retargeted at the same collection: upsert by sku
    let october = upload(&app, "catalogue-october.xlsx").await;
    let october_id = october["datasets"][0]["dataset_id"].as_str().unwrap().to_string();
    let mut retarget = mapping.clone();
    retarget["collection"] = json!("catalogue / Products");
    let (status, _) = patch(&app, &format!("/datasets/{october_id}/configure"), retarget).await;
    assert_eq!(status, StatusCode::OK);
    let (status, merged) = patch(&app, &format!("/datasets/{october_id}/import"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    assert_eq!(merged["mode"], "upsert");
    assert_eq!((merged["rows_retained"].as_i64(), merged["num_invalid"].as_i64()), (Some(3), Some(1)));

    let current = get(&app, &format!("/datasets/{october_id}?sort=sku")).await;
    assert_eq!(
        skus(&current),
        vec![("A-1".into(), 2.5), ("A-2".into(), 4.0), ("B-7".into(), 12.0), ("B-8".into(), 19.99), ("B-9".into(), 1.25)]
    );
    let previous = get(&app, &format!("/datasets/{id}")).await;
    assert_eq!(previous["dataset"]["status"], "superseded");
    assert_eq!(previous["dataset"]["collection"]["current"], json!(october_id));
}

fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn strip_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            map.values_mut().for_each(strip_nulls);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

/// Reads every record of `table` into `T` and back: a field the model lacks, or reads
/// differently, fails the round trip.
async fn check_model<T: DeserializeOwned + Serialize>(db: &db::Database, table: &str) {
    let rows = db::run(db, &format!("SELECT * FROM {table};"), json!({})).await.unwrap().pop().unwrap();
    let rows = rows.as_array().unwrap();
    assert!(!rows.is_empty(), "{table} has no records to check");
    for row in rows {
        let model: T = serde_json::from_value(row.clone()).unwrap_or_else(|e| panic!("{table}: {e}\n{row}"));
        let (mut back, mut row) = (serde_json::to_value(&model).unwrap(), row.clone());
        strip_nulls(&mut back);
        strip_nulls(&mut row);
        assert_eq!(back, row, "{table} does not round-trip through its model");
    }
}

#[tokio::test]
async fn models_match_the_schema() {
    let db = database().await;
    let app = routes::router(routes::AppState::new(db.clone(), service_settings()));
    let uploaded = upload(&app, "catalogue.xlsx").await;
    let id = uploaded["datasets"][0]["dataset_id"].as_str().unwrap();
    let mapping = json!({
        "source": { "header_row": 4 },
        "primary_key": ["sku"],
        "fields": [{ "source": "SKU", "required": true }, { "source": "name", "searchable": true }, { "source": "D", "default": 0 }]
    });
    let (status, json) = patch(&app, &format!("/datasets/{id}/configure"), mapping).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let (status, json) = patch(&app, &format!("/datasets/{id}/import"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{json}");

    // tables the routes don't write yet
    let sql = "
        LET $me = principals:['service', 'local'];
        CREATE social_users CONTENT { principal: $me, provider: 'google', email: 'someone@example.com' };
        CREATE usage_events CONTENT { principal: $me, metric: 'api_request', qty: 2000, ip: '127.0.0.1' };
        CREATE credit_ledger CONTENT { principal: $me, delta: 100, reason: 'purchase', note: 'top-up' };
        LET $child = (SELECT VALUE id FROM collections LIMIT 1)[0];
        LET $parent = (CREATE ONLY collections CONTENT { owner: $me, name: 'parent', primary_key: ['sku'] }).id;
        LET $link = (RELATE ONLY $child->links_to->$parent SET name = 'parent', from_path = ['sku']).id;
        CREATE endpoints CONTENT { owner: $me, collection: $child, include_links: [$link], config: { x: 'lng' } };
        CREATE auth_sessions CONTENT { principal: $me, token_hash: 'abc', ip: '127.0.0.1', expires_at: time::now() + 1d };
        CREATE auth_states:xyz CONTENT { provider: 'github', pkce_verifier: 'v', return_to: '/', guest: $me };";
    db::run(&db, sql, json!({})).await.unwrap();

    check_model::<models::Plan>(&db, "plans").await;
    check_model::<models::Issuer>(&db, "issuers").await;
    check_model::<models::Principal>(&db, "principals").await;
    check_model::<models::SocialUser>(&db, "social_users").await;
    check_model::<models::UsageEvent>(&db, "usage_events").await;
    check_model::<models::CreditEntry>(&db, "credit_ledger").await;
    check_model::<models::Upload>(&db, "uploads").await;
    check_model::<models::ApiSchema>(&db, "api_schemas").await;
    check_model::<models::ApiSchemaVersion>(&db, "api_schema_versions").await;
    check_model::<models::Mapping>(&db, "mappings").await;
    check_model::<models::Collection>(&db, "collections").await;
    check_model::<models::Link>(&db, "links_to").await;
    check_model::<models::Dataset>(&db, "datasets").await;
    check_model::<models::DatasetRow>(&db, "dataset_rows").await;
    check_model::<models::Endpoint>(&db, "endpoints").await;
    check_model::<models::AuthSession>(&db, "auth_sessions").await;
    check_model::<models::AuthState>(&db, "auth_states").await;
}

fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| c.strip_prefix(&format!("{name}="))?.split(';').next().map(str::to_string))
}

/// Serves `router` on a random local port; returns its base URL.
async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

#[tokio::test]
async fn service_mode_scopes_tenants_and_accepts_url_and_path_uploads() {
    let shared = std::env::temp_dir().join(format!("sheet-api-shared-{}", std::process::id()));
    std::fs::create_dir_all(shared.join("inbox")).unwrap();
    std::fs::copy("tests/fixtures/prices.ssv", shared.join("inbox/prices.ssv")).unwrap();
    std::fs::write(shared.with_extension("csv"), "secret,data\n1,2\n").unwrap();
    let files = serve(Router::new().route(
        "/files/prices.ssv",
        axum::routing::get(|| async { std::fs::read("tests/fixtures/prices.ssv").unwrap() }),
    ))
    .await;

    let mut auth = service_settings();
    auth.default_ref = None;
    auth.service_token = Some("s3cret".into());
    auth.shared_dir = Some(shared.clone());
    let app = app_with(auth.clone()).await;
    let acme = [("authorization", "Bearer s3cret"), ("x-user-ref", "acme/1")];

    assert_eq!(upload_with(&app, "prices.ssv", &[("x-user-ref", "acme/1")]).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(upload_with(&app, "prices.ssv", &[("authorization", "Bearer s3cret")]).await.0, StatusCode::UNAUTHORIZED);
    let (status, uploaded) = upload_with(&app, "prices.ssv", &acme).await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    assert_eq!(uploaded["allowance"]["plan"], "Service");
    let id = uploaded["datasets"][0]["dataset_id"].as_str().unwrap();

    // another tenant can't see or change it
    let other = [("x-service-token", "s3cret"), ("x-user-ref", "globex/7")];
    let (status, _, _) = send(&app, "PATCH", &format!("/datasets/{id}/configure"), &other, Some(json!({}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(send(&app, "GET", "/datasets", &other, None).await.1["total"], 0);
    assert_eq!(send(&app, "GET", "/datasets", &acme, None).await.1["total"], 1);

    let (status, copied, _) = send(&app, "POST", "/datasets/upload", &acme, Some(json!({ "path": "inbox/prices.ssv" }))).await;
    assert_eq!(status, StatusCode::OK, "{copied}");
    assert_eq!(copied["upload"]["source_kind"], "transfer");
    assert!(shared.join("inbox/prices.ssv").exists(), "the shared original is left in place");
    let outside = shared.with_extension("csv");
    for escape in ["../".to_string() + outside.file_name().unwrap().to_str().unwrap(), outside.to_string_lossy().to_string()] {
        let (status, _, _) = send(&app, "POST", "/datasets/upload", &acme, Some(json!({ "path": escape }))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{escape}");
    }

    let url = format!("{files}/files/prices.ssv");
    let (status, fetched, _) = send(&app, "POST", "/datasets/upload", &acme, Some(json!({ "url": url, "source": { "decimal_comma": true } }))).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["upload"]["source_kind"], "url");
    assert_eq!(fetched["datasets"][0]["rows"][0]["price"], 1.5);
    let (status, _, _) = send(&app, "POST", "/datasets/upload", &acme, Some(json!({ "url": "file:///etc/passwd" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let mut restricted = auth;
    restricted.url_hosts = vec!["example.org".into()];
    let app = app_with(restricted).await;
    let (status, _, _) = send(&app, "POST", "/datasets/upload", &acme, Some(json!({ "url": url }))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn demo_guests_convert_and_preview_but_cannot_customise_or_list() {
    let app = app_with(demo_settings()).await;
    let guest = [("x-real-ip", "10.0.0.1"), ("user-agent", "TestBrowser/1")];
    // identity headers mean nothing in demo mode
    let (_, me, _) = send(&app, "GET", "/auth/me", &[guest[0], guest[1], ("x-user-ref", "service/local")], None).await;
    assert_eq!((me["account"].as_str(), me["allowance"]["plan"].as_str()), (Some("guest"), Some("Guest")));

    let (status, uploaded) = upload_with(&app, "catalogue.xlsx", &guest).await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    let id = uploaded["datasets"][0]["dataset_id"].as_str().unwrap().to_string();

    assert_eq!(send(&app, "GET", "/datasets", &guest, None).await.0, StatusCode::FORBIDDEN);
    let custom = json!({ "source": { "header_row": 4 }, "fields": [{ "source": "sku" }], "primary_key": ["sku"] });
    let (status, locked, _) = send(&app, "PATCH", &format!("/datasets/{id}/configure"), &guest, Some(custom)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let paths: Vec<&str> = locked["errors"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec!["primary_key", "fields"]);
    let (status, _, _) = send(&app, "PATCH", &format!("/datasets/{id}/configure"), &guest, Some(json!({ "source": { "header_row": 4 } }))).await;
    assert_eq!(status, StatusCode::OK);

    // someone on another network can't touch it, but the id works as a public link
    let stranger = [("x-real-ip", "10.0.0.2"), ("user-agent", "TestBrowser/1")];
    assert_eq!(send(&app, "PATCH", &format!("/datasets/{id}/import"), &stranger, None).await.0, StatusCode::NOT_FOUND);
    let (status, imported, _) = send(&app, "PATCH", &format!("/datasets/{id}/import"), &guest, None).await;
    assert_eq!(status, StatusCode::OK, "{imported}");
    assert_eq!(get(&app, &format!("/datasets/{id}")).await["total"], 4);

    let response = app.clone().oneshot(Request::get(format!("/datasets/{id}/export?format=jsonl")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.headers()["content-type"], "application/x-ndjson");
    assert!(response.headers()["content-disposition"].to_str().unwrap().contains("catalogue___Products.jsonl"));
    let text = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert_eq!(text.lines().count(), 4);
    let (_, all, _) = send(&app, "GET", &format!("/datasets/{id}/export"), &[], None).await;
    assert_eq!(all.as_array().map(Vec::len), Some(4));
    assert_eq!(all[0]["sku"], "A-1");

    let (status, too_big, _) = send(&app, "POST", "/datasets/upload", &guest, Some(json!({ "url": "http://x/a.csv" }))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{too_big}");
    let mut small = demo_settings();
    small.guest_max_upload_bytes = 100;
    let (status, rejected) = upload_with(&app_with(small).await, "catalogue.xlsx", &guest).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");
}

#[tokio::test]
async fn demo_rate_limits_each_client_address() {
    let mut auth = demo_settings();
    auth.rate_limit_per_minute = 3;
    let app = app_with(auth).await;
    for _ in 0..3 {
        assert_eq!(send(&app, "GET", "/", &[("x-real-ip", "10.1.1.1")], None).await.0, StatusCode::OK);
    }
    let (status, body, headers) = send(&app, "GET", "/", &[("x-real-ip", "10.1.1.1")], None).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited")));
    assert!(headers["retry-after"].to_str().unwrap().parse::<u64>().unwrap() >= 1);
    assert_eq!(send(&app, "GET", "/", &[("x-real-ip", "10.1.1.2")], None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn demo_login_with_a_provider_keeps_the_guest_uploads() {
    use axum::{Form, extract::Query as Q, http::HeaderMap, routing::{get as rget, post as rpost}};
    use std::collections::HashMap;
    let provider = serve(
        Router::new()
            .route(
                "/token",
                rpost(|Form(form): Form<HashMap<String, String>>| async move {
                    let ok = form.get("code").map(String::as_str) == Some("good-code") && form.contains_key("code_verifier");
                    if ok {
                        (StatusCode::OK, axum::Json(json!({ "access_token": "tok123", "token_type": "bearer" })))
                    } else {
                        (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": "invalid_grant" })))
                    }
                }),
            )
            .route(
                "/user",
                rget(|headers: HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer tok123");
                    axum::Json(json!({ "id": 42, "login": "octo", "name": null, "email": null, "avatar_url": "https://img/42" }))
                }),
            )
            .route("/emails", rget(|| async { axum::Json(json!([{ "email": "octo@example.com", "primary": true, "verified": true }])) }))
            .route("/authorize", rget(|Q(_): Q<HashMap<String, String>>| async { "consent screen" })),
    )
    .await;
    let mut auth = demo_settings();
    auth.public_url = "http://api.test".into();
    auth.frontend_url = Some("http://app.test".into());
    auth.providers = vec![Provider {
        name: "github".into(),
        client_id: "client".into(),
        client_secret: "secret".into(),
        auth_url: format!("{provider}/authorize"),
        token_url: format!("{provider}/token"),
        profile_url: format!("{provider}/user"),
        emails_url: Some(format!("{provider}/emails")),
        scopes: vec!["read:user".into()],
    }];
    let app = app_with(auth).await;
    let browser = [("x-real-ip", "10.0.0.9"), ("user-agent", "TestBrowser/2")];
    let uploaded = upload_with(&app, "catalogue.xlsx", &browser).await.1;
    let guest_dataset = uploaded["datasets"][0]["dataset_id"].as_str().unwrap().to_string();
    let (_, me, _) = send(&app, "GET", "/auth/me", &browser, None).await;
    assert_eq!(me["login"][0]["url"], "http://api.test/auth/github/login");

    let (status, _, headers) = send(&app, "GET", "/auth/github/login?return_to=/datasets", &browser, None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = headers["location"].to_str().unwrap();
    assert!(location.starts_with(&format!("{provider}/authorize?")), "{location}");
    assert!(location.contains("code_challenge=") && location.contains("redirect_uri=http%3A%2F%2Fapi.test%2Fauth%2Fgithub%2Fcallback"));
    let state = cookie_value(&headers, "sheet_login").unwrap();
    assert!(location.contains(&format!("state={state}")));

    let callback = format!("/auth/github/callback?code=good-code&state={state}");
    // a callback link opened in a browser that didn't start the login is refused
    assert_eq!(send(&app, "GET", &callback, &browser, None).await.0, StatusCode::BAD_REQUEST);
    let login_cookie = format!("sheet_login={state}");
    let with_login = [browser[0], browser[1], ("cookie", login_cookie.as_str())];
    let (status, body, headers) = send(&app, "GET", &callback, &with_login, None).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(headers["location"], "http://app.test/datasets");
    let session = cookie_value(&headers, "sheet_session").unwrap();
    assert_eq!(cookie_value(&headers, "sheet_login").as_deref(), Some(""), "the login cookie is cleared");
    assert_eq!(send(&app, "GET", &callback, &with_login, None).await.0, StatusCode::BAD_REQUEST, "states are single use");

    let session_cookie = format!("sheet_session={session}");
    let signed_in = [browser[0], browser[1], ("cookie", session_cookie.as_str())];
    let (_, me, _) = send(&app, "GET", "/auth/me", &signed_in, None).await;
    assert_eq!((me["account"].as_str(), me["user_ref"].as_str()), (Some("registered"), Some("github/42")));
    assert_eq!((me["profile"]["email"].as_str(), me["allowance"]["plan"].as_str()), (Some("octo@example.com"), Some("Registered")));

    // the guest's upload came along, and can now be customised
    let (_, history, _) = send(&app, "GET", "/datasets", &signed_in, None).await;
    let ids: Vec<&str> = history["datasets"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&guest_dataset.as_str()), "{history}");
    let custom = json!({ "source": { "header_row": 4 }, "fields": [{ "source": "sku", "required": true }], "primary_key": ["sku"] });
    let (status, body, _) = send(&app, "PATCH", &format!("/datasets/{guest_dataset}/configure"), &signed_in, Some(custom)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, _, headers) = send(&app, "POST", "/auth/logout", &signed_in, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(cookie_value(&headers, "sheet_session").as_deref(), Some(""));
    assert_eq!(send(&app, "GET", "/auth/me", &signed_in, None).await.1["account"], "guest");
}



