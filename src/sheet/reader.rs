//! Thin async wrappers over spreadsheet-to-json. The library does blocking file and CPU
//! work inside its async functions, so every read runs on the blocking thread pool.

use std::path::Path;

use serde_json::Value;
use spreadsheet_to_json::{
    OptionSet, PathData, ReadMode, ResultSet,
    calamine::{Reader, open_workbook_auto},
    error::GenericError,
    indexmap::IndexMap,
    process_spreadsheet_async, process_spreadsheet_immediate,
};
use tokio::{runtime::Handle, task::JoinHandle};

use super::mapping::{MappingDoc, MappingError, base_options};

/// Rows above the data a preview may have to scan past (title/notes rows, detection sample).
const SCAN_MARGIN: u32 = 25;

pub type Row = IndexMap<String, Value>;

/// Accepted upload extensions, and the extension the file is stored under. The reader
/// sniffs `,`/`;`/tab delimiters from content, so semicolon files are read as .csv.
pub fn storage_extension(filename: &str) -> Option<&'static str> {
    let ext = Path::new(filename).extension()?.to_str()?.to_lowercase();
    Some(match ext.as_str() {
        "xlsx" => "xlsx",
        "xlsm" => "xlsm",
        "xls" => "xls",
        "xlsb" => "xlsb",
        "ods" => "ods",
        "csv" | "ssv" | "scsv" | "txt" => "csv",
        "tsv" | "tab" => "tsv",
        _ => return None,
    })
}

async fn read(opts: OptionSet) -> Result<ResultSet, GenericError> {
    tokio::task::spawn_blocking(move || Handle::current().block_on(process_spreadsheet_immediate(&opts)))
        .await
        .map_err(|_| GenericError("reader_panicked"))?
}

/// Worksheet names in order; empty for CSV/TSV, which hold a single unnamed sheet.
pub async fn list_sheets(path: &Path) -> Result<Vec<String>, GenericError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let path_data = PathData::new(&path);
        if !path_data.is_valid() {
            return Err(GenericError("unsupported_format"));
        }
        if !path_data.use_calamine() {
            return Ok(vec![]);
        }
        let workbook = open_workbook_auto(&path).map_err(|_| GenericError("cannot_open_workbook"))?;
        let names = workbook.sheet_names();
        if names.is_empty() { Err(GenericError("workbook_with_no_sheets")) } else { Ok(names) }
    })
    .await
    .map_err(|_| GenericError("reader_panicked"))?
}

#[derive(Debug, Clone)]
pub struct SheetPreview {
    /// Column keys before mapping, in sheet order.
    pub natural_keys: Vec<String>,
    /// 1-based header row actually used (detected or configured); None = no header.
    pub header_row: Option<usize>,
    /// 1-based first data row actually used.
    pub data_start: usize,
    /// Sheet rows from `data_start` down (may include trailing blank rows).
    pub row_count: usize,
    pub natural_rows: Vec<Value>,
    /// Rows after applying the mapping; empty when the mapping has errors.
    pub rows: Vec<Value>,
    pub errors: Vec<MappingError>,
}

fn to_values(result: &ResultSet, n: usize) -> Vec<Value> {
    result.to_vec().into_iter().take(n).map(|row| Value::Object(row.into_iter().collect())).collect()
}

/// Reads the first `n` data rows of one worksheet, both raw and mapped by `doc`.
/// `Err` only when the source options themselves are invalid or the file can't be read;
/// column mapping problems come back in `SheetPreview::errors`.
pub async fn preview(
    path: &Path,
    sheet_index: u32,
    doc: &MappingDoc,
    n: u32,
) -> Result<Result<SheetPreview, Vec<MappingError>>, GenericError> {
    let base = match base_options(path, sheet_index, &doc.source) {
        Ok(opts) => opts,
        Err(errors) => return Ok(Err(errors)),
    };
    let src = &doc.source;
    let margin = SCAN_MARGIN
        .max(src.header_row.unwrap_or(0) + src.header_rows.unwrap_or(1))
        .max(src.data_start.unwrap_or(0));
    // in direct mode `max` counts sheet rows from the top, not data rows
    let mut natural_opts = base.clone();
    natural_opts.max = Some(margin + n);
    let natural = read(natural_opts).await?;
    let data_start_index = natural.body_start_index;
    let mut preview = SheetPreview {
        natural_keys: natural.keys.clone(),
        header_row: natural.header_row_index.map(|i| i + 1),
        data_start: data_start_index + 1,
        row_count: natural.num_rows.saturating_sub(data_start_index),
        natural_rows: to_values(&natural, n as usize),
        rows: vec![],
        errors: vec![],
    };
    match doc.resolve(&natural.keys) {
        Err(errors) => preview.errors = errors,
        Ok(resolved) if resolved.columns.is_empty() && resolved.filter.is_none()
            && resolved.excluded_paths.is_empty() && !doc.omit_nulls =>
        {
            preview.rows = preview.natural_rows.clone();
        }
        Ok(resolved) => {
            let mut mapped_opts = base;
            resolved.apply(&mut mapped_opts, doc);
            // a filter may skip rows: scan further, but stop once `n` rows match
            mapped_opts.max = Some(margin + if resolved.filter.is_some() { (n * 50).max(2_000) } else { n });
            mapped_opts.limit = Some(n);
            preview.rows = to_values(&read(mapped_opts).await?, n as usize);
        }
    }
    Ok(Ok(preview))
}

/// Streams every mapped row of a worksheet through a bounded channel. At most
/// `max_rows` data rows are scanned; the reader stops early if the receiver is dropped.
pub fn stream(mut opts: OptionSet, max_rows: u32) -> (async_channel::Receiver<Row>, JoinHandle<Result<ResultSet, GenericError>>) {
    let (tx, rx) = async_channel::bounded::<Row>(4096);
    opts.read_mode = ReadMode::Async;
    opts.max = Some(max_rows);
    let handle = tokio::task::spawn_blocking(move || {
        let save = Box::new(move |row: Row| tx.send_blocking(row).map_err(|_| GenericError("import_aborted")));
        Handle::current().block_on(process_spreadsheet_async(&opts, save, None))
    });
    (rx, handle)
}

#[cfg(test)]
mod tests {
    use super::storage_extension;

    #[test]
    fn maps_upload_extensions_to_readable_ones() {
        assert_eq!(storage_extension("Report.XLSX"), Some("xlsx"));
        assert_eq!(storage_extension("data.ssv"), Some("csv"));
        assert_eq!(storage_extension("data.tab"), Some("tsv"));
        assert_eq!(storage_extension("notes.docx"), None);
        assert_eq!(storage_extension("noext"), None);
    }
}
