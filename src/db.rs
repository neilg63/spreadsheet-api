use std::path::Path;

use serde_json::{Value, json};
use surrealdb::{
    Surreal,
    engine::local::{Db, Mem, SurrealKv},
};

use crate::{
    config::Config,
    error::{ApiError, ApiResult},
};

pub type Database = Surreal<Db>;

pub async fn connect(config: &Config) -> ApiResult<Database> {
    let db = if config.db_path == "memory" {
        Surreal::new::<Mem>(()).await?
    } else {
        std::fs::create_dir_all(&config.db_path)?;
        Surreal::new::<SurrealKv>(config.db_path.as_str()).await?
    };
    db.use_ns(config.db_namespace.as_str()).use_db(config.db_database.as_str()).await?;
    ensure_schema(&db).await?;
    Ok(db)
}

fn fingerprint(text: &str) -> String {
    // FNV-1a: stable across builds, unlike std's DefaultHasher
    let hash = text.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{hash:016x}")
}

/// Applies data/schema.surql to a fresh database. On an existing one it only warns when
/// the file has changed since it was applied: `DEFINE` statements are not idempotent and
/// re-running index definitions would rebuild them, so changes need a deliberate migration.
async fn ensure_schema(db: &Database) -> ApiResult<()> {
    let path = dotenvy::var("SCHEMA_PATH").unwrap_or_else(|_| "data/schema.surql".to_string());
    let schema = std::fs::read_to_string(Path::new(&path))
        .map_err(|e| ApiError::Internal(format!("cannot read schema file {path}: {e}")))?;
    let hash = fingerprint(&schema);
    let tables = run(db, "RETURN object::keys((INFO FOR DB).tables);", json!({})).await?;
    let has_schema = tables[0].as_array().is_some_and(|t| t.iter().any(|name| name == "plans"));
    if !has_schema {
        tracing::info!("applying {path} to a new database");
        run(db, &schema, json!({})).await?;
        run(db, "UPSERT service_meta:schema CONTENT { hash: $hash, applied_at: time::now() };", json!({ "hash": hash })).await?;
        return Ok(());
    }
    let applied = run(db, "RETURN service_meta:schema.hash;", json!({})).await?;
    if applied[0].as_str() != Some(hash.as_str()) {
        tracing::warn!("{path} differs from the schema this database was created with; apply the changes as a migration");
    }
    Ok(())
}

/// Runs a (possibly multi-statement) query with bound variables and returns each
/// statement's result as JSON. Fails with the first meaningful statement error.
pub async fn run(db: &Database, sql: &str, vars: Value) -> ApiResult<Vec<Value>> {
    let mut query = db.query(sql);
    if let Value::Object(map) = vars {
        for (key, value) in map {
            query = query.bind((key, value));
        }
    }
    let mut response = query.await?;
    let mut errors: Vec<(usize, surrealdb::Error)> = response.take_errors().into_iter().collect();
    if !errors.is_empty() {
        errors.sort_by_key(|(i, _)| *i);
        // In a failed transaction the statements after the real error report
        // "not executed"/"cannot COMMIT"; the cause is the first other error.
        let position = errors
            .iter()
            .position(|(_, e)| {
                let text = e.to_string();
                !text.contains("not executed") && !text.contains("Cannot COMMIT")
            })
            .unwrap_or(0);
        return Err(errors.swap_remove(position).1.into());
    }
    let mut results = Vec::with_capacity(response.num_statements());
    for i in 0..response.num_statements() {
        let value = response.take::<surrealdb::types::Value>(i)?;
        results.push(value.into_json_value());
    }
    Ok(results)
}

/// The result of the last statement.
pub async fn run_last(db: &Database, sql: &str, vars: Value) -> ApiResult<Value> {
    Ok(run(db, sql, vars).await?.pop().unwrap_or(Value::Null))
}

/// The first row of the last statement's result, or the result itself for `ONLY` queries.
pub async fn run_one(db: &Database, sql: &str, vars: Value) -> ApiResult<Option<Value>> {
    Ok(match run_last(db, sql, vars).await? {
        Value::Array(mut rows) if !rows.is_empty() => Some(rows.swap_remove(0)),
        Value::Array(_) | Value::Null => None,
        other => Some(other),
    })
}
