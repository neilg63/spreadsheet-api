//! Full import of one worksheet into dataset_rows.
//!
//! Rows stream from the reader through a bounded channel and are inserted in batches, so
//! memory stays flat for large sheets. With a primary key, `upsert` mode then carries
//! over the collection's previous rows whose keys the new file doesn't contain; when the
//! dataset is marked imported, the schema's event makes it the collection's current data.

use std::{collections::HashSet, time::Instant};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    db::{Database, run, run_last},
    error::{ApiError, ApiResult},
    sheet::{
        mapping::{FieldType, MappingDoc, ResolvedMapping, base_options},
        reader,
        spec::{FieldSpec, SpecInference, json_schema},
    },
    models::{DatasetInfo, DatasetStatus},
    store,
};

const BATCH_SIZE: usize = 1000;
const MAX_INVALID_SAMPLES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportMode {
    /// The import holds only the new file's rows.
    Replace,
    /// New rows replace current rows with the same primary key; other current rows are kept.
    Upsert,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImportRequest {
    /// Defaults to `upsert` when the mapping has a primary key and the collection
    /// already has imported data, otherwise `replace`.
    pub mode: Option<ImportMode>,
}

/// Declared per-field rules checked on every row.
struct RowRules {
    primary_key: Vec<String>,
    checks: Vec<(String, Option<FieldType>, bool)>,
    searchable: Vec<String>,
}

impl RowRules {
    fn new(doc: &MappingDoc, resolved: &ResolvedMapping, spec: &[FieldSpec]) -> Self {
        let mut checks: Vec<(String, Option<FieldType>, bool)> = resolved
            .field_paths
            .iter()
            .map(|(i, path)| (path.clone(), doc.fields[*i].field_type, doc.fields[*i].required))
            .collect();
        for path in &doc.primary_key {
            if !checks.iter().any(|(p, ..)| p == path) {
                checks.push((path.clone(), None, true));
            }
        }
        RowRules {
            primary_key: doc.primary_key.clone(),
            checks,
            searchable: spec.iter().filter(|s| s.searchable).map(|s| s.path.clone()).collect(),
        }
    }

    fn primary_key(&self, data: &Map<String, Value>) -> Option<String> {
        if self.primary_key.is_empty() {
            return None;
        }
        let parts: Option<Vec<String>> = self
            .primary_key
            .iter()
            .map(|path| match value_at(data, path)? {
                Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
                Value::Number(n) => Some(n.to_string()),
                Value::Bool(b) => Some(b.to_string()),
                _ => None,
            })
            .collect();
        parts.map(|p| p.join("|"))
    }

    fn errors(&self, data: &Map<String, Value>) -> Vec<Value> {
        let mut errors = vec![];
        for (path, field_type, required) in &self.checks {
            let blank = |s: &String| s.trim().is_empty();
            match value_at(data, path) {
                None | Some(Value::Null) => {
                    if *required {
                        errors.push(json!({ "path": path, "message": "a value is required" }));
                    }
                }
                Some(Value::String(s)) if blank(s) => {
                    if *required {
                        errors.push(json!({ "path": path, "message": "a value is required" }));
                    }
                }
                Some(value) => {
                    let ok = match field_type {
                        Some(FieldType::Integer) => value.is_i64() || value.is_u64(),
                        Some(FieldType::Number) => value.is_number(),
                        Some(FieldType::Boolean) => value.is_boolean(),
                        Some(FieldType::String | FieldType::Date | FieldType::Datetime | FieldType::Time) => value.is_string(),
                        Some(FieldType::Array) => value.is_array(),
                        Some(FieldType::Auto) | None => true,
                    };
                    if !ok {
                        let expected = format!("{:?}", field_type.unwrap()).to_lowercase();
                        errors.push(json!({ "path": path, "message": format!("expected {expected}, found {value}") }));
                    }
                }
            }
        }
        errors
    }

    fn search_text(&self, data: &Map<String, Value>) -> Option<String> {
        let parts: Vec<String> = self
            .searchable
            .iter()
            .filter_map(|path| match value_at(data, path)? {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                Value::Array(items) => Some(items.iter().filter_map(|i| i.as_str()).collect::<Vec<_>>().join(" ")),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

pub fn value_at<'a>(data: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut segments = path.split('.');
    let mut value = data.get(segments.next()?)?;
    for segment in segments {
        value = value.as_object()?.get(segment)?;
    }
    Some(value)
}

#[derive(Default)]
struct Progress {
    batch: Vec<Value>,
    next_row: i64,
    inserted: i64,
    invalid: i64,
    invalid_samples: Vec<Value>,
    seen_keys: HashSet<String>,
    inference: SpecInference,
}

impl Progress {
    fn push(&mut self, data: Map<String, Value>, rules: &RowRules, pk: Option<String>, mut errors: Vec<Value>) {
        self.next_row += 1;
        if let Some(pk) = &pk {
            if !self.seen_keys.insert(pk.clone()) {
                errors.push(json!({ "path": rules.primary_key.join("|"), "message": format!("duplicate primary key {pk}") }));
            }
        } else if !rules.primary_key.is_empty() {
            errors.push(json!({ "path": rules.primary_key.join("|"), "message": "missing primary key value" }));
        }
        let valid = errors.is_empty();
        if valid {
            self.inference.observe(&data);
        } else {
            self.invalid += 1;
            if self.invalid_samples.len() < MAX_INVALID_SAMPLES {
                self.invalid_samples.push(json!({ "row_num": self.next_row, "errors": errors }));
            }
        }
        let mut row = json!({ "row_num": self.next_row, "valid": valid, "errors": errors });
        if let Some(pk) = pk {
            row["pk"] = json!(pk);
        }
        if let Some(text) = rules.search_text(&data) {
            row["search_text"] = json!(text);
        }
        row["data"] = Value::Object(data);
        self.batch.push(row);
    }

    async fn flush(&mut self, db: &Database, dataset_id: &str) -> ApiResult<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.batch);
        self.inserted += rows.len() as i64;
        run(
            db,
            "LET $ds = type::record('datasets', $id);
            INSERT INTO dataset_rows $rows.map(|$r| $r.extend({ dataset: $ds })) RETURN NONE;",
            json!({ "id": dataset_id, "rows": rows }),
        )
        .await?;
        Ok(())
    }
}

/// Runs the import; on failure the dataset is marked `failed` with the reason.
pub async fn import(db: &Database, dataset: &DatasetInfo, request: ImportRequest) -> ApiResult<Value> {
    if dataset.status == DatasetStatus::Importing {
        return Err(ApiError::Conflict("this dataset is already being imported".into()));
    }
    if dataset.status == DatasetStatus::Expired {
        return Err(ApiError::Conflict("the mapping window for this upload has closed; upload the file again".into()));
    }
    let path = dataset.require_source()?;
    let doc = dataset.doc();
    let sheet_index = dataset.sheet_index();

    // natural keys first: the mapping resolves against them
    let preview = reader::preview(&path, sheet_index, &doc, 1).await?.map_err(ApiError::InvalidMapping)?;
    let resolved = doc.resolve(&preview.natural_keys).map_err(ApiError::InvalidMapping)?;
    let mut opts = base_options(&path, sheet_index, &doc.source).map_err(ApiError::InvalidMapping)?;
    resolved.apply(&mut opts, &doc);

    let features = store::plan_features(db, &dataset.owner).await?;
    if !doc.primary_key.is_empty() && !features.iter().any(|f| f == "links") {
        return Err(ApiError::Forbidden("primary keys need a plan with the 'links' feature".into()));
    }
    let previous = dataset.current_id.clone().filter(|id| *id != dataset.id);
    let mode = match request.mode {
        Some(ImportMode::Upsert) if doc.primary_key.is_empty() => {
            return Err(ApiError::Unprocessable("upsert needs a primary_key in the mapping".into()));
        }
        Some(mode) => mode,
        None if !doc.primary_key.is_empty() && previous.is_some() => ImportMode::Upsert,
        None => ImportMode::Replace,
    };
    let max_rows = run_last(
        db,
        "RETURN fn::effective_plan(type::record('principals', $owner)).limits.max_rows_per_import;",
        json!({ "owner": dataset.owner }),
    )
    .await?
    .as_u64()
    .unwrap_or(10_000) as u32;
    let max_rows = doc.source.max_rows.map_or(max_rows, |m| m.min(max_rows));

    run(
        db,
        "LET $ds = type::record('datasets', $id);
        UPDATE $ds SET status = 'importing', error = NONE RETURN NONE;
        DELETE dataset_rows WHERE dataset = $ds;",
        json!({ "id": dataset.id }),
    )
    .await?;

    let started = Instant::now();
    let rules = RowRules::new(&doc, &resolved, dataset.field_spec());
    let outcome = run_import(db, dataset, &doc, &resolved, &rules, opts, max_rows, mode, previous.as_deref()).await;
    match outcome {
        Ok(mut summary) => {
            summary["duration_ms"] = json!(started.elapsed().as_millis() as u64);
            if let Some(upload_id) = &dataset.upload_id {
                store::purge_upload_if_done(db, upload_id).await?;
            }
            Ok(summary)
        }
        Err(err) => {
            let message = err.to_string();
            run(
                db,
                "LET $ds = type::record('datasets', $id);
                DELETE dataset_rows WHERE dataset = $ds;
                UPDATE $ds SET status = 'failed', error = $error RETURN NONE;",
                json!({ "id": dataset.id, "error": message }),
            )
            .await?;
            Err(err)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_import(
    db: &Database,
    dataset: &DatasetInfo,
    doc: &MappingDoc,
    resolved: &ResolvedMapping,
    rules: &RowRules,
    opts: spreadsheet_to_json::OptionSet,
    max_rows: u32,
    mode: ImportMode,
    previous: Option<&str>,
) -> ApiResult<Value> {
    let mut progress = Progress::default();
    // scan one row past the plan limit to tell "exactly at the limit" from "over it"
    let (rx, reader_task) = reader::stream(opts, max_rows.saturating_add(1));
    while let Ok(row) = rx.recv().await {
        if progress.next_row >= max_rows as i64 {
            drop(rx);
            let _ = reader_task.await;
            return Err(ApiError::Rejected(format!(
                "the worksheet has more than {max_rows} rows, the most your plan allows per import"
            )));
        }
        let data: Map<String, Value> = row.into_iter().collect();
        let pk = rules.primary_key(&data);
        let errors = rules.errors(&data);
        progress.push(data, rules, pk, errors);
        if progress.batch.len() >= BATCH_SIZE {
            progress.flush(db, &dataset.id).await?;
        }
    }
    reader_task.await.map_err(|e| ApiError::Internal(format!("reader task failed: {e}")))??;
    progress.flush(db, &dataset.id).await?;
    let imported = progress.inserted;

    let mut retained = 0;
    if let (ImportMode::Upsert, Some(previous)) = (mode, previous) {
        retained = carry_over(db, &dataset.id, previous, rules, &mut progress).await?;
    }

    // types inferred over every valid row replace the 30-row preview guess
    let spec = progress.inference.finish(doc, Some(resolved));
    let spec = merge_flags(spec, dataset.field_spec());
    let definition = json_schema(&spec, doc, Some(resolved));
    let pk: Value = if doc.primary_key.is_empty() { Value::Null } else { json!(doc.primary_key) };
    run(
        db,
        "LET $ds = type::record('datasets', $id);
        UPDATE $ds SET status = 'imported', num_rows = $num_rows, num_invalid = $num_invalid,
            imported_at = time::now(), error = NONE RETURN NONE;
        LET $v = $ds.schema_version;
        IF $v.status = 'draft' {
            UPDATE $v SET definition = $definition, field_spec = $spec, status = 'published' RETURN NONE;
        };
        UPDATE $ds.collection SET schema = $v.schema,
            primary_key = IF $pk { $pk } ELSE { NONE } RETURN NONE;",
        json!({
            "id": dataset.id, "num_rows": progress.inserted, "num_invalid": progress.invalid,
            "definition": definition, "spec": spec, "pk": pk,
        }),
    )
    .await?;

    let dataset = store::get_dataset(db, &dataset.id).await?;
    Ok(json!({
        "dataset": dataset.summary(),
        "mode": if mode == ImportMode::Upsert { "upsert" } else { "replace" },
        "rows_imported": imported,
        "rows_retained": retained,
        "num_rows": progress.inserted,
        "num_invalid": progress.invalid,
        "invalid_rows": progress.invalid_samples,
        "field_spec": dataset.field_spec(),
    }))
}

/// Keeps the filterable/sortable/searchable choices already made for existing paths.
fn merge_flags(mut spec: Vec<FieldSpec>, configured: &[FieldSpec]) -> Vec<FieldSpec> {
    for entry in &mut spec {
        if let Some(prev) = configured.iter().find(|c| c.path == entry.path) {
            entry.searchable = prev.searchable;
            entry.filterable = prev.filterable;
            entry.sortable = prev.sortable;
            entry.required |= prev.required;
        }
    }
    spec
}

/// Copies the previous import's rows whose primary key the new file didn't supply.
async fn carry_over(
    db: &Database,
    dataset_id: &str,
    previous: &str,
    rules: &RowRules,
    progress: &mut Progress,
) -> ApiResult<i64> {
    let mut retained = 0;
    let mut after = 0i64;
    loop {
        let page = run_last(
            db,
            "SELECT row_num, data FROM dataset_rows
                WHERE dataset = type::record('datasets', $old) AND row_num > $after AND valid = true
                ORDER BY row_num LIMIT $limit;",
            json!({ "old": previous, "after": after, "limit": BATCH_SIZE }),
        )
        .await?;
        let rows = page.as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            after = row["row_num"].as_i64().unwrap_or(after);
            let Some(data) = row["data"].as_object() else { continue };
            // keys are recomputed, so a changed primary_key definition still dedupes correctly
            let Some(pk) = rules.primary_key(data) else { continue };
            if progress.seen_keys.contains(&pk) {
                continue;
            }
            let errors = rules.errors(data);
            progress.push(data.clone(), rules, Some(pk), errors);
            retained += 1;
        }
        progress.flush(db, dataset_id).await?;
    }
    Ok(retained)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sheet::mapping::MappingDoc;

    #[test]
    fn computes_keys_errors_and_search_text() {
        let doc: MappingDoc = serde_json::from_value(json!({
            "primary_key": ["sku", "size.code"],
            "fields": [
                { "source": "qty", "type": "integer", "required": true },
                { "source": "name" }
            ]
        }))
        .unwrap();
        let keys = vec!["sku".to_string(), "qty".to_string(), "name".to_string()];
        let resolved = doc.resolve(&keys).unwrap();
        let spec = vec![FieldSpec {
            path: "name".into(),
            field_type: "string".into(),
            required: false,
            searchable: true,
            filterable: true,
            sortable: true,
        }];
        let rules = RowRules::new(&doc, &resolved, &spec);
        let row = |v: Value| v.as_object().unwrap().clone();

        let good = row(json!({ "sku": "A1", "size": { "code": 3 }, "qty": 2, "name": "Red widget" }));
        assert_eq!(rules.primary_key(&good).as_deref(), Some("A1|3"));
        assert!(rules.errors(&good).is_empty());
        assert_eq!(rules.search_text(&good).as_deref(), Some("Red widget"));

        let bad = row(json!({ "sku": " ", "qty": 2.5 }));
        assert_eq!(rules.primary_key(&bad), None);
        let paths: Vec<String> = rules.errors(&bad).iter().map(|e| e["path"].as_str().unwrap().to_string()).collect();
        assert_eq!(paths, vec!["qty", "sku", "size.code"]);
    }
}
