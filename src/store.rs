//! Queries behind the dataset endpoints; the records themselves are in `models`. Each
//! worksheet becomes:
//!
//!   collection (stable, named) ── api_schema ── api_schema_version (+ mapping)
//!        └── dataset (one per upload of that sheet) ── dataset_rows
//!
//! Re-uploading a sheet whose collection already exists reuses that collection's latest
//! schema version, so the same field mapping and primary key apply automatically.

use serde_json::{Value, json};

use crate::{
    config::config,
    db::{Database, run, run_last, run_one},
    error::{ApiError, ApiResult},
    models::{Allowance, CollectionInfo, DatasetInfo, FieldSpec, Me, VersionInfo},
    sheet::mapping::MappingDoc,
};

/// Registers the principal on first contact and refreshes its activity otherwise.
/// Returns the principal's plan allowance.
pub async fn touch_principal(db: &Database, me: &Me) -> ApiResult<Allowance> {
    let sql = "
        LET $iss = type::record('issuers', $issuer);
        IF $iss.name = NONE { THROW 'Unknown issuer: ' + $issuer };
        IF !$iss.enabled { THROW 'Sign-ins from ' + $iss.name + ' are disabled' };
        LET $p = type::record('principals', [$issuer, $ext]);
        IF $p.created_at = NONE {
            CREATE $p CONTENT { issuer: $iss, external_id: $ext };
        } ELSE {
            UPDATE $p SET last_seen_at = time::now();
        };
        IF $p.status = 'blocked' { THROW 'This account is blocked' };
        RETURN fn::allowance($p);";
    let vars = json!({ "issuer": me.issuer, "ext": me.external_id });
    let allowance = run_last(db, sql, vars).await.map_err(|e| match e {
        ApiError::Rejected(message) => ApiError::Forbidden(message),
        other => other,
    })?;
    serde_json::from_value(allowance).map_err(|e| ApiError::Internal(format!("unexpected allowance: {e}")))
}

pub async fn plan_features(db: &Database, owner: &[String]) -> ApiResult<Vec<String>> {
    let features = run_last(
        db,
        "RETURN fn::effective_plan(type::record('principals', $owner)).features;",
        json!({ "owner": owner }),
    )
    .await?;
    Ok(serde_json::from_value(features).unwrap_or_default())
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

pub async fn create_upload(db: &Database, me: &Me, upload: &NewUpload<'_>, sheets: &[String]) -> ApiResult<()> {
    let sql = "CREATE type::record('uploads', $id) CONTENT {
            owner: type::record('principals', $me), source_kind: $kind, filename: $filename,
            storage_key: $key, size_bytes: $size, sheets: $sheets,
            content_type: IF $content_type { $content_type } ELSE { NONE }
        } RETURN NONE;";
    let vars = json!({
        "id": upload.id, "me": me.key(), "kind": upload.source_kind, "filename": upload.filename,
        "key": upload.storage_key, "size": upload.size, "sheets": sheets,
        "content_type": upload.content_type.unwrap_or_default(),
    });
    run(db, sql, vars).await?;
    Ok(())
}

const COLLECTION_FIELDS: &str =
    "record::id(id) AS id, name, IF schema { record::id(schema) } AS schema_id";

/// A collection owned by `me`, by id or else by name.
pub async fn find_collection(db: &Database, me: &Me, id_or_name: &str) -> ApiResult<Option<CollectionInfo>> {
    let sql = format!(
        "LET $me = type::record('principals', $owner);
        LET $by_id = type::record('collections', $ref);
        RETURN IF $by_id.owner = $me {{ (SELECT {COLLECTION_FIELDS} FROM ONLY $by_id) }}
            ELSE {{ (SELECT {COLLECTION_FIELDS} FROM collections WHERE owner = $me AND name = $ref LIMIT 1)[0] }};"
    );
    let row = run_one(db, &sql, json!({ "owner": me.key(), "ref": id_or_name })).await?;
    Ok(row.and_then(|r| serde_json::from_value(r).ok()))
}

pub async fn create_collection(db: &Database, me: &Me, name: &str) -> ApiResult<CollectionInfo> {
    let sql = format!(
        "CREATE type::record('collections', $id) CONTENT {{ owner: type::record('principals', $owner), name: $name }}
            RETURN {COLLECTION_FIELDS};"
    );
    let id = uuid::Uuid::now_v7().to_string();
    let row = run_one(db, &sql, json!({ "id": id, "owner": me.key(), "name": name })).await?;
    row.and_then(|r| serde_json::from_value(r).ok())
        .ok_or_else(|| ApiError::Internal("collection was not created".into()))
}

/// The collection's api_schema, created on first use.
pub async fn ensure_schema(db: &Database, me: &Me, collection: &CollectionInfo, doc: &MappingDoc) -> ApiResult<String> {
    if let Some(id) = &collection.schema_id {
        return Ok(id.clone());
    }
    let sql = "
        LET $me = type::record('principals', $owner);
        LET $existing = (SELECT VALUE id FROM api_schemas WHERE owner = $me AND name = $name LIMIT 1)[0];
        LET $schema = IF $existing { $existing } ELSE {
            (CREATE ONLY api_schemas CONTENT { owner: $me, name: $name, inferred: true }).id
        };
        UPDATE type::record('collections', $collection) SET schema = $schema;
        RETURN record::id($schema);";
    let vars = json!({ "owner": me.key(), "name": collection.name, "collection": collection.id });
    let id = run_last(db, sql, vars).await?;
    if let Some(description) = &doc.description {
        set_schema_description(db, id.as_str().unwrap_or_default(), description).await?;
    }
    id.as_str().map(str::to_string).ok_or_else(|| ApiError::Internal("schema was not created".into()))
}

async fn set_schema_description(db: &Database, schema_id: &str, description: &str) -> ApiResult<()> {
    run(
        db,
        "UPDATE type::record('api_schemas', $id) SET description = $description;",
        json!({ "id": schema_id, "description": description }),
    )
    .await?;
    Ok(())
}

pub async fn latest_version(db: &Database, schema_id: &str) -> ApiResult<Option<VersionInfo>> {
    let row = run_one(
        db,
        "SELECT record::id(id) AS id, version, status, definition FROM api_schema_versions
            WHERE schema = type::record('api_schemas', $schema) ORDER BY version DESC LIMIT 1;",
        json!({ "schema": schema_id }),
    )
    .await?;
    Ok(row.and_then(|r| serde_json::from_value(r).ok()))
}

/// Creates the next draft version of a schema, with its mapping row.
pub async fn create_version(
    db: &Database,
    schema_id: &str,
    definition: &Value,
    field_spec: &[FieldSpec],
    mapping: Value,
) -> ApiResult<VersionInfo> {
    let sql = "
        LET $schema = type::record('api_schemas', $schema_id);
        LET $next = math::max(array::push((SELECT VALUE version FROM api_schema_versions WHERE schema = $schema), 0)) + 1;
        LET $v = CREATE ONLY api_schema_versions CONTENT {
            schema: $schema, version: $next, definition: $definition, field_spec: $spec
        };
        CREATE mappings CONTENT $mapping.extend({ schema_version: $v.id }) RETURN NONE;
        UPDATE $schema SET updated_at = time::now() RETURN NONE;
        RETURN { id: record::id($v.id), version: $v.version, status: $v.status, definition: $v.definition };";
    let vars = json!({ "schema_id": schema_id, "definition": definition, "spec": field_spec, "mapping": mapping });
    let row = run_last(db, sql, vars).await?;
    serde_json::from_value(row).map_err(|e| ApiError::Internal(format!("unexpected version record: {e}")))
}

/// Rewrites a draft version in place (published versions are immutable).
pub async fn update_draft_version(
    db: &Database,
    version_id: &str,
    definition: &Value,
    field_spec: &[FieldSpec],
    mapping: Value,
) -> ApiResult<()> {
    let sql = "
        LET $v = type::record('api_schema_versions', $id);
        UPDATE $v SET definition = $definition, field_spec = $spec RETURN NONE;
        DELETE mappings WHERE schema_version = $v;
        CREATE mappings CONTENT $mapping.extend({ schema_version: $v }) RETURN NONE;";
    run(db, sql, json!({ "id": version_id, "definition": definition, "spec": field_spec, "mapping": mapping })).await?;
    Ok(())
}

/// Is this version a draft that only `dataset_id` uses, so it can be edited in place?
pub async fn version_is_private_draft(db: &Database, version_id: &str, dataset_id: &str) -> ApiResult<bool> {
    let result = run_last(
        db,
        "LET $v = type::record('api_schema_versions', $v_id);
        RETURN $v.status = 'draft'
            AND count(SELECT id FROM datasets WHERE schema_version = $v AND id != type::record('datasets', $ds)) = 0;",
        json!({ "v_id": version_id, "ds": dataset_id }),
    )
    .await?;
    Ok(result.as_bool().unwrap_or(false))
}

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
    // client_ip feeds the schema's per-IP import quota for anonymous principals
    let sql = "
        LET $coll = type::record('collections', $collection);
        LET $v = type::record('api_schema_versions', $version);
        CREATE type::record('datasets', $id) CONTENT {
            owner: type::record('principals', $owner),
            collection: $coll,
            upload: type::record('uploads', $upload),
            schema_version: $v,
            mapping: (SELECT VALUE id FROM mappings WHERE schema_version = $v LIMIT 1)[0],
            series: math::max(array::push((SELECT VALUE series FROM datasets WHERE collection = $coll), 0)) + 1,
            sheet: IF $sheet { $sheet } ELSE { NONE },
            client_ip: IF $ip { $ip } ELSE { NONE }
        } RETURN NONE;";
    let vars = json!({
        "id": id, "owner": me.key(), "collection": collection_id, "upload": upload_id,
        "version": version_id, "sheet": sheet.unwrap_or_default(), "ip": client_ip.unwrap_or_default(),
    });
    run(db, sql, vars).await?;
    Ok(())
}

/// Points a dataset at a (new) schema version and, optionally, another collection.
pub async fn retarget_dataset(db: &Database, dataset_id: &str, version_id: &str, collection_id: &str) -> ApiResult<()> {
    let sql = "
        LET $ds = type::record('datasets', $id);
        LET $coll = type::record('collections', $collection);
        LET $old = $ds.collection;
        LET $v = type::record('api_schema_versions', $version);
        UPDATE $ds SET schema_version = $v,
            mapping = (SELECT VALUE id FROM mappings WHERE schema_version = $v LIMIT 1)[0] RETURN NONE;
        IF $old != $coll {
            UPDATE $ds SET collection = $coll,
                series = math::max(array::push((SELECT VALUE series FROM datasets WHERE collection = $coll), 0)) + 1 RETURN NONE;
            -- a collection created just for this sheet is dropped once nothing uses it
            IF count(SELECT id FROM datasets WHERE collection = $old) = 0 { DELETE $old };
        };";
    run(db, sql, json!({ "id": dataset_id, "collection": collection_id, "version": version_id })).await?;
    Ok(())
}

const DATASET_FIELDS: &str = "record::id(id) AS id, record::id(owner) AS owner, status, sheet, series, error,
    num_rows, num_invalid, imported_at, created_at, mapping_deadline, expires_at,
    IF upload { record::id(upload) } AS upload_id, upload.storage_key AS storage_key,
    upload.purged_at AS purged_at, upload.sheets AS sheets, upload.filename AS filename,
    record::id(collection) AS collection_id, collection.name AS collection_name,
    collection.primary_key AS primary_key, IF collection.current { record::id(collection.current) } AS current_id,
    record::id(schema_version) AS version_id, schema_version.version AS version,
    schema_version.status AS version_status, schema_version.definition AS definition,
    schema_version.field_spec AS field_spec, record::id(schema_version.schema) AS schema_id";

pub async fn get_dataset(db: &Database, id: &str) -> ApiResult<DatasetInfo> {
    let sql = format!("SELECT {DATASET_FIELDS} FROM datasets WHERE id = type::record('datasets', $id);");
    let row = run_one(db, &sql, json!({ "id": id })).await?;
    let row = row.ok_or_else(|| ApiError::NotFound(format!("dataset {id} not found")))?;
    serde_json::from_value(row).map_err(|e| ApiError::Internal(format!("unexpected dataset record: {e}")))
}

/// Like `get_dataset`, but only for its owner.
pub async fn get_own_dataset(db: &Database, me: &Me, id: &str) -> ApiResult<DatasetInfo> {
    let dataset = get_dataset(db, id).await?;
    if !me.owns(&dataset.owner) {
        return Err(ApiError::NotFound(format!("dataset {id} not found")));
    }
    Ok(dataset)
}

pub async fn list_datasets(db: &Database, me: &Me, start: u32, limit: u32) -> ApiResult<(u64, Vec<Value>)> {
    let sql = format!(
        "LET $me = type::record('principals', $owner);
        RETURN (SELECT count() FROM datasets WHERE owner = $me GROUP ALL)[0].count ?? 0;
        SELECT {DATASET_FIELDS} FROM datasets WHERE owner = $me ORDER BY created_at DESC LIMIT $limit START $start;"
    );
    let mut results = run(db, &sql, json!({ "owner": me.key(), "start": start, "limit": limit })).await?;
    let rows = results.pop().unwrap_or_default();
    let total = results.pop().and_then(|t| t.as_u64()).unwrap_or(0);
    let rows = serde_json::from_value::<Vec<DatasetInfo>>(rows)
        .map_err(|e| ApiError::Internal(format!("unexpected dataset record: {e}")))?;
    Ok((total, rows.iter().map(DatasetInfo::summary).collect()))
}

/// Deletes the upload's file once none of its worksheets still need it.
pub async fn purge_upload_if_done(db: &Database, upload_id: &str) -> ApiResult<()> {
    let sql = "
        LET $u = type::record('uploads', $id);
        LET $pending = count(SELECT id FROM datasets WHERE upload = $u AND status IN ['mapping', 'importing', 'failed']);
        -- a top-level RETURN inside IF doesn't stop later statements, so the purge is the ELSE branch
        RETURN IF $u.purged_at != NONE OR $pending > 0 { NONE } ELSE {
            (UPDATE ONLY $u SET purged_at = time::now()).storage_key
        };";
    if let Some(key) = run_last(db, sql, json!({ "id": upload_id })).await?.as_str() {
        remove_upload_file(key);
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

/// Expires and purges records via the schema's fn::sweep, then deletes purged files.
pub async fn sweep(db: &Database) -> ApiResult<usize> {
    run(
        db,
        "DELETE auth_sessions WHERE expires_at < time::now(); DELETE auth_states WHERE expires_at < time::now();",
        json!({}),
    )
    .await?;
    let keys = run_last(db, "RETURN fn::sweep();", json!({})).await?;
    let keys: Vec<String> = serde_json::from_value(keys).unwrap_or_default();
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
    let sql = "
        LET $g = type::record('principals', $guest);
        LET $u = type::record('principals', $user);
        LET $limits = fn::effective_plan($u).limits;
        LET $expires = IF $limits.dataset_ttl != NONE { time::now() + $limits.dataset_ttl } ELSE { NONE };
        LET $deadline = IF $limits.mapping_window != NONE { time::now() + $limits.mapping_window } ELSE { NONE };
        LET $taken = SELECT VALUE name FROM collections WHERE owner = $u;
        LET $moved = (UPDATE collections SET owner = $u, expires_at = $expires
            WHERE owner = $g AND name NOTINSIDE $taken RETURN VALUE id);
        -- selected first: SurrealDB 3.3 doesn't follow collection.owner inside UPDATE ... WHERE
        LET $datasets = SELECT VALUE id FROM datasets WHERE owner = $g AND collection.owner = $u;
        UPDATE $datasets SET owner = $u, expires_at = $expires,
            mapping_deadline = IF status = 'mapping' { $deadline } ELSE { mapping_deadline } RETURN NONE;
        LET $schemas_taken = SELECT VALUE name FROM api_schemas WHERE owner = $u;
        UPDATE api_schemas SET owner = $u WHERE owner = $g AND name NOTINSIDE $schemas_taken RETURN NONE;
        UPDATE uploads SET owner = $u WHERE owner = $g RETURN NONE;
        RETURN array::len($moved);";
    let moved = run_last(db, sql, json!({ "guest": guest.key(), "user": account.key() })).await?;
    Ok(moved.as_u64().unwrap_or(0))
}
