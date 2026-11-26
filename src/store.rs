//! Queries behind the dataset endpoints, and the rules that depend on the caller's plan:
//! quotas, row caps, deadlines and expiry. Each worksheet becomes:
//!
//!   collection (stable, named) ── api_schema ── api_schema_version (+ mapping)
//!        └── dataset (one per upload of that sheet) ── dataset_rows
//!
//! Re-uploading a sheet whose collection already exists reuses that collection's latest
//! schema version, so the same field mapping and primary key apply automatically.

use serde_json::Value;
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

use crate::{
    config::config,
    db::{self, Database, NOW, after, parse_json},
    error::{ApiError, ApiResult},
    models::{Allowance, CollectionInfo, DatasetInfo, DatasetStatus, FieldSpec, Mapping, Me, Plan, VersionInfo, VersionStatus, Visibility},
    sheet::mapping::MappingDoc,
};

fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn json_text<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".into())
}

/// The plan in force for a principal: its own while that lasts, otherwise its issuer's.
async fn plan_on(conn: &mut SqliteConnection, principal: &str) -> ApiResult<Plan> {
    let sql = format!(
        "SELECT pl.name, pl.imports_per_hour, pl.max_rows_per_import, pl.max_collections, pl.mapping_window_secs,
                pl.source_retention_secs, pl.dataset_ttl_secs, pl.features
           FROM principals p
           JOIN issuers i ON i.id = p.issuer
           JOIN plans pl ON pl.id = CASE WHEN p.plan IS NOT NULL AND (p.plan_until IS NULL OR p.plan_until > {NOW})
                                         THEN p.plan ELSE i.default_plan END
          WHERE p.id = ?"
    );
    let row = sqlx::query(&sql)
        .bind(principal)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| ApiError::Internal(format!("no plan for {principal}")))?;
    Ok(Plan {
        name: row.try_get("name")?,
        imports_per_hour: row.try_get("imports_per_hour")?,
        max_rows_per_import: row.try_get("max_rows_per_import")?,
        max_collections: row.try_get("max_collections")?,
        mapping_window_secs: row.try_get("mapping_window_secs")?,
        source_retention_secs: row.try_get("source_retention_secs")?,
        dataset_ttl_secs: row.try_get("dataset_ttl_secs")?,
        features: parse_json(row.try_get("features")?),
    })
}

pub async fn plan_of(db: &Database, principal: &str) -> ApiResult<Plan> {
    plan_on(&mut *db.acquire().await?, principal).await
}

/// Usage of one metric over the last hour, by principal or by client address.
async fn usage_on(conn: &mut SqliteConnection, column: &str, key: &str, metric: &str) -> ApiResult<i64> {
    let sql = format!(
        "SELECT coalesce(sum(qty), 0) FROM usage_events
          WHERE {column} = ? AND metric = ? AND at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1 hour')"
    );
    Ok(sqlx::query_scalar(&sql).bind(key).bind(metric).fetch_one(conn).await?)
}

/// Registers the principal on first contact and refreshes its activity otherwise.
/// Returns the principal's plan allowance.
pub async fn touch_principal(db: &Database, me: &Me) -> ApiResult<Allowance> {
    let id = me.id();
    let mut conn = db.acquire().await?;
    let issuer = sqlx::query("SELECT name, enabled, principal_ttl_secs FROM issuers WHERE id = ?")
        .bind(&me.issuer)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| ApiError::Forbidden(format!("Unknown issuer: {}", me.issuer)))?;
    if !issuer.try_get::<bool, _>("enabled")? {
        return Err(ApiError::Forbidden(format!("Sign-ins from {} are disabled", issuer.try_get::<String, _>("name")?)));
    }
    let ttl: Option<i64> = issuer.try_get("principal_ttl_secs")?;
    // most requests find the principal seen within the last minute, and write nothing
    let seen = sqlx::query("SELECT status, credits, last_seen_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1 minute') AS fresh FROM principals WHERE id = ?")
        .bind(&id)
        .fetch_optional(&mut *conn)
        .await?;
    let fresh = match &seen {
        Some(row) => row.try_get::<bool, _>("fresh")?,
        None => false,
    };
    if !fresh {
        let sql = format!(
            "INSERT INTO principals (id, issuer, external_id, expires_at) VALUES (?1, ?2, ?3, {expiry})
             ON CONFLICT (id) DO UPDATE SET last_seen_at = {NOW}, updated_at = {NOW}, expires_at = {expiry}",
            expiry = after("?4")
        );
        sqlx::query(&sql).bind(&id).bind(&me.issuer).bind(&me.external_id).bind(ttl).execute(&mut *conn).await?;
    }
    let (status, credits): (String, i64) = match seen {
        Some(row) => (row.try_get("status")?, row.try_get("credits")?),
        None => ("active".into(), 0),
    };
    if status == "blocked" {
        return Err(ApiError::Forbidden("This account is blocked".into()));
    }
    let plan = plan_on(&mut conn, &id).await?;
    let used = usage_on(&mut conn, "principal", &id, "import").await?;
    Ok(Allowance {
        plan: plan.name,
        imports_left_hour: (plan.imports_per_hour - used).max(0),
        max_rows: plan.max_rows_per_import,
        credits,
        features: plan.features,
    })
}

pub async fn plan_features(db: &Database, owner: &str) -> ApiResult<Vec<String>> {
    Ok(plan_of(db, owner).await?.features)
}

/// A stored source file, before its record exists.
pub struct NewUpload<'a> {
    pub id: &'a str,
    pub filename: &'a str,
    pub storage_key: &'a str,
    pub content_type: Option<&'a str>,
    pub size: u64,
    /// `file` (multipart), `url` (downloaded) or `transfer` (copied from the shared volume)
    pub source_kind: &'a str,
}

/// Records an upload; its file is purged once the owner's plan says so.
pub async fn create_upload(db: &Database, me: &Me, upload: &NewUpload<'_>, sheets: &[String]) -> ApiResult<()> {
    let owner = me.id();
    let mut conn = db.acquire().await?;
    let plan = plan_on(&mut conn, &owner).await?;
    let sql = format!(
        "INSERT INTO uploads (id, owner, source_kind, filename, storage_key, content_type, size_bytes, sheets, purge_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, {})",
        after("?")
    );
    sqlx::query(&sql)
        .bind(upload.id)
        .bind(&owner)
        .bind(upload.source_kind)
        .bind(upload.filename)
        .bind(upload.storage_key)
        .bind(upload.content_type)
        .bind(upload.size as i64)
        .bind(json_text(sheets))
        .bind(plan.source_retention_secs)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Marks an upload's file as gone, after a failed upload removed it.
pub async fn mark_upload_purged(db: &Database, upload_id: &str) -> ApiResult<()> {
    sqlx::query(&format!("UPDATE uploads SET purged_at = {NOW} WHERE id = ? AND purged_at IS NULL")).bind(upload_id).execute(db).await?;
    Ok(())
}

fn collection_info(row: &SqliteRow) -> ApiResult<CollectionInfo> {
    Ok(CollectionInfo { id: row.try_get("id")?, name: row.try_get("name")?, schema_id: row.try_get("schema_id")? })
}

/// A collection owned by `me`, by id or else by name.
pub async fn find_collection(db: &Database, me: &Me, id_or_name: &str) -> ApiResult<Option<CollectionInfo>> {
    let row = sqlx::query(
        "SELECT id, name, schema_id FROM collections WHERE owner = ?1 AND (id = ?2 OR name = ?2) ORDER BY id = ?2 DESC LIMIT 1",
    )
    .bind(me.id())
    .bind(id_or_name)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(collection_info).transpose()
}

pub async fn create_collection(db: &Database, me: &Me, name: &str) -> ApiResult<CollectionInfo> {
    let owner = me.id();
    let mut tx = db::write(db).await?;
    let plan = plan_on(&mut tx, &owner).await?;
    if let Some(max) = plan.max_collections {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM collections WHERE owner = ?").bind(&owner).fetch_one(&mut *tx).await?;
        if count >= max {
            return Err(ApiError::Rejected("Collection limit reached for your plan".into()));
        }
    }
    let id = new_id();
    let sql = format!("INSERT INTO collections (id, owner, name, expires_at) VALUES (?, ?, ?, {})", after("?"));
    sqlx::query(&sql).bind(&id).bind(&owner).bind(name).bind(plan.dataset_ttl_secs).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(CollectionInfo { id, name: name.to_string(), schema_id: None })
}

/// The collection's api_schema, created on first use.
pub async fn ensure_schema(db: &Database, me: &Me, collection: &CollectionInfo, doc: &MappingDoc) -> ApiResult<String> {
    if let Some(id) = &collection.schema_id {
        return Ok(id.clone());
    }
    let owner = me.id();
    let mut tx = db::write(db).await?;
    let existing: Option<String> = sqlx::query_scalar("SELECT id FROM api_schemas WHERE owner = ? AND name = ?")
        .bind(&owner)
        .bind(&collection.name)
        .fetch_optional(&mut *tx)
        .await?;
    let id = match existing {
        Some(id) => id,
        None => {
            let id = new_id();
            sqlx::query("INSERT INTO api_schemas (id, owner, name, inferred) VALUES (?, ?, ?, 1)")
                .bind(&id)
                .bind(&owner)
                .bind(&collection.name)
                .execute(&mut *tx)
                .await?;
            id
        }
    };
    sqlx::query(&format!("UPDATE collections SET schema_id = ?, updated_at = {NOW} WHERE id = ?"))
        .bind(&id)
        .bind(&collection.id)
        .execute(&mut *tx)
        .await?;
    if let Some(description) = &doc.description {
        sqlx::query(&format!("UPDATE api_schemas SET description = ?, updated_at = {NOW} WHERE id = ?"))
            .bind(description)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(id)
}

pub async fn latest_version(db: &Database, schema_id: &str) -> ApiResult<Option<VersionInfo>> {
    let row = sqlx::query("SELECT id, version, status, definition FROM api_schema_versions WHERE schema_id = ? ORDER BY version DESC LIMIT 1")
        .bind(schema_id)
        .fetch_optional(db)
        .await?;
    let Some(row) = row else { return Ok(None) };
    Ok(Some(VersionInfo {
        id: row.try_get("id")?,
        version: row.try_get("version")?,
        status: VersionStatus::parse(&row.try_get::<String, _>("status")?),
        definition: parse_json(row.try_get("definition")?),
    }))
}

async fn insert_mapping(conn: &mut SqliteConnection, version_id: &str, mapping: &Mapping) -> ApiResult<()> {
    sqlx::query("INSERT INTO mappings (id, schema_version, name, sheet, header_row, columns) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(new_id())
        .bind(version_id)
        .bind(&mapping.name)
        .bind(&mapping.sheet)
        .bind(mapping.header_row)
        .bind(json_text(&mapping.columns))
        .execute(conn)
        .await?;
    Ok(())
}

/// Creates the next draft version of a schema, with its mapping row.
pub async fn create_version(
    db: &Database,
    schema_id: &str,
    definition: &Value,
    field_spec: &[FieldSpec],
    mapping: Mapping,
) -> ApiResult<VersionInfo> {
    let mut tx = db::write(db).await?;
    let version: i64 = sqlx::query_scalar("SELECT coalesce(max(version), 0) + 1 FROM api_schema_versions WHERE schema_id = ?")
        .bind(schema_id)
        .fetch_one(&mut *tx)
        .await?;
    let id = new_id();
    sqlx::query("INSERT INTO api_schema_versions (id, schema_id, version, definition, field_spec) VALUES (?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(schema_id)
        .bind(version)
        .bind(json_text(definition))
        .bind(json_text(field_spec))
        .execute(&mut *tx)
        .await?;
    insert_mapping(&mut tx, &id, &mapping).await?;
    sqlx::query(&format!("UPDATE api_schemas SET updated_at = {NOW} WHERE id = ?")).bind(schema_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(VersionInfo { id, version, status: VersionStatus::Draft, definition: definition.clone() })
}

/// Rewrites a draft version in place. Published versions are not changed.
pub async fn update_draft_version(
    db: &Database,
    version_id: &str,
    definition: &Value,
    field_spec: &[FieldSpec],
    mapping: Mapping,
) -> ApiResult<()> {
    let mut tx = db::write(db).await?;
    let updated = sqlx::query(&format!(
        "UPDATE api_schema_versions SET definition = ?, field_spec = ?, updated_at = {NOW} WHERE id = ? AND status = 'draft'"
    ))
    .bind(json_text(definition))
    .bind(json_text(field_spec))
    .bind(version_id)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::Rejected("Published schema versions are immutable; create a new version".into()));
    }
    sqlx::query("DELETE FROM mappings WHERE schema_version = ?").bind(version_id).execute(&mut *tx).await?;
    insert_mapping(&mut tx, version_id, &mapping).await?;
    tx.commit().await?;
    Ok(())
}

/// Is this version a draft that only `dataset_id` uses, so it can be edited in place?
pub async fn version_is_private_draft(db: &Database, version_id: &str, dataset_id: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT status = 'draft' AND NOT EXISTS (SELECT 1 FROM datasets WHERE schema_version = ?1 AND id != ?2)
           FROM api_schema_versions WHERE id = ?1",
    )
    .bind(version_id)
    .bind(dataset_id)
    .fetch_optional(db)
    .await?
    .unwrap_or(false))
}

/// Creates a dataset for one worksheet of an upload. This is where the hourly import
/// quota is checked and counted, and where the plan's deadlines are set.
#[allow(clippy::too_many_arguments)]
pub async fn create_dataset(
    db: &Database,
    me: &Me,
    id: &str,
    collection_id: &str,
    upload_id: &str,
    sheet: Option<&str>,
    version_id: &str,
    client_ip: Option<&str>,
) -> ApiResult<()> {
    let owner = me.id();
    let mut tx = db::write(db).await?;
    let plan = plan_on(&mut tx, &owner).await?;
    if usage_on(&mut tx, "principal", &owner, "import").await? >= plan.imports_per_hour {
        return Err(ApiError::Rejected("Hourly import limit reached for your plan".into()));
    }
    // guests are also counted by client address, since a guest identity is cheap to change
    let anonymous: bool = sqlx::query_scalar("SELECT trust = 'anonymous' FROM issuers WHERE id = ?").bind(&me.issuer).fetch_one(&mut *tx).await?;
    if let (true, Some(ip)) = (anonymous, client_ip)
        && usage_on(&mut tx, "ip", ip, "import").await? >= plan.imports_per_hour
    {
        return Err(ApiError::Rejected("Hourly import limit reached".into()));
    }
    let sql = format!(
        "INSERT INTO datasets (id, owner, collection_id, upload_id, sheet, schema_version, mapping_id, series, client_ip,
                               mapping_deadline, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                 (SELECT id FROM mappings WHERE schema_version = ?6 LIMIT 1),
                 (SELECT coalesce(max(series), 0) + 1 FROM datasets WHERE collection_id = ?3),
                 ?7, {deadline}, {expiry})",
        deadline = after("?8"),
        expiry = after("?9")
    );
    sqlx::query(&sql)
        .bind(id)
        .bind(&owner)
        .bind(collection_id)
        .bind(upload_id)
        .bind(sheet)
        .bind(version_id)
        .bind(client_ip)
        .bind(plan.mapping_window_secs)
        .bind(plan.dataset_ttl_secs)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO usage_events (principal, metric, ref, ip) VALUES (?, 'import', ?, ?)")
        .bind(&owner)
        .bind(id)
        .bind(client_ip)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Points a dataset at a (new) schema version and, optionally, another collection.
pub async fn retarget_dataset(db: &Database, dataset_id: &str, version_id: &str, collection_id: &str) -> ApiResult<()> {
    let mut tx = db::write(db).await?;
    let old: String = sqlx::query_scalar("SELECT collection_id FROM datasets WHERE id = ?").bind(dataset_id).fetch_one(&mut *tx).await?;
    sqlx::query(&format!(
        "UPDATE datasets SET schema_version = ?1, mapping_id = (SELECT id FROM mappings WHERE schema_version = ?1 LIMIT 1),
                updated_at = {NOW} WHERE id = ?2"
    ))
    .bind(version_id)
    .bind(dataset_id)
    .execute(&mut *tx)
    .await?;
    if old != collection_id {
        sqlx::query(
            "UPDATE datasets SET collection_id = ?1,
                    series = (SELECT coalesce(max(series), 0) + 1 FROM datasets WHERE collection_id = ?1) WHERE id = ?2",
        )
        .bind(collection_id)
        .bind(dataset_id)
        .execute(&mut *tx)
        .await?;
        // a collection created just for this sheet is dropped once nothing uses it
        sqlx::query("DELETE FROM collections WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM datasets WHERE collection_id = ?1)")
            .bind(&old)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

const DATASET_SQL: &str = "SELECT d.id, d.owner, d.status, d.sheet, d.series, d.error, d.num_rows, d.num_invalid, d.imported_at,
        d.created_at, d.mapping_deadline, d.expires_at, d.upload_id,
        u.storage_key, u.purged_at, u.sheets, u.filename,
        c.id AS collection_id, c.name AS collection_name, c.visibility, c.primary_key, c.current_id,
        v.id AS version_id, v.version, v.status AS version_status, v.definition, v.field_spec, v.schema_id
   FROM datasets d
   JOIN collections c ON c.id = d.collection_id
   JOIN api_schema_versions v ON v.id = d.schema_version
   LEFT JOIN uploads u ON u.id = d.upload_id";

fn dataset_info(row: &SqliteRow) -> ApiResult<DatasetInfo> {
    let json = |column: &str| -> ApiResult<Option<String>> { Ok(row.try_get(column)?) };
    Ok(DatasetInfo {
        id: row.try_get("id")?,
        owner: row.try_get("owner")?,
        status: DatasetStatus::parse(&row.try_get::<String, _>("status")?),
        sheet: row.try_get("sheet")?,
        series: row.try_get("series")?,
        error: row.try_get("error")?,
        num_rows: row.try_get("num_rows")?,
        num_invalid: row.try_get("num_invalid")?,
        imported_at: row.try_get("imported_at")?,
        created_at: row.try_get("created_at")?,
        mapping_deadline: row.try_get("mapping_deadline")?,
        expires_at: row.try_get("expires_at")?,
        upload_id: row.try_get("upload_id")?,
        storage_key: row.try_get("storage_key")?,
        purged_at: row.try_get("purged_at")?,
        sheets: json("sheets")?.and_then(|s| serde_json::from_str(&s).ok()),
        filename: row.try_get("filename")?,
        collection_id: row.try_get("collection_id")?,
        collection_name: row.try_get("collection_name")?,
        visibility: Visibility::parse(&row.try_get::<String, _>("visibility")?),
        primary_key: json("primary_key")?.and_then(|s| serde_json::from_str(&s).ok()),
        current_id: row.try_get("current_id")?,
        version_id: row.try_get("version_id")?,
        version: row.try_get("version")?,
        version_status: VersionStatus::parse(&row.try_get::<String, _>("version_status")?),
        definition: parse_json(json("definition")?),
        field_spec: json("field_spec")?.and_then(|s| serde_json::from_str(&s).ok()),
        schema_id: row.try_get("schema_id")?,
    })
}

pub async fn get_dataset(db: &Database, id: &str) -> ApiResult<DatasetInfo> {
    let row = sqlx::query(&format!("{DATASET_SQL} WHERE d.id = ?"))
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("dataset {id} not found")))?;
    dataset_info(&row)
}

pub async fn list_datasets(db: &Database, me: &Me, start: u32, limit: u32) -> ApiResult<(u64, Vec<Value>)> {
    let owner = me.id();
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM datasets WHERE owner = ?").bind(&owner).fetch_one(db).await?;
    let rows = sqlx::query(&format!("{DATASET_SQL} WHERE d.owner = ? ORDER BY d.created_at DESC, d.id DESC LIMIT ? OFFSET ?"))
        .bind(&owner)
        .bind(limit)
        .bind(start)
        .fetch_all(db)
        .await?;
    let datasets = rows.iter().map(|row| dataset_info(row).map(|d| d.summary())).collect::<ApiResult<Vec<_>>>()?;
    Ok((total as u64, datasets))
}

/// Sets who may read a collection's rows. Private collections need a plan with
/// `private_endpoints`.
pub async fn set_visibility(db: &Database, dataset: &DatasetInfo, visibility: Visibility) -> ApiResult<()> {
    if visibility == Visibility::Private && !plan_of(db, &dataset.owner).await?.has("private_endpoints") {
        return Err(ApiError::Forbidden("private data needs a plan with the 'private_endpoints' feature".into()));
    }
    sqlx::query(&format!("UPDATE collections SET visibility = ?, updated_at = {NOW} WHERE id = ?"))
        .bind(visibility.as_str())
        .bind(&dataset.collection_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Starts an import: the mapping window must still be open. Clears rows left by an
/// earlier attempt.
pub async fn begin_import(db: &Database, dataset_id: &str) -> ApiResult<()> {
    let mut tx = db::write(db).await?;
    let closed: bool = sqlx::query_scalar(&format!(
        "SELECT status = 'mapping' AND mapping_deadline IS NOT NULL AND mapping_deadline < {NOW} FROM datasets WHERE id = ?"
    ))
    .bind(dataset_id)
    .fetch_one(&mut *tx)
    .await?;
    if closed {
        return Err(ApiError::Rejected("The mapping window for this import has closed".into()));
    }
    sqlx::query(&format!("UPDATE datasets SET status = 'importing', error = NULL, updated_at = {NOW} WHERE id = ?"))
        .bind(dataset_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM dataset_rows WHERE dataset_id = ?").bind(dataset_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Marks a dataset as failed, with the reason, and removes its rows.
pub async fn fail_import(db: &Database, dataset_id: &str, message: &str) -> ApiResult<()> {
    let mut tx = db::write(db).await?;
    sqlx::query("DELETE FROM dataset_rows WHERE dataset_id = ?").bind(dataset_id).execute(&mut *tx).await?;
    sqlx::query(&format!("UPDATE datasets SET status = 'failed', error = ?, updated_at = {NOW} WHERE id = ?"))
        .bind(message)
        .bind(dataset_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Completes an import: enforces the plan's row cap, counts the rows as usage, publishes
/// a draft schema version with what the full read found, and makes the dataset its
/// collection's current data.
pub async fn finish_import(
    db: &Database,
    dataset: &DatasetInfo,
    num_rows: i64,
    num_invalid: i64,
    definition: &Value,
    field_spec: &[FieldSpec],
    primary_key: &[String],
) -> ApiResult<()> {
    let mut tx = db::write(db).await?;
    if num_rows > plan_on(&mut tx, &dataset.owner).await?.max_rows_per_import {
        return Err(ApiError::Rejected("Row limit exceeded for your plan".into()));
    }
    sqlx::query(&format!(
        "UPDATE datasets SET status = 'imported', num_rows = ?, num_invalid = ?, imported_at = {NOW}, error = NULL,
                updated_at = {NOW} WHERE id = ?"
    ))
    .bind(num_rows)
    .bind(num_invalid)
    .bind(&dataset.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO usage_events (principal, metric, qty, ref) VALUES (?, 'rows_imported', ?, ?)")
        .bind(&dataset.owner)
        .bind(num_rows)
        .bind(&dataset.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(&format!(
        "UPDATE api_schema_versions SET definition = ?, field_spec = ?, status = 'published', updated_at = {NOW}
          WHERE id = ? AND status = 'draft'"
    ))
    .bind(json_text(definition))
    .bind(json_text(field_spec))
    .bind(&dataset.version_id)
    .execute(&mut *tx)
    .await?;
    let key = (!primary_key.is_empty()).then(|| json_text(primary_key));
    sqlx::query(&format!("UPDATE collections SET schema_id = ?, primary_key = ?, updated_at = {NOW} WHERE id = ?"))
        .bind(&dataset.schema_id)
        .bind(key)
        .bind(&dataset.collection_id)
        .execute(&mut *tx)
        .await?;
    // a finished import replaces the collection's live data
    let row = sqlx::query("SELECT auto_publish, current_id FROM collections WHERE id = ?")
        .bind(&dataset.collection_id)
        .fetch_one(&mut *tx)
        .await?;
    if row.try_get::<bool, _>("auto_publish")? {
        let old: Option<String> = row.try_get("current_id")?;
        sqlx::query("UPDATE collections SET current_id = ? WHERE id = ?")
            .bind(&dataset.id)
            .bind(&dataset.collection_id)
            .execute(&mut *tx)
            .await?;
        if let Some(old) = old.filter(|old| *old != dataset.id) {
            sqlx::query(&format!("UPDATE datasets SET status = 'superseded', updated_at = {NOW} WHERE id = ?"))
                .bind(old)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Deletes the upload's file once none of its worksheets still need it.
pub async fn purge_upload_if_done(db: &Database, upload_id: &str) -> ApiResult<()> {
    let key: Option<String> = sqlx::query_scalar(&format!(
        "UPDATE uploads SET purged_at = {NOW}
          WHERE id = ?1 AND purged_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM datasets WHERE upload_id = ?1 AND status IN ('mapping', 'importing', 'failed'))
          RETURNING storage_key"
    ))
    .bind(upload_id)
    .fetch_optional(db)
    .await?;
    if let Some(key) = key {
        remove_upload_file(&key);
    }
    Ok(())
}

pub fn remove_upload_file(storage_key: &str) {
    // storage keys are generated file names; never follow anything path-like
    if storage_key.contains('/') || storage_key.contains('\\') || storage_key.starts_with('.') {
        return;
    }
    let path = config().upload_dir.join(storage_key);
    if let Err(err) = std::fs::remove_file(&path)
        && err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("could not delete {}: {err}", path.display());
        }
}

/// Expires and removes what has run out of time: login sessions and states, revoked
/// tokens, mapping windows, datasets, collections and inactive guests. Purges source
/// files past their plan's retention and returns how many.
pub async fn sweep(db: &Database) -> ApiResult<usize> {
    let expired = format!("expires_at IS NOT NULL AND expires_at < {NOW}");
    for sql in [
        format!("DELETE FROM auth_sessions WHERE {expired}"),
        format!("DELETE FROM auth_states WHERE {expired}"),
        format!("DELETE FROM revoked_tokens WHERE {expired}"),
        format!("UPDATE datasets SET status = 'expired' WHERE status = 'mapping' AND mapping_deadline IS NOT NULL AND mapping_deadline < {NOW}"),
        // rows go with their dataset, datasets with their collection, everything with its principal
        format!("DELETE FROM datasets WHERE {expired}"),
        format!("DELETE FROM collections WHERE {expired}"),
        format!("DELETE FROM principals WHERE {expired}"),
    ] {
        // one step failing must not hold up the others
        if let Err(err) = sqlx::query(&sql).execute(db).await {
            tracing::warn!("sweep step failed ({err}): {sql}");
        }
    }
    let keys: Vec<String> = sqlx::query_scalar(&format!(
        "UPDATE uploads SET purged_at = {NOW} WHERE purged_at IS NULL AND purge_at IS NOT NULL AND purge_at < {NOW} RETURNING storage_key"
    ))
    .fetch_all(db)
    .await?;
    for key in &keys {
        remove_upload_file(key);
    }
    Ok(keys.len())
}

/// Moves what a guest made into the account they just signed in to: collections whose
/// names the account doesn't already use, with their datasets and schemas, and every
/// upload. Expiry dates are recomputed from the account's plan. Returns the number of
/// collections moved; anything left behind expires with the guest.
pub async fn claim_guest_data(db: &Database, guest: &Me, account: &Me) -> ApiResult<u64> {
    let (guest, user) = (guest.id(), account.id());
    let mut tx = db::write(db).await?;
    let plan = plan_on(&mut tx, &user).await?;
    let moved = sqlx::query(&format!(
        "UPDATE collections SET owner = ?1, expires_at = {expiry}, updated_at = {NOW}
          WHERE owner = ?2 AND name NOT IN (SELECT name FROM collections WHERE owner = ?1)",
        expiry = after("?3")
    ))
    .bind(&user)
    .bind(&guest)
    .bind(plan.dataset_ttl_secs)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query(&format!(
        "UPDATE datasets SET owner = ?1, expires_at = {expiry},
                mapping_deadline = CASE WHEN status = 'mapping' THEN {deadline} ELSE mapping_deadline END
          WHERE owner = ?2 AND collection_id IN (SELECT id FROM collections WHERE owner = ?1)",
        expiry = after("?3"),
        deadline = after("?4")
    ))
    .bind(&user)
    .bind(&guest)
    .bind(plan.dataset_ttl_secs)
    .bind(plan.mapping_window_secs)
    .execute(&mut *tx)
    .await?;
    // a schema the moved data uses comes along, under another name if the account has that one
    sqlx::query(
        "UPDATE api_schemas
            SET name = CASE WHEN name IN (SELECT name FROM api_schemas WHERE owner = ?1) THEN name || ' (' || substr(id, -8) || ')' ELSE name END,
                owner = ?1
          WHERE owner = ?2
            AND (id IN (SELECT schema_id FROM collections WHERE owner = ?1)
                 OR id IN (SELECT v.schema_id FROM api_schema_versions v JOIN datasets d ON d.schema_version = v.id WHERE d.owner = ?1))",
    )
    .bind(&user)
    .bind(&guest)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE uploads SET owner = ? WHERE owner = ?").bind(&user).bind(&guest).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(moved)
}
