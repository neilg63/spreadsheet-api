//! End-to-end: upload -> configure -> import -> query -> re-import with upsert, through
//! the real router and an in-memory SurrealDB with data/schema.surql applied.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{config::config, db, routes};

async fn app() -> Router {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let uploads = std::env::temp_dir().join(format!("sheet-api-test-{}", std::process::id()));
        // SAFETY: set once, before any test reads the configuration
        unsafe {
            std::env::set_var("DB_PATH", "memory");
            std::env::set_var("UPLOAD_DIR", &uploads);
            std::env::set_var("DEFAULT_USER_PLAN", "pro");
        }
    });
    let db = db::connect(config()).await.expect("database");
    routes::router(routes::AppState { db })
}

async fn call(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn upload(app: &Router, fixture: &str) -> Value {
    let bytes = std::fs::read(format!("tests/fixtures/{fixture}")).unwrap();
    let mut body = format!(
        "--XBOUNDARY\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{fixture}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend(bytes);
    body.extend(b"\r\n--XBOUNDARY--\r\n");
    let request = Request::post("/datasets/upload")
        .header("content-type", "multipart/form-data; boundary=XBOUNDARY")
        .body(Body::from(body))
        .unwrap();
    let (status, json) = call(app, request).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    json
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
