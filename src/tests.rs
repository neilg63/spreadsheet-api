//! End-to-end: upload -> configure -> import -> query -> re-import with upsert, through
//! the real router and an in-memory SQLite database with data/schema.sql applied.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{
    auth::{AuthMode, AuthSettings, Provider},
    config::config,
    db, routes,
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

#[tokio::test]
async fn preview_size_follows_the_rows_parameter() {
    let app = app().await;
    // sparse.csv: 60 rows, with `note` empty until row 41
    let (status, body) = call(
        &app,
        Request::post("/datasets/upload?rows=10")
            .header("content-type", "multipart/form-data; boundary=XBOUNDARY")
            .body(Body::from(multipart("sparse.csv")))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sheet = &body["datasets"][0];
    assert_eq!(sheet["rows"].as_array().unwrap().len(), 10);
    assert_eq!(sheet["row_count"], 60);
    let id = sheet["dataset_id"].as_str().unwrap();
    let note_samples = |preview: &Value| preview["columns"][2]["samples"].as_array().unwrap().len();

    // the default, with nothing in `note` yet
    let preview = get(&app, &format!("/datasets/{id}/preview")).await;
    assert_eq!(preview["preview"]["rows"].as_array().unwrap().len(), config().preview_rows as usize);
    assert_eq!(note_samples(&preview["preview"]), 0);

    // 50 rows reach the first values of `note`
    let (status, body) = patch(&app, &format!("/datasets/{id}/configure?dry_run=true&rows=50"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["preview"]["rows"].as_array().unwrap().len(), 50);
    assert_eq!(note_samples(&body["preview"]), 1);

    // out-of-range requests are brought into range, not refused
    let preview = get(&app, &format!("/datasets/{id}/preview?rows=3")).await;
    assert_eq!(preview["preview"]["rows"].as_array().unwrap().len(), 10);
    let preview = get(&app, &format!("/datasets/{id}/preview?rows=999999")).await;
    assert_eq!(preview["preview"]["rows"].as_array().unwrap().len(), 60);

    let options = get(&app, "/mapping/options").await;
    assert_eq!(options["preview_rows"]["options"], json!([10, 30, 50, 100, 250, 500, 1000]));
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




/// An application's signing key, as the application would hold it.
struct Signer {
    issuer: &'static str,
    key: jsonwebtoken::EncodingKey,
    public_key: String,
}

impl Signer {
    fn new(issuer: &'static str) -> Self {
        use base64::Engine;
        use ring::signature::KeyPair;
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        Signer {
            issuer,
            key: jsonwebtoken::EncodingKey::from_ed_der(pkcs8.as_ref()),
            public_key: base64::engine::general_purpose::STANDARD.encode(pair.public_key().as_ref()),
        }
    }

    fn config(&self) -> crate::auth::token::IssuerConfig {
        serde_json::from_value(json!({ "id": self.issuer, "name": "Partner app", "keys": [{ "kid": "k1", "public_key": self.public_key }] })).unwrap()
    }

    /// A token for `sub` with these grants; `edit` changes the claims before signing.
    fn token(&self, sub: &str, grants: Value, edit: impl FnOnce(&mut Value)) -> String {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let mut claims = json!({ "iss": self.issuer, "sub": sub, "aud": "spreadsheet-api", "iat": now, "exp": now + 600, "grants": grants });
        edit(&mut claims);
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
        header.kid = Some("k1".into());
        format!("Bearer {}", jsonwebtoken::encode(&header, &claims, &self.key).unwrap())
    }
}

#[tokio::test]
async fn applications_control_access_with_signed_tokens() {
    let db = database().await;
    let mut auth = service_settings();
    auth.default_ref = None;
    let app = routes::router(routes::AppState::new(db.clone(), auth));
    let partner = Signer::new("partner");
    let rival = Signer::new("rival");
    crate::auth::token::register_issuers(&db, &[partner.config(), rival.config()]).await.unwrap();

    // the application's own server manages a tenant's data
    let manage = partner.token("acme/1", json!([{ "resource": "datasets", "actions": ["upload", "configure", "import", "read"] }]), |_| {});
    let manage = [("authorization", manage.as_str())];
    let (_, me, _) = send(&app, "GET", "/auth/me", &manage, None).await;
    assert_eq!((me["account"].as_str(), me["user_ref"].as_str()), (Some("token"), Some("partner/acme/1")));
    assert_eq!((me["allowance"]["plan"].as_str(), me["grants"][0]["resource"].as_str()), (Some("Service"), Some("datasets")));

    let read_only = partner.token("acme/1", json!([{ "resource": "datasets", "actions": ["read"] }]), |_| {});
    assert_eq!(upload_with(&app, "catalogue.xlsx", &[("authorization", read_only.as_str())]).await.0, StatusCode::FORBIDDEN);
    let (status, uploaded) = upload_with(&app, "catalogue.xlsx", &manage).await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    let id = uploaded["datasets"][0]["dataset_id"].as_str().unwrap().to_string();
    let mapping = json!({ "source": { "header_row": 4 }, "fields": [{ "source": "SKU" }, { "source": "name" }, { "source": "Unit Price", "path": "price" }] });
    let (status, body, _) = send(&app, "PATCH", &format!("/datasets/{id}/configure"), &manage, Some(mapping)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body, _) = send(&app, "PATCH", &format!("/datasets/{id}/import"), &[("authorization", read_only.as_str())], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body, _) = send(&app, "PATCH", &format!("/datasets/{id}/import"), &manage, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(send(&app, "GET", "/datasets", &manage, None).await.1["total"], 2, "one per worksheet");

    // unlisted until the application says otherwise
    assert_eq!(get(&app, &format!("/datasets/{id}")).await["total"], 4);
    let (status, body, _) = send(&app, "PATCH", &format!("/datasets/{id}/access"), &manage, Some(json!({ "visibility": "private" }))).await;
    assert_eq!((status, body["dataset"]["visibility"].as_str()), (StatusCode::OK, Some("private")), "{body}");
    assert_eq!(send(&app, "GET", &format!("/datasets/{id}"), &[], None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(send(&app, "GET", &format!("/datasets/{id}/export"), &[], None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(send(&app, "GET", &format!("/datasets/{id}"), &manage, None).await.1["total"], 4);

    // a viewer is handed a narrow token: one dataset, some rows, some fields
    let narrow = json!([{ "resource": format!("dataset/{id}"), "actions": ["read"],
        "constraints": { "filter": { "sku": { "$in": ["A-1", "B-7"] } }, "fields": ["sku", "name"], "max_page_size": 1 } }]);
    let viewer = partner.token("viewer/9", narrow.clone(), |_| {});
    let viewer = [("authorization", viewer.as_str())];
    let (status, page, _) = send(&app, "GET", &format!("/datasets/{id}?limit=50&sort=sku"), &viewer, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!((page["total"].as_i64(), page["limit"].as_i64()), (Some(2), Some(1)));
    assert_eq!(page["rows"], json!([{ "sku": "A-1", "name": "Red widget" }]));
    assert_eq!(page["fields"].as_array().unwrap().len(), 2, "the other fields are not described either");
    // the token's filter cannot be widened, and it grants nothing else
    let all = urlencode(r#"{"$or":[{"sku":"A-2"},{"sku":"A-1"}]}"#);
    assert_eq!(send(&app, "GET", &format!("/datasets/{id}?filter={all}"), &viewer, None).await.1["total"], 1);
    assert_eq!(send(&app, "GET", &format!("/datasets/{id}/export"), &viewer, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(send(&app, "PATCH", &format!("/datasets/{id}/import"), &viewer, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(send(&app, "GET", "/datasets", &viewer, None).await.0, StatusCode::FORBIDDEN);

    let exporter = partner.token("viewer/9", json!([{ "resource": format!("dataset/{id}"), "actions": ["export"], "constraints": { "fields": ["sku"] } }]), |_| {});
    let (status, rows, _) = send(&app, "GET", &format!("/datasets/{id}/export"), &[("authorization", exporter.as_str())], None).await;
    assert_eq!((status, rows.as_array().map(Vec::len)), (StatusCode::OK, Some(4)));
    assert_eq!(rows[0], json!({ "sku": "A-1" }));

    // another application cannot reach this data, whatever its token claims
    let outsider = rival.token("acme/1", json!([{ "resource": format!("dataset/{id}"), "actions": ["read"] }, { "resource": "datasets", "actions": ["read"] }]), |_| {});
    assert_eq!(send(&app, "GET", &format!("/datasets/{id}"), &[("authorization", outsider.as_str())], None).await.0, StatusCode::NOT_FOUND);

    // tokens that must not verify
    let refused = [
        ("signed by another key", rival.token("viewer/9", narrow.clone(), |c| c["iss"] = json!("partner"))),
        ("expired", partner.token("viewer/9", narrow.clone(), |c| {
            c["exp"] = json!(c["iat"].as_u64().unwrap() - 120);
            c["iat"] = json!(c["iat"].as_u64().unwrap() - 600);
        })),
        ("valid for too long", partner.token("viewer/9", narrow.clone(), |c| c["exp"] = json!(c["iat"].as_u64().unwrap() + 86_400))),
        ("for another service", partner.token("viewer/9", narrow.clone(), |c| c["aud"] = json!("someone-else"))),
        ("without iat", partner.token("viewer/9", narrow.clone(), |c| {
            c.as_object_mut().unwrap().remove("iat");
        })),
        ("an unknown resource", partner.token("viewer/9", json!([{ "resource": "endpoint/x", "actions": ["read"] }]), |_| {})),
        ("an unknown issuer", partner.token("viewer/9", narrow.clone(), |c| c["iss"] = json!("nobody"))),
    ];
    for (why, token) in &refused {
        let (status, body, _) = send(&app, "GET", &format!("/datasets/{id}"), &[("authorization", token.as_str())], None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{why}: {body}");
    }

    // one token withdrawn early, then the whole application
    let revocable = partner.token("viewer/9", narrow.clone(), |c| c["jti"] = json!("t-1"));
    let revocable = [("authorization", revocable.as_str())];
    assert_eq!(send(&app, "GET", &format!("/datasets/{id}"), &revocable, None).await.0, StatusCode::OK);
    sqlx::query("INSERT INTO revoked_tokens (issuer, jti, expires_at) VALUES ('partner', 't-1', '2999-01-01T00:00:00.000Z')").execute(&db).await.unwrap();
    assert_eq!(send(&app, "GET", &format!("/datasets/{id}"), &revocable, None).await.0, StatusCode::UNAUTHORIZED);
    crate::auth::token::register_issuers(&db, &[rival.config()]).await.unwrap();
    assert_eq!(send(&app, "GET", &format!("/datasets/{id}"), &viewer, None).await.0, StatusCode::UNAUTHORIZED, "an application dropped from the file is disabled");
}

#[tokio::test]
async fn filters_compare_by_type_and_ignore_case_in_any_alphabet() {
    let app = app().await;
    let csv = "name,qty,tags,when\nÉclair,5,a;b,2024-03-01\nzebra,12,b;c,2025-01-15\nApple,,c,2023-12-31\n";
    let mut body = b"--XBOUNDARY\r\nContent-Disposition: form-data; name=\"file\"; filename=\"mixed.csv\"\r\n\r\n".to_vec();
    body.extend(csv.as_bytes());
    body.extend(b"\r\n--XBOUNDARY--\r\n");
    let request = Request::post("/datasets/upload").header("content-type", "multipart/form-data; boundary=XBOUNDARY").body(Body::from(body)).unwrap();
    let (status, uploaded) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    let id = uploaded["datasets"][0]["dataset_id"].as_str().unwrap().to_string();
    let mapping = json!({ "fields": [{ "source": "tags", "format": "text[](;)" }, { "source": "name", "searchable": true }] });
    let (status, body) = patch(&app, &format!("/datasets/{id}/configure"), mapping).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = patch(&app, &format!("/datasets/{id}/import"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let names = |result: Value| -> Vec<String> { result["rows"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap().to_string()).collect() };
    let find = async |filter: Value| names(get(&app, &format!("/datasets/{id}?filter={}", urlencode(&filter.to_string()))).await);
    let rule = |field: &str, op: &str, value: Value| json!({ "field": field, "op": op, "value": value });

    assert_eq!(find(rule("name", "eq", json!("éclair"))).await, ["Éclair"], "case is ignored beyond A to Z");
    assert_eq!(find(rule("name", "starts_with", json!("ÉC"))).await, ["Éclair"]);
    assert_eq!(find(rule("name", "ends_with", json!("RA"))).await, ["zebra"]);
    assert_eq!(find(json!({ "name": "éclair" })).await, Vec::<String>::new(), "the MongoDB form of equality is exact");
    assert_eq!(find(json!({ "qty": { "$gt": 5 } })).await, ["zebra"]);
    assert_eq!(find(json!({ "qty": { "$gte": "5" } })).await, Vec::<String>::new(), "a number is not compared with text");
    assert_eq!(find(json!({ "not": rule("qty", "between", json!([1, 20])) })).await, ["Apple"], "an empty value is outside every range");
    // an empty cell of a text file arrives as "", which exists but is empty
    assert_eq!(find(json!({ "field": "qty", "op": "empty" })).await, ["Apple"]);
    assert_eq!(find(json!({ "qty": { "$exists": false } })).await, Vec::<String>::new());
    assert_eq!(find(json!({ "missing": { "$exists": false } })).await.len(), 3);
    assert_eq!(find(json!({ "qty": { "$ne": 5 } })).await, ["zebra", "Apple"]);
    assert_eq!(find(json!({ "when": { "$gte": "2024-01-01", "$lt": "2025-01-01" } })).await, ["Éclair"]);
    assert_eq!(find(json!({ "name": { "$regex": "^[a-z]+$" } })).await, ["zebra"]);
    assert_eq!(find(json!({ "name": { "$regex": "^a", "$options": "i" } })).await, ["Apple"]);
    assert_eq!(find(json!({ "tags": { "$has": "b" } })).await, ["Éclair", "zebra"]);
    assert_eq!(find(json!({ "tags": { "$all": ["b", "c"] } })).await, ["zebra"]);
    assert_eq!(find(rule("tags", "any", json!(["a", "c"]))).await, ["Éclair", "zebra", "Apple"]);
    assert_eq!(find(json!({ "tags": { "$size": 1 } })).await, ["Apple"]);
    assert_eq!(find(json!({ "name": { "$nin": ["zebra"] } })).await, ["Éclair", "Apple"]);

    assert_eq!(names(get(&app, &format!("/datasets/{id}?sort=-qty")).await), ["zebra", "Éclair", "Apple"]);
    assert_eq!(names(get(&app, &format!("/datasets/{id}?q=eclair")).await), ["Éclair"], "search ignores accents and case");
    assert_eq!(get(&app, &format!("/datasets/{id}?q=%22%20OR%20*")).await["total"], 0, "search syntax in the words is inert");
}
