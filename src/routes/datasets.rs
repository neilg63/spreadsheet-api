use std::path::Path as FsPath;

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{FromRequest, Multipart, Path, Query, Request, State},
    http::header,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::AppState;
use crate::{
    config::config,
    db::run,
    error::{ApiError, ApiResult},
    auth::{AccountKind, Caller},
    importer::{self, ImportRequest},
    query::{self, QueryParams},
    sheet::{
        mapping::{MappingDoc, SourceOptions, mapping_error},
        reader::{self, SheetPreview},
        spec::{self, FieldSpec},
    },
    models::{CollectionInfo, DatasetInfo, DatasetStatus, Mapping, Me, VersionStatus},
    store,
};

fn a1_letter(mut index: usize) -> String {
    let mut letters = vec![];
    loop {
        letters.push((b'A' + (index % 26) as u8) as char);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    letters.iter().rev().collect()
}

/// One entry per source column, for building column pickers.
fn columns(preview: &SheetPreview) -> Vec<Value> {
    let inferred = spec::infer(&preview.natural_rows, &MappingDoc::default(), None);
    preview
        .natural_keys
        .iter()
        .enumerate()
        .map(|(i, key)| {
            let mut samples: Vec<&Value> = vec![];
            for row in &preview.natural_rows {
                match row.get(key) {
                    Some(v) if !v.is_null() && v != "" && !samples.contains(&v) => samples.push(v),
                    _ => {}
                }
                if samples.len() == 3 {
                    break;
                }
            }
            json!({
                "index": i + 1,
                "letter": a1_letter(i),
                "key": key,
                "type": inferred.iter().find(|s| s.path == *key).map(|s| s.field_type.as_str()).unwrap_or("string"),
                "samples": samples,
            })
        })
        .collect()
}

/// The mapping-screen view of one worksheet.
fn sheet_overview(dataset_id: &str, sheet: Option<&str>, sheet_index: u32, preview: &SheetPreview, doc: &MappingDoc, field_spec: &[FieldSpec]) -> Value {
    json!({
        "dataset_id": dataset_id,
        "sheet": sheet,
        "sheet_index": sheet_index,
        "header_row": preview.header_row,
        "data_start": preview.data_start,
        "row_count": preview.row_count,
        "columns": columns(preview),
        "mapping": doc,
        "mapping_errors": preview.errors,
        "field_spec": field_spec,
        "rows": preview.rows,
    })
}

fn preview_spec(preview: &SheetPreview, doc: &MappingDoc) -> (Vec<FieldSpec>, Value) {
    let resolved = doc.resolve(&preview.natural_keys).ok();
    let field_spec = spec::infer(&preview.rows, doc, resolved.as_ref());
    let definition = spec::json_schema(&field_spec, doc, resolved.as_ref());
    (field_spec, definition)
}

struct SavedFile {
    id: String,
    storage_key: String,
    filename: String,
    content_type: Option<String>,
    size: u64,
    source_kind: &'static str,
}

const UNSUPPORTED: &str = "unsupported file type; use xlsx, xlsm, xls, xlsb, ods, csv, tsv or semicolon-separated values";

/// Streams a source file into UPLOAD_DIR under a generated name, enforcing `max_bytes`.
struct Incoming {
    file: SavedFile,
    path: std::path::PathBuf,
    out: tokio::fs::File,
    max_bytes: usize,
}

impl Incoming {
    async fn create(filename: &str, content_type: Option<String>, source_kind: &'static str, max_bytes: usize) -> ApiResult<Self> {
        let filename = filename.trim().to_string();
        let ext = reader::storage_extension(&filename).ok_or_else(|| ApiError::Unprocessable(UNSUPPORTED.into()))?;
        let id = uuid::Uuid::now_v7().to_string();
        let storage_key = format!("{id}.{ext}");
        tokio::fs::create_dir_all(&config().upload_dir).await?;
        let path = config().upload_dir.join(&storage_key);
        let out = tokio::fs::File::create(&path).await?;
        let file = SavedFile { id, storage_key, filename, content_type, size: 0, source_kind };
        Ok(Incoming { file, path, out, max_bytes })
    }

    fn too_large(&self) -> ApiError {
        ApiError::Unprocessable(format!("the file exceeds the {} byte upload limit", self.max_bytes))
    }

    async fn write(&mut self, chunk: &[u8]) -> ApiResult<()> {
        self.file.size += chunk.len() as u64;
        if self.file.size > self.max_bytes as u64 {
            return Err(self.too_large());
        }
        self.out.write_all(chunk).await?;
        Ok(())
    }

    async fn finish(mut self) -> ApiResult<SavedFile> {
        self.out.flush().await?;
        if self.file.size == 0 {
            let _ = tokio::fs::remove_file(&self.path).await;
            return Err(ApiError::BadRequest("the uploaded file is empty".into()));
        }
        Ok(self.file)
    }

    async fn discard(self) {
        let _ = tokio::fs::remove_file(&self.path).await;
    }
}

async fn save_file_field(mut field: axum::extract::multipart::Field<'_>, max_bytes: usize) -> ApiResult<SavedFile> {
    let filename = field
        .file_name()
        .map(str::to_string)
        .filter(|n| !n.trim().is_empty())
        .ok_or_else(|| ApiError::BadRequest("the file field needs a file name".into()))?;
    let mut incoming = Incoming::create(&filename, field.content_type().map(str::to_string), "file", max_bytes).await?;
    let result: ApiResult<()> = async {
        while let Some(chunk) = field.chunk().await.map_err(|e| ApiError::BadRequest(format!("upload interrupted: {e}")))? {
            incoming.write(&chunk).await?;
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => incoming.finish().await,
        Err(err) => {
            incoming.discard().await;
            Err(err)
        }
    }
}

/// JSON body for service-mode uploads that don't come as multipart.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteUpload {
    /// Download from this http(s) URL (hosts limited by URL_UPLOAD_HOSTS when set).
    pub url: Option<String>,
    /// Copy this file from SHARED_UPLOAD_DIR (relative to it, or absolute inside it).
    pub path: Option<String>,
    /// Name to record and to take the format from, when the URL or path doesn't end in one.
    pub filename: Option<String>,
    #[serde(default)]
    pub source: SourceOptions,
}

async fn fetch_url(state: &AppState, url: &str, filename: Option<&str>, max_bytes: usize) -> ApiResult<SavedFile> {
    let parsed = reqwest::Url::parse(url).map_err(|e| ApiError::BadRequest(format!("invalid url: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::BadRequest("only http and https URLs can be fetched".into()));
    }
    let hosts = &state.auth.url_hosts;
    if !hosts.is_empty() && !parsed.host_str().is_some_and(|h| hosts.iter().any(|a| a.eq_ignore_ascii_case(h))) {
        return Err(ApiError::Forbidden("that host is not in URL_UPLOAD_HOSTS".into()));
    }
    let name = filename
        .map(str::to_string)
        .or_else(|| parsed.path_segments().and_then(|mut s| s.next_back()).map(str::to_string))
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ApiError::BadRequest("add a filename: the URL doesn't end in one".into()))?;
    let mut response = state
        .fetch
        .get(parsed)
        .send()
        .await
        .map_err(|e| ApiError::BadGateway(format!("could not fetch the file: {e}")))?;
    if !response.status().is_success() {
        return Err(ApiError::BadGateway(format!("fetching the file returned {}", response.status())));
    }
    if response.content_length().is_some_and(|len| len > max_bytes as u64) {
        return Err(ApiError::Unprocessable(format!("the file exceeds the {max_bytes} byte upload limit")));
    }
    let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
    let mut incoming = Incoming::create(&name, content_type, "url", max_bytes).await?;
    let result: ApiResult<()> = async {
        while let Some(chunk) = response.chunk().await.map_err(|e| ApiError::BadGateway(format!("download interrupted: {e}")))? {
            incoming.write(&chunk).await?;
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => incoming.finish().await,
        Err(err) => {
            incoming.discard().await;
            Err(err)
        }
    }
}

/// Copies a file the primary API left on the shared volume. The original stays where it is.
async fn copy_shared(state: &AppState, requested: &str, filename: Option<&str>, max_bytes: usize) -> ApiResult<SavedFile> {
    let root = state
        .auth
        .shared_dir
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("path uploads need SHARED_UPLOAD_DIR".into()))?;
    let root = tokio::fs::canonicalize(root).await.map_err(|e| ApiError::Internal(format!("SHARED_UPLOAD_DIR: {e}")))?;
    let candidate = if FsPath::new(requested).is_absolute() { std::path::PathBuf::from(requested) } else { root.join(requested) };
    let not_found = || ApiError::NotFound("no such file in the shared upload directory".into());
    // canonicalize resolves .. and symlinks, so the check below can't be sidestepped
    let source = tokio::fs::canonicalize(&candidate).await.map_err(|_| not_found())?;
    if !source.starts_with(&root) || !source.is_file() {
        return Err(not_found());
    }
    let size = tokio::fs::metadata(&source).await?.len();
    if size > max_bytes as u64 {
        return Err(ApiError::Unprocessable(format!("the file exceeds the {max_bytes} byte upload limit")));
    }
    let name = filename.map(str::to_string).or_else(|| source.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_default();
    let incoming = Incoming::create(&name, None, "transfer", max_bytes).await?;
    let (target, mut file) = (incoming.path.clone(), incoming.file);
    drop(incoming.out);
    tokio::fs::copy(&source, &target).await?;
    file.size = size;
    if size == 0 {
        let _ = tokio::fs::remove_file(&target).await;
        return Err(ApiError::BadRequest("the shared file is empty".into()));
    }
    Ok(file)
}

/// POST /datasets/upload. Multipart: `file`, plus optional `source` (JSON source options
/// applied to every worksheet, e.g. `{"decimal_comma": true}`). In service mode, also a
/// JSON body `{"url": ...}` or `{"path": ...}` with optional `filename` and `source`.
pub async fn upload(State(state): State<AppState>, caller: Caller, request: Request) -> ApiResult<Json<Value>> {
    let allowance = store::touch_principal(&state.db, &caller.me).await?;
    let max_bytes = state.max_upload_bytes(&caller);
    let is_json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    let (file, source) = if is_json {
        if caller.kind != AccountKind::Service {
            return Err(ApiError::Forbidden("URL and path uploads are only available in service mode; send multipart".into()));
        }
        let Json(body) = Json::<RemoteUpload>::from_request(request, &state)
            .await
            .map_err(|e| ApiError::BadRequest(format!("invalid upload request: {e}")))?;
        let file = match (&body.url, &body.path) {
            (Some(url), None) => fetch_url(&state, url, body.filename.as_deref(), max_bytes).await?,
            (None, Some(path)) => copy_shared(&state, path, body.filename.as_deref(), max_bytes).await?,
            _ => return Err(ApiError::BadRequest("send either url or path".into())),
        };
        (file, body.source)
    } else {
        let mut multipart = Multipart::from_request(request, &state)
            .await
            .map_err(|e| ApiError::BadRequest(format!("send multipart/form-data with a 'file' field: {e}")))?;
        read_multipart(&mut multipart, max_bytes).await?
    };
    match process_upload(&state, &caller, &file, &source).await {
        Ok(mut body) => {
            body["allowance"] = json!(allowance);
            Ok(Json(body))
        }
        Err(err) => {
            store::remove_upload_file(&file.storage_key);
            let _ = run(
                &state.db,
                "UPDATE type::record('uploads', $id) SET purged_at = time::now() WHERE purged_at = NONE;",
                json!({ "id": file.id }),
            )
            .await;
            Err(err)
        }
    }
}

async fn read_multipart(multipart: &mut Multipart, max_bytes: usize) -> ApiResult<(SavedFile, SourceOptions)> {
    let mut saved: Option<SavedFile> = None;
    let mut source = SourceOptions::default();
    let mut failure: Option<ApiError> = None;
    while let Some(field) = multipart.next_field().await.map_err(|e| ApiError::BadRequest(format!("invalid multipart body: {e}")))? {
        match field.name() {
            Some("file") if saved.is_none() && failure.is_none() => match save_file_field(field, max_bytes).await {
                Ok(file) => saved = Some(file),
                Err(err) => failure = Some(err),
            },
            Some("source") => {
                let text = field.text().await.map_err(|e| ApiError::BadRequest(e.to_string()))?;
                if !text.trim().is_empty() {
                    match serde_json::from_str(&text) {
                        Ok(parsed) => source = parsed,
                        Err(e) => failure = Some(ApiError::InvalidMapping(vec![mapping_error("source", e.to_string())])),
                    }
                }
            }
            _ => {}
        }
    }
    match (saved, failure) {
        (Some(file), None) => Ok((file, source)),
        (saved, Some(err)) => {
            if let Some(file) = saved {
                store::remove_upload_file(&file.storage_key);
            }
            Err(err)
        }
        (None, None) => Err(ApiError::BadRequest("send the spreadsheet in a multipart field named 'file'".into())),
    }
}

async fn process_upload(state: &AppState, caller: &Caller, file: &SavedFile, source: &SourceOptions) -> ApiResult<Value> {
    let me = &caller.me;
    let path = config().upload_dir.join(&file.storage_key);
    let sheets = reader::list_sheets(&path).await?;
    let upload = store::NewUpload {
        id: &file.id,
        filename: &file.filename,
        storage_key: &file.storage_key,
        content_type: file.content_type.as_deref(),
        size: file.size,
        source_kind: file.source_kind,
    };
    store::create_upload(&state.db, me, &upload, &sheets).await?;
    let stem = FsPath::new(&file.filename).file_stem().and_then(|s| s.to_str()).unwrap_or("upload").trim().to_string();
    let targets: Vec<(u32, Option<&str>)> = if sheets.is_empty() {
        vec![(0, None)]
    } else {
        sheets.iter().enumerate().map(|(i, s)| (i as u32, Some(s.as_str()))).collect()
    };

    let mut datasets = vec![];
    let mut skipped = vec![];
    for (index, sheet) in targets {
        let name = match sheet {
            Some(sheet) => format!("{stem} / {sheet}"),
            None => stem.clone(),
        };
        match add_sheet(state, caller, file, &path, index, sheet, &name, source).await {
            Ok(Some(overview)) => datasets.push(overview),
            Ok(None) => skipped.push(json!({ "sheet": sheet, "sheet_index": index, "reason": "the worksheet is empty" })),
            Err(err) => skipped.push(json!({ "sheet": sheet, "sheet_index": index, "reason": err.to_string() })),
        }
    }
    if datasets.is_empty() {
        let reasons: Vec<String> = skipped.iter().filter_map(|s| s["reason"].as_str().map(str::to_string)).collect();
        return Err(ApiError::Unprocessable(format!("no worksheet could be read: {}", reasons.join("; "))));
    }
    Ok(json!({
        "upload": { "id": file.id, "filename": file.filename, "size": file.size, "source_kind": file.source_kind, "sheets": sheets },
        "datasets": datasets,
        "skipped": skipped,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn add_sheet(
    state: &AppState,
    caller: &Caller,
    file: &SavedFile,
    path: &FsPath,
    index: u32,
    sheet: Option<&str>,
    name: &str,
    source: &SourceOptions,
) -> ApiResult<Option<Value>> {
    let (db, me) = (&state.db, &caller.me);
    let existing = store::find_collection(db, me, name).await?;
    let reused = match &existing {
        Some(CollectionInfo { schema_id: Some(schema_id), .. }) => store::latest_version(db, schema_id).await?,
        _ => None,
    };
    let mut doc = match &reused {
        Some(version) => version.doc(),
        None => MappingDoc { source: source.clone(), ..Default::default() },
    };
    let preview = reader::preview(path, index, &doc, config().preview_rows).await?.map_err(ApiError::InvalidMapping)?;
    if preview.natural_keys.is_empty() || preview.row_count == 0 {
        return Ok(None);
    }
    if reused.is_none() && !doc.source.no_header {
        // pin what was detected, so the mapping screen shows it and later imports don't re-guess
        doc.source.header_row = doc.source.header_row.or(preview.header_row.map(|r| r as u32));
        if preview.data_start > preview.header_row.unwrap_or(0) + 1 {
            doc.source.data_start = doc.source.data_start.or(Some(preview.data_start as u32));
        }
    }
    let (field_spec, definition) = preview_spec(&preview, &doc);
    let collection = match existing {
        Some(collection) => collection,
        None => store::create_collection(db, me, name).await?,
    };
    let schema_id = store::ensure_schema(db, me, &collection, &doc).await?;
    let version = match reused {
        Some(version) => version,
        None => {
            let mapping = Mapping::content(&doc, sheet, preview.header_row);
            store::create_version(db, &schema_id, &definition, &field_spec, mapping).await?
        }
    };
    let dataset_id = uuid::Uuid::now_v7().to_string();
    store::create_dataset(db, me, &dataset_id, &collection.id, &file.id, sheet, &version.id, caller.ip.as_deref()).await?;
    let mut overview = sheet_overview(&dataset_id, sheet, index, &preview, &doc, &field_spec);
    overview["collection"] = json!({ "id": collection.id, "name": collection.name, "reused_mapping": version.status == VersionStatus::Published });
    overview["schema_version"] = json!({ "id": version.id, "version": version.version, "status": version.status });
    Ok(Some(overview))
}

/// GET /datasets: the caller's datasets, newest first.
pub async fn list(State(state): State<AppState>, caller: Caller, Query(params): Query<QueryParams>) -> ApiResult<Json<Value>> {
    // a guest id is a network + browser hash, too weak to show anyone an upload history
    if caller.is_guest() {
        return Err(ApiError::Forbidden("sign in to see your upload history".into()));
    }
    let me = caller.me;
    store::touch_principal(&state.db, &me).await?;
    let limit = params.limit.unwrap_or(config().default_page_size).clamp(1, config().max_page_size);
    let start = params.start.unwrap_or_else(|| params.page.unwrap_or(1).saturating_sub(1) * limit);
    let (total, datasets) = store::list_datasets(&state.db, &me, start, limit).await?;
    Ok(Json(json!({ "total": total, "start": start, "limit": limit, "datasets": datasets })))
}

/// GET /datasets/{id}: imported rows with filtering, sorting and pagination.
pub async fn show(State(state): State<AppState>, Path(id): Path<String>, Query(params): Query<QueryParams>) -> ApiResult<Json<Value>> {
    let dataset = store::get_dataset(&state.db, &id).await?;
    let built = query::build(&params, dataset.field_spec(), config().default_page_size, config().max_page_size)?;
    let features = store::plan_features(&state.db, &dataset.owner).await?;
    for (used, feature, what) in [
        (built.uses_filter, "filter", "Filtering"),
        (built.uses_sort, "sort", "Sorting"),
        (built.uses_search, "search", "Full-text search"),
    ] {
        if used && !features.iter().any(|f| f == feature) {
            return Err(ApiError::Forbidden(format!("{what} is not available on this dataset owner's plan")));
        }
    }
    let mut vars = built.vars;
    vars.insert("dataset".into(), json!(dataset.id));
    let sql = format!("{}\n{}", built.count_sql, built.rows_sql);
    let mut results = run(&state.db, &sql, Value::Object(vars)).await?;
    let rows = results.pop().unwrap_or_else(|| json!([]));
    let total = results.pop().and_then(|t| t.as_u64()).unwrap_or(0);
    Ok(Json(json!({
        "dataset": dataset.summary(),
        "fields": dataset.field_spec(),
        "total": total,
        "start": built.start,
        "limit": built.limit,
        "rows": rows,
    })))
}

#[derive(Debug, Default, Deserialize)]
pub struct ExportParams {
    /// `json` (one array, the default) or `jsonl` (one row per line).
    pub format: Option<String>,
    pub include_invalid: Option<bool>,
}

/// GET /datasets/{id}/export: every imported row as a download, streamed page by page.
pub async fn export(State(state): State<AppState>, Path(id): Path<String>, Query(params): Query<ExportParams>) -> ApiResult<Response> {
    let dataset = store::get_dataset(&state.db, &id).await?;
    if !matches!(dataset.status, DatasetStatus::Imported | DatasetStatus::Superseded) {
        return Err(ApiError::Conflict("this dataset has not been imported yet".into()));
    }
    let jsonl = match params.format.as_deref().unwrap_or("json") {
        "json" => false,
        "jsonl" | "ndjson" => true,
        other => return Err(ApiError::BadRequest(format!("unknown export format '{other}'; use json or jsonl"))),
    };
    let only_valid = !params.include_invalid.unwrap_or(false);
    let (tx, rx) = async_channel::bounded::<Result<Bytes, std::io::Error>>(8);
    let db = state.db.clone();
    let dataset_id = dataset.id.clone();
    tokio::spawn(async move {
        let sql = format!(
            "SELECT row_num, data FROM dataset_rows WHERE dataset = type::record('datasets', $id) AND row_num > $after {}
                ORDER BY row_num LIMIT 1000;",
            if only_valid { "AND valid = true" } else { "" }
        );
        let (mut after, mut first) = (0i64, true);
        if !jsonl && tx.send(Ok(Bytes::from_static(b"["))).await.is_err() {
            return;
        }
        loop {
            let page = match crate::db::run_last(&db, &sql, json!({ "id": dataset_id, "after": after })).await {
                Ok(Value::Array(rows)) if !rows.is_empty() => rows,
                Ok(_) => break,
                Err(err) => {
                    // an error mid-stream can only abort the download
                    let _ = tx.send(Err(std::io::Error::other(err.to_string()))).await;
                    return;
                }
            };
            let mut chunk = String::new();
            for row in &page {
                after = row["row_num"].as_i64().unwrap_or(after);
                if !jsonl && !first {
                    chunk.push(',');
                }
                first = false;
                chunk.push_str(&row["data"].to_string());
                if jsonl {
                    chunk.push('\n');
                }
            }
            if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                return; // the client went away
            }
        }
        if !jsonl {
            let _ = tx.send(Ok(Bytes::from_static(b"]"))).await;
        }
    });
    let name: String = dataset
        .collection_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let (content_type, ext) = if jsonl { ("application/x-ndjson", "jsonl") } else { ("application/json", "json") };
    Ok((
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}.{ext}\"")),
        ],
        Body::from_stream(rx),
    )
        .into_response())
}

/// GET /datasets/{id}/preview: re-reads the source file with the saved mapping.
pub async fn preview(State(state): State<AppState>, caller: Caller, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let me = caller.me;
    let dataset = store::get_own_dataset(&state.db, &me, &id).await?;
    let path = dataset.require_source()?;
    let doc = dataset.doc();
    let preview = reader::preview(&path, dataset.sheet_index(), &doc, config().preview_rows)
        .await?
        .map_err(ApiError::InvalidMapping)?;
    let (field_spec, _) = preview_spec(&preview, &doc);
    let overview = sheet_overview(&dataset.id, dataset.sheet.as_deref(), dataset.sheet_index(), &preview, &doc, &field_spec);
    Ok(Json(json!({ "dataset": dataset.summary(), "preview": overview })))
}

#[derive(Debug, Default, Deserialize)]
pub struct ConfigureParams {
    /// Validate and preview without saving.
    pub dry_run: Option<bool>,
}

/// PATCH /datasets/{id}/configure: replaces the dataset's mapping document, validates it
/// against the source file, saves it as a schema version and returns a fresh preview.
pub async fn configure(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Query(params): Query<ConfigureParams>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let (db, me) = (&state.db, caller.me);
    store::touch_principal(db, &me).await?;
    let dataset = store::get_own_dataset(db, &me, &id).await?;
    if dataset.status == DatasetStatus::Importing {
        return Err(ApiError::Conflict("this dataset is being imported; try again when it finishes".into()));
    }
    let doc: MappingDoc = serde_json::from_slice(&body).map_err(|e| ApiError::InvalidMapping(vec![mapping_error("", e.to_string())]))?;
    let features = store::plan_features(db, &dataset.owner).await?;
    if !features.iter().any(|f| f == "custom_schema") {
        // guests may fix how the sheet is read (header rows, number formats ...) but not reshape it
        let locked: Vec<_> = doc
            .customised_parts()
            .into_iter()
            .map(|part| mapping_error(part, "sign in to customise the field mapping"))
            .collect();
        if !locked.is_empty() {
            return Err(ApiError::InvalidMapping(locked));
        }
    }
    if !doc.primary_key.is_empty() && !features.iter().any(|f| f == "links") {
        return Err(ApiError::Forbidden("primary keys need a plan with the 'links' feature".into()));
    }
    let path = dataset.require_source()?;
    let preview = reader::preview(&path, dataset.sheet_index(), &doc, config().preview_rows)
        .await?
        .map_err(ApiError::InvalidMapping)?;
    if !preview.errors.is_empty() {
        return Err(ApiError::InvalidMapping(preview.errors));
    }
    let (field_spec, definition) = preview_spec(&preview, &doc);
    let overview = sheet_overview(&dataset.id, dataset.sheet.as_deref(), dataset.sheet_index(), &preview, &doc, &field_spec);
    if params.dry_run.unwrap_or(false) {
        return Ok(Json(json!({ "dataset": dataset.summary(), "saved": false, "definition": definition, "preview": overview })));
    }

    save_mapping(&state, &me, &dataset, &doc, &preview, &field_spec, &definition).await?;
    let dataset = store::get_dataset(db, &id).await?;
    Ok(Json(json!({ "dataset": dataset.summary(), "saved": true, "definition": dataset.definition, "preview": overview })))
}

async fn save_mapping(
    state: &AppState,
    me: &Me,
    dataset: &DatasetInfo,
    doc: &MappingDoc,
    preview: &SheetPreview,
    field_spec: &[FieldSpec],
    definition: &Value,
) -> ApiResult<()> {
    let db = &state.db;
    let current = CollectionInfo {
        id: dataset.collection_id.clone(),
        name: dataset.collection_name.clone(),
        schema_id: Some(dataset.schema_id.clone()),
    };
    let target = match doc.collection.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        None => current,
        Some(reference) => match store::find_collection(db, me, reference).await? {
            Some(found) => found,
            None => store::create_collection(db, me, reference).await?,
        },
    };
    let schema_id = store::ensure_schema(db, me, &target, doc).await?;
    let mapping = Mapping::content(doc, dataset.sheet.as_deref(), preview.header_row);
    let same_collection = target.id == dataset.collection_id;
    if same_collection && store::version_is_private_draft(db, &dataset.version_id, &dataset.id).await? {
        store::update_draft_version(db, &dataset.version_id, definition, field_spec, mapping).await?;
    } else {
        let version = store::create_version(db, &schema_id, definition, field_spec, mapping).await?;
        store::retarget_dataset(db, &dataset.id, &version.id, &target.id).await?;
    }
    Ok(())
}

/// PATCH /datasets/{id}/import: imports every row with the saved mapping.
/// Body (optional): `{"mode": "replace" | "upsert"}`.
pub async fn import(State(state): State<AppState>, caller: Caller, Path(id): Path<String>, body: Bytes) -> ApiResult<Json<Value>> {
    let me = caller.me;
    store::touch_principal(&state.db, &me).await?;
    let dataset = store::get_own_dataset(&state.db, &me, &id).await?;
    let request: ImportRequest = if body.iter().all(u8::is_ascii_whitespace) {
        ImportRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::BadRequest(format!("invalid import options: {e}")))?
    };
    Ok(Json(importer::import(&state.db, &dataset, request).await?))
}

#[cfg(test)]
mod tests {
    use super::a1_letter;

    #[test]
    fn names_columns_like_a_spreadsheet() {
        assert_eq!(a1_letter(0), "A");
        assert_eq!(a1_letter(25), "Z");
        assert_eq!(a1_letter(26), "AA");
        assert_eq!(a1_letter(701), "ZZ");
        assert_eq!(a1_letter(702), "AAA");
    }
}
