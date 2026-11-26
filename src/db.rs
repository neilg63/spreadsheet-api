//! The SQLite database: connection pool, schema set-up and a few query helpers.
//!
//! A file database runs in WAL mode, so readers never wait for the one writer. Writes
//! go through [`write`], which takes the write lock up front: a transaction that starts
//! by reading and writes later can otherwise fail as "busy" instead of waiting.

use std::{path::Path, str::FromStr, time::Duration};

use serde_json::Value;
use sqlx::{
    Sqlite, SqlitePool, Transaction,
    sqlite::{SqliteArguments, SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::{
    config::Config,
    error::{ApiError, ApiResult},
};

pub type Database = SqlitePool;
pub type Tx = Transaction<'static, Sqlite>;
pub type Query<'q> = sqlx::query::Query<'q, Sqlite, SqliteArguments<'q>>;

/// The current time as the schema stores it: UTC text that sorts as time.
pub const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

/// SQL for the time `seconds` from now, where `seconds` is a placeholder or an integer
/// expression. NULL seconds give NULL, which the schema reads as "never".
pub fn after(seconds: &str) -> String {
    format!("strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ({seconds}) || ' seconds')")
}

pub async fn connect(config: &Config) -> ApiResult<Database> {
    let in_memory = config.db_path == "memory";
    let options = if in_memory {
        SqliteConnectOptions::from_str("sqlite::memory:").map_err(|e| ApiError::Internal(e.to_string()))?
    } else {
        if let Some(dir) = Path::new(&config.db_path).parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        SqliteConnectOptions::new()
            .filename(&config.db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            // in WAL mode a commit survives a crash of the service; only a power cut can lose the last ones
            .synchronous(SqliteSynchronous::Normal)
            .pragma("cache_size", format!("-{}", config.db_cache_kb))
            .pragma("journal_size_limit", (64 * 1024 * 1024).to_string())
    };
    let options = options.foreign_keys(true).busy_timeout(Duration::from_secs(30)).with_regexp();
    let pool = SqlitePoolOptions::new()
        // an in-memory database belongs to its one connection, which must never be closed
        .max_connections(if in_memory { 1 } else { config.db_connections })
        .min_connections(if in_memory { 1 } else { 0 })
        .idle_timeout(if in_memory { None } else { Some(Duration::from_secs(300)) })
        .max_lifetime(None)
        .after_connect(|conn, _| {
            Box::pin(async move {
                let mut handle = conn.lock_handle().await?;
                functions::register(handle.as_raw_handle().as_ptr()).map_err(sqlx::Error::Protocol)
            })
        })
        .connect_with(options)
        .await?;
    ensure_schema(&pool).await?;
    Ok(pool)
}

/// Begins a transaction that holds the write lock from the start.
pub async fn write(db: &Database) -> ApiResult<Tx> {
    Ok(db.begin_with("BEGIN IMMEDIATE").await?)
}

fn fingerprint(text: &str) -> String {
    // FNV-1a: stable across builds, unlike std's DefaultHasher
    let hash = text.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{hash:016x}")
}

async fn meta(db: &Database, key: &str) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar("SELECT value FROM service_meta WHERE key = ?").bind(key).fetch_optional(db).await?)
}

async fn set_meta(db: &Database, key: &str, value: &str) -> ApiResult<()> {
    sqlx::query("INSERT INTO service_meta (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value")
        .bind(key)
        .bind(value)
        .execute(db)
        .await?;
    Ok(())
}

/// Applies data/schema.sql to a new database, then any files in data/migrations
/// (applied in name order, each once) to databases created from an earlier schema.
/// A new database is already current, so it records every migration as applied.
async fn ensure_schema(db: &Database) -> ApiResult<()> {
    let path = dotenvy::var("SCHEMA_PATH").unwrap_or_else(|_| "data/schema.sql".to_string());
    let schema = std::fs::read_to_string(Path::new(&path))
        .map_err(|e| ApiError::Internal(format!("cannot read schema file {path}: {e}")))?;
    let hash = fingerprint(&schema);
    let migrations = migration_files(Path::new(&path))?;
    let has_schema: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'plans'").fetch_optional(db).await?;
    if has_schema.is_none() {
        tracing::info!("applying {path} to a new database");
        sqlx::raw_sql(&format!("BEGIN IMMEDIATE;\n{schema}\nCOMMIT;"))
            .execute(db)
            .await
            .map_err(|e| ApiError::Internal(format!("{path} could not be applied: {e}")))?;
        set_meta(db, "schema_hash", &hash).await?;
        let names: Vec<&str> = migrations.iter().map(|(name, _)| name.as_str()).collect();
        set_meta(db, "migrations", &serde_json::to_string(&names).unwrap_or_default()).await?;
        return Ok(());
    }
    let mut applied: Vec<String> = meta(db, "migrations").await?.and_then(|m| serde_json::from_str(&m).ok()).unwrap_or_default();
    for (name, sql) in &migrations {
        if applied.contains(name) {
            continue;
        }
        tracing::info!("applying migration {name}");
        sqlx::raw_sql(&format!("BEGIN IMMEDIATE;\n{sql}\nCOMMIT;"))
            .execute(db)
            .await
            .map_err(|e| ApiError::Internal(format!("migration {name} failed: {e}")))?;
        applied.push(name.clone());
        set_meta(db, "migrations", &serde_json::to_string(&applied).unwrap_or_default()).await?;
        set_meta(db, "schema_hash", &hash).await?;
    }
    if meta(db, "schema_hash").await?.as_deref() != Some(hash.as_str()) {
        tracing::warn!("{path} has changed since this database was set up; add the changes as a file in data/migrations");
    }
    Ok(())
}

/// `(file name, contents)` of every .sql file in the migrations directory, sorted.
fn migration_files(schema_path: &Path) -> ApiResult<Vec<(String, String)>> {
    let dir = dotenvy::var("MIGRATIONS_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| schema_path.with_file_name("migrations"));
    let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(vec![]) };
    let mut files = vec![];
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "sql") {
            let name = entry.file_name().to_string_lossy().to_string();
            files.push((name, std::fs::read_to_string(&path)?));
        }
    }
    files.sort();
    Ok(files)
}

/// Binds a JSON value as the SQL value it compares as: numbers as numbers, text as
/// text, true and false as 1 and 0, lists and objects as their JSON text.
pub fn bind_json<'q>(query: Query<'q>, value: &Value) -> Query<'q> {
    match value {
        Value::Null => query.bind(None::<i64>),
        Value::Bool(b) => query.bind(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => query.bind(i),
            None => query.bind(n.as_f64().unwrap_or_default()),
        },
        Value::String(s) => query.bind(s.clone()),
        other => query.bind(other.to_string()),
    }
}

/// A JSON column, parsed; `default` for NULL or unreadable text.
pub fn parse_json<T: serde::de::DeserializeOwned + Default>(text: Option<String>) -> T {
    text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// SQL functions SQLite lacks, added to every connection.
mod functions {
    use std::os::raw::{c_char, c_int};

    use libsqlite3_sys::{
        SQLITE_DETERMINISTIC, SQLITE_NULL, SQLITE_OK, SQLITE_TRANSIENT, SQLITE_UTF8, sqlite3, sqlite3_context,
        sqlite3_create_function_v2, sqlite3_result_null, sqlite3_result_text, sqlite3_value, sqlite3_value_bytes,
        sqlite3_value_text, sqlite3_value_type,
    };

    /// `lower_u(x)`: lower case for every alphabet. SQLite's own `lower` only knows A to Z.
    unsafe extern "C" fn lower_u(context: *mut sqlite3_context, _argc: c_int, argv: *mut *mut sqlite3_value) {
        // SAFETY: SQLite calls this with the one argument the function was registered for,
        // and the text it returns stays valid until the next call on the same value.
        unsafe {
            let value = *argv;
            if sqlite3_value_type(value) == SQLITE_NULL {
                return sqlite3_result_null(context);
            }
            let text = sqlite3_value_text(value);
            if text.is_null() {
                return sqlite3_result_null(context);
            }
            let bytes = std::slice::from_raw_parts(text, sqlite3_value_bytes(value) as usize);
            let lowered = String::from_utf8_lossy(bytes).to_lowercase();
            // SQLITE_TRANSIENT: SQLite copies the text before `lowered` is dropped
            sqlite3_result_text(context, lowered.as_ptr() as *const c_char, lowered.len() as c_int, SQLITE_TRANSIENT());
        }
    }

    pub fn register(connection: *mut sqlite3) -> Result<(), String> {
        // SAFETY: `connection` is the live handle sqlx has locked for this call.
        let status = unsafe {
            sqlite3_create_function_v2(
                connection,
                c"lower_u".as_ptr(),
                1,
                SQLITE_UTF8 | SQLITE_DETERMINISTIC,
                std::ptr::null_mut(),
                Some(lower_u),
                None,
                None,
                None,
            )
        };
        if status == SQLITE_OK { Ok(()) } else { Err(format!("could not add the lower_u SQL function (SQLite error {status})")) }
    }
}
