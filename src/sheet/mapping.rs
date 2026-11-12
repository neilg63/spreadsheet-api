//! The JSON mapping document accepted by `PATCH /datasets/{id}/configure`.
//!
//! It is the structured equivalent of every spread-cli option (header/body rows, column
//! naming, `--keys` renames/formats/defaults/nesting, placeholder patterns, suppressions,
//! `--skip-cols`, `--filter`, `--exclude-null` ...) plus what an API needs on top:
//! primary keys, per-field schema metadata and query permissions. It only lists
//! *overrides*: any column not mentioned keeps its natural (snake_cased header) key and
//! auto-detected type, unless `unmapped` is set to `"drop"`.

use std::{collections::HashSet, path::Path, str::FromStr, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use spreadsheet_to_json::{
    Column, ColumnMatch, DateTimeMode, FieldNameMode, FilterCondition, FilterMatch, Format, KeySegment,
    MatchMode, OptionSet, heck::ToSnakeCase,
};

use super::{filter_pattern, key_pattern};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MappingDoc {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Import into an existing collection (id or name) instead of the one derived from
    /// the file and sheet name, e.g. when this month's file has a different name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    pub source: SourceOptions,
    /// Output paths whose values identify a row. Rows with the same key replace each
    /// other when a new file is imported into the same collection (upsert).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub primary_key: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldMapping>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<PatternMapping>,
    /// Source columns to drop: natural keys, A1 letters, column numbers or `start..end` ranges.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Dot-separated output paths to drop after mapping, e.g. `size.depth` or
    /// `addresses.$.admin2` (`$` = every array item).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<ImportFilter>,
    /// Drop keys whose value is null instead of storing `"key": null`.
    pub omit_nulls: bool,
    pub unmapped: Unmapped,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceOptions {
    /// 1-based header row; `null` means auto-detect (title and notes rows are skipped).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_row: Option<u32>,
    /// Number of rows forming the header, for merged multi-row headers (default 1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_rows: Option<u32>,
    /// 1-based row where data begins, when there is a gap below the header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_start: Option<u32>,
    /// The sheet has no header row: columns are named by `column_names`.
    pub no_header: bool,
    pub column_names: ColumnNames,
    /// Parse "1.234,5"-style numbers.
    pub decimal_comma: bool,
    /// Default rendering for date/time cells.
    pub datetime: DateTimeStyle,
    /// Stop after scanning this many data rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnNames {
    /// snake_cased header text, A1 letters where a header cell is blank
    #[default]
    Header,
    /// snake_cased header text, c01-style numbers where a header cell is blank
    HeaderNumbered,
    /// every column named by its letter: a, b, ... aa
    A1,
    /// every column named by its number: c01, c02 ...
    Numbered,
}

impl From<ColumnNames> for FieldNameMode {
    fn from(value: ColumnNames) -> Self {
        match value {
            ColumnNames::Header => FieldNameMode::AutoA1,
            ColumnNames::HeaderNumbered => FieldNameMode::AutoNumPadded,
            ColumnNames::A1 => FieldNameMode::A1,
            ColumnNames::Numbered => FieldNameMode::NumPadded,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateTimeStyle {
    /// 2026-06-15T10:17:00.000Z
    #[default]
    Full,
    /// 2026-06-15T10:17:00
    Simple,
    /// 2026-06-15
    Date,
    /// 10:17:00
    Time,
    /// 10:17
    Hm,
}

impl From<DateTimeStyle> for DateTimeMode {
    fn from(value: DateTimeStyle) -> Self {
        match value {
            DateTimeStyle::Full => DateTimeMode::Full,
            DateTimeStyle::Simple => DateTimeMode::Simple,
            DateTimeStyle::Date => DateTimeMode::DateOnly,
            DateTimeStyle::Time => DateTimeMode::TimeOnly,
            DateTimeStyle::Hm => DateTimeMode::HmOnly,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unmapped {
    /// Columns not listed in `fields`/`patterns` keep their natural key.
    #[default]
    Keep,
    /// Only columns listed in `fields`/`patterns` are output.
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Auto,
    String,
    Integer,
    Number,
    Boolean,
    Date,
    Datetime,
    Time,
    Array,
}

impl FieldType {
    fn to_format(self, precision: Option<u8>) -> Format {
        match self {
            Self::Auto => Format::Auto,
            Self::String => Format::Text,
            Self::Integer => Format::Integer,
            Self::Number => precision.map(Format::Decimal).unwrap_or(Format::Float),
            // Truthy also accepts yes/no, y/n, 1/0 text
            Self::Boolean => Format::Truthy,
            Self::Date => Format::Date,
            Self::Datetime => Format::DateTime,
            Self::Time => Format::Time,
            Self::Array => Format::Array(Arc::new(Format::Auto), Arc::from(",")),
        }
    }

    /// The field_spec type stored in api_schema_versions (which has no `time`).
    pub fn spec_type(self) -> Option<&'static str> {
        Some(match self {
            Self::Auto => return None,
            Self::String | Self::Time => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Date => "date",
            Self::Datetime => "datetime",
            Self::Array => "array",
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FieldMapping {
    /// Source column: natural key (`unit_price`), header text (`Unit Price`),
    /// A1 letter (`D`) or 1-based column number (`4` / `c04`).
    pub source: String,
    /// Output path: `price`, `size.width` (nested object) or `tags[]` (append to an array).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Advanced placement as a spreadsheet-to-json KeySegment tree (overrides `path`),
    /// e.g. `{"type":"array","container":"sales","identifier":2015,"key_field":"year","next":"amount"}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<Value>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub field_type: Option<FieldType>,
    /// Any spreadsheet-to-json format, overriding `type`: `d2`, `truthy(si,no)`,
    /// `dt:%d/%m/%Y`, `int[](|)` ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Decimal places for `type: number`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precision: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub datetime: Option<DateTimeStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decimal_comma: Option<bool>,
    /// Rows missing this value are imported but flagged invalid (hidden from queries).
    #[serde(skip_serializing_if = "is_false")]
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filterable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sortable: Option<bool>,
    /// Include in full-text search (`?q=`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub searchable: Option<bool>,
    /// Drop this column from the output.
    #[serde(skip_serializing_if = "is_false")]
    pub exclude: bool,
}

/// spread-cli placeholder patterns: `file_[n]` -> `files[]`, `sales_{region}` ->
/// `sales.{region}`, `sales_[year]` -> `sales[].{year}{amount}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PatternMapping {
    pub source: String,
    pub target: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub field_type: Option<FieldType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// A validation problem, addressed by a JSON path into the mapping document
/// (`fields[2].format`, `source.header_row`, `filter.all[1].op` ...) so a form can
/// highlight the widget that produced it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MappingError {
    pub path: String,
    pub message: String,
}

pub fn mapping_error(path: impl Into<String>, message: impl Into<String>) -> MappingError {
    MappingError { path: path.into(), message: message.into() }
}

pub type MappingResult<T> = Result<T, Vec<MappingError>>;

/// Rows that don't match are not imported. Either spread-cli's text syntax
/// (`"age >= 18 and name ILIKE 'a%'"`) or a tree a query-builder widget can emit:
/// `{"all": [{"field": "age", "op": "gte", "value": 18}, {"any": [...]}, {"not": {...}}]}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ImportFilter {
    Text(String),
    Tree(FilterNode),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FilterNode {
    All { all: Vec<FilterNode> },
    Any { any: Vec<FilterNode> },
    Not { not: Box<FilterNode> },
    Rule(FilterRule),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterRule {
    /// Output path after mapping, e.g. `age` or `size.width`.
    pub field: String,
    pub op: FilterOp,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub value: Value,
    /// String comparisons ignore case unless this is set.
    #[serde(default, skip_serializing_if = "is_false")]
    pub case_sensitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    Between,
    NotBetween,
    StartsWith,
    EndsWith,
    Contains,
    In,
    NotIn,
    /// the field is an array sharing at least one element with `value`
    Any,
    NotEmpty,
    Empty,
}

impl ImportFilter {
    pub fn compile(&self) -> MappingResult<FilterCondition> {
        match self {
            Self::Text(text) => filter_pattern::parse_filter(text).map_err(|e| vec![mapping_error("filter", e)]),
            Self::Tree(node) => {
                let mut errors = vec![];
                let condition = node.compile("filter", &mut errors);
                if errors.is_empty() { Ok(condition) } else { Err(errors) }
            }
        }
    }
}

impl FilterNode {
    fn compile(&self, at: &str, errors: &mut Vec<MappingError>) -> FilterCondition {
        let children = |key: &str, nodes: &[FilterNode], errors: &mut Vec<MappingError>| {
            if nodes.is_empty() {
                errors.push(mapping_error(format!("{at}.{key}"), "needs at least one condition"));
            }
            nodes
                .iter()
                .enumerate()
                .map(|(i, n)| n.compile(&format!("{at}.{key}[{i}]"), errors))
                .collect::<Vec<_>>()
        };
        match self {
            Self::All { all } => FilterCondition::And(children("all", all, errors)),
            Self::Any { any } => FilterCondition::Or(children("any", any, errors)),
            Self::Not { not } => FilterCondition::Not(Arc::new(not.compile(&format!("{at}.not"), errors))),
            Self::Rule(rule) => rule.compile(at, errors),
        }
    }
}

impl FilterRule {
    fn compile(&self, at: &str, errors: &mut Vec<MappingError>) -> FilterCondition {
        if !is_valid_path(&self.field) {
            errors.push(mapping_error(format!("{at}.field"), "must be a dotted output path, e.g. age or size.width"));
        }
        let mode = |v: &Value| if v.is_string() && !self.case_sensitive { MatchMode::Ci } else { MatchMode::Cs };
        let value = self.value.clone();
        let mut need = |ok: bool, what: &str| {
            if !ok {
                errors.push(mapping_error(format!("{at}.value"), format!("'{:?}' needs {what}", self.op).to_lowercase()));
            }
        };
        let pair = |v: &Value| match v.as_array().map(|a| a.as_slice()) {
            Some([lo, hi]) => (lo.clone(), hi.clone()),
            _ => (Value::Null, Value::Null),
        };
        let list = |v: &Value| v.as_array().cloned().unwrap_or_default();
        use FilterOp::*;
        let condition = match self.op {
            Eq | Ne => {
                need(!value.is_null(), "a value");
                FilterMatch::Exact(value.clone(), mode(&value))
            }
            Gt | Gte | Lt | Lte => {
                need(value.is_number() || value.is_string(), "a number or string");
                match self.op {
                    Gt => FilterMatch::Gt(value),
                    Gte => FilterMatch::Gte(value),
                    Lt => FilterMatch::Lt(value),
                    _ => FilterMatch::Lte(value),
                }
            }
            Between | NotBetween => {
                need(value.as_array().is_some_and(|a| a.len() == 2), "a [low, high] pair");
                let (lo, hi) = pair(&value);
                if self.op == Between { FilterMatch::Between(lo, hi) } else { FilterMatch::NotBetween(lo, hi) }
            }
            StartsWith | EndsWith | Contains => {
                need(value.is_string(), "a string");
                match self.op {
                    StartsWith => FilterMatch::StartsWith(value.clone(), mode(&value)),
                    EndsWith => FilterMatch::EndsWith(value.clone(), mode(&value)),
                    _ => FilterMatch::Contains(value.clone(), mode(&value)),
                }
            }
            In | NotIn | Any => {
                need(value.as_array().is_some_and(|a| !a.is_empty()), "a non-empty list");
                if self.op == Any { FilterMatch::AnyOneOf(list(&value)) } else { FilterMatch::OneOf(list(&value)) }
            }
            NotEmpty | Empty => FilterMatch::NotEmpty,
        };
        let path: Vec<&str> = self.field.split('.').collect();
        let matched = FilterCondition::Match(ColumnMatch::at_path(&path, condition));
        match self.op {
            Ne | NotIn | Empty => FilterCondition::Not(Arc::new(matched)),
            _ => matched,
        }
    }
}

impl FieldMapping {
    /// The dotted output path this field lands on, when it can be expressed as one.
    pub fn output_path(&self, natural_key: &str) -> Option<String> {
        if self.exclude {
            return None;
        }
        if self.key.is_some() {
            return None;
        }
        match &self.path {
            Some(path) => Some(path.trim_end_matches("[]").to_string()),
            None => Some(natural_key.to_string()),
        }
    }
}

/// The OptionSet shared by every read of one worksheet, before any column mapping.
pub fn base_options(path: &Path, sheet_index: u32, src: &SourceOptions) -> MappingResult<OptionSet> {
    let mut errors = vec![];
    if src.header_row == Some(0) {
        errors.push(mapping_error("source.header_row", "rows are numbered from 1"));
    }
    if src.data_start == Some(0) {
        errors.push(mapping_error("source.data_start", "rows are numbered from 1"));
    }
    if let (Some(header), Some(start)) = (src.header_row, src.data_start)
        && start < header {
            errors.push(mapping_error("source.data_start", "cannot come before the header row"));
        }
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut opts = OptionSet::new(&path.to_string_lossy()).sheet_index(sheet_index);
    opts.header_row = src.header_row.map(|row| (row - 1) as usize);
    opts.data_row_index = src.data_start.map(|row| (row - 1) as usize);
    opts.header_row_span = src.header_rows.unwrap_or(1).max(1) as usize;
    opts.detect_header = true;
    opts.omit_header = src.no_header;
    opts.field_mode = src.column_names.into();
    opts.rows.decimal_comma = src.decimal_comma;
    opts.rows.datetime_mode = src.datetime.into();
    Ok(opts)
}

/// Everything the mapping contributes to a read once the sheet's natural keys are known.
#[derive(Debug, Default, Clone)]
pub struct ResolvedMapping {
    pub columns: Vec<Column>,
    pub excluded_paths: Vec<Vec<String>>,
    pub filter: Option<FilterCondition>,
    /// natural key -> output path, for declared fields with a plain path
    pub field_paths: Vec<(usize, String)>,
}

impl ResolvedMapping {
    pub fn apply(&self, opts: &mut OptionSet, doc: &MappingDoc) {
        opts.rows.columns = self.columns.clone();
        opts.rows.excluded_paths = self.excluded_paths.clone();
        opts.rows.filter_rules = self.filter.clone();
        opts.rows.omit_null_values = doc.omit_nulls;
    }
}

impl MappingDoc {
    /// The parts of the document beyond `source`, i.e. what reshapes the output rather than
    /// how the sheet is read.
    pub fn customised_parts(&self) -> Vec<&'static str> {
        let mut parts = vec![];
        let mut flag = |used: bool, name: &'static str| {
            if used {
                parts.push(name);
            }
        };
        flag(self.title.is_some(), "title");
        flag(self.description.is_some(), "description");
        flag(self.collection.is_some(), "collection");
        flag(!self.primary_key.is_empty(), "primary_key");
        flag(!self.fields.is_empty(), "fields");
        flag(!self.patterns.is_empty(), "patterns");
        flag(!self.exclude.is_empty(), "exclude");
        flag(!self.exclude_paths.is_empty(), "exclude_paths");
        flag(self.filter.is_some(), "filter");
        flag(self.omit_nulls, "omit_nulls");
        flag(self.unmapped == Unmapped::Drop, "unmapped");
        parts
    }

    /// Validates the document on its own, before any file is read.
    pub fn validate(&self) -> MappingResult<()> {
        let mut errors = vec![];
        for (i, field) in self.fields.iter().enumerate() {
            let at = |prop: &str| format!("fields[{i}].{prop}");
            if field.source.trim().is_empty() {
                errors.push(mapping_error(at("source"), "choose a source column"));
            }
            if let Some(path) = &field.path
                && let Err(msg) = path_to_segment(path) {
                    errors.push(mapping_error(at("path"), msg));
                }
            if let Some(key) = &field.key
                && KeySegment::from_json(key).is_none() {
                    errors.push(mapping_error(at("key"), "not a valid key segment"));
                }
            if let Some(fmt) = &field.format
                && let Err(msg) = parse_format(fmt) {
                    errors.push(mapping_error(at("format"), msg));
                }
            if let Some(default) = &field.default
                && !(default.is_string() || default.is_number() || default.is_boolean()) {
                    errors.push(mapping_error(at("default"), "must be a string, number or boolean"));
                }
        }
        for (i, pattern) in self.patterns.iter().enumerate() {
            if pattern.source.trim().is_empty() {
                errors.push(mapping_error(format!("patterns[{i}].source"), "enter a column pattern, e.g. sales_{region}"));
            }
            if pattern.target.trim().is_empty() {
                errors.push(mapping_error(format!("patterns[{i}].target"), "enter a target path, e.g. sales.{region}"));
            }
            if let Some(fmt) = &pattern.format
                && let Err(msg) = parse_format(fmt) {
                    errors.push(mapping_error(format!("patterns[{i}].format"), msg));
                }
        }
        for (i, path) in self.primary_key.iter().enumerate() {
            if !is_valid_path(path) {
                errors.push(mapping_error(format!("primary_key[{i}]"), "must be an output path, e.g. sku or product.code"));
            }
        }
        for (i, path) in self.exclude_paths.iter().enumerate() {
            if path.split('.').any(|s| s.trim().is_empty()) {
                errors.push(mapping_error(format!("exclude_paths[{i}]"), "has an empty segment"));
            }
        }
        if let Some(filter) = &self.filter
            && let Err(filter_errors) = filter.compile() {
                errors.extend(filter_errors);
            }
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }

    /// Resolves column references against a worksheet's natural keys (in sheet order).
    /// Reports document and column errors together, so a form can flag them all at once.
    pub fn resolve(&self, natural_keys: &[String]) -> MappingResult<ResolvedMapping> {
        let mut errors = self.validate().err().unwrap_or_default();
        let mut resolved = ResolvedMapping::default();
        let mut claimed: HashSet<String> = HashSet::new();

        for (i, field) in self.fields.iter().enumerate() {
            if field.source.trim().is_empty() {
                continue; // already reported by validate()
            }
            let Some(natural_key) = resolve_source(&field.source, natural_keys) else {
                errors.push(mapping_error(
                    format!("fields[{i}].source"),
                    format!("'{}' matches no column in this sheet", field.source),
                ));
                continue;
            };
            if !claimed.insert(natural_key.clone()) {
                errors.push(mapping_error(format!("fields[{i}].source"), format!("column '{natural_key}' is mapped more than once")));
                continue;
            }
            if field.exclude {
                resolved.columns.push(key_pattern::build_suppression_column(&natural_key));
                continue;
            }
            let format = match (&field.format, field.field_type) {
                (Some(fmt), _) => parse_format(fmt).unwrap_or(Format::Auto),
                (None, Some(field_type)) => field_type.to_format(field.precision),
                (None, None) => Format::Auto,
            };
            let mut column = Column::from_source_key_with_format(
                &natural_key,
                None,
                format,
                field.default.clone(),
                field.datetime.map(DateTimeMode::from).unwrap_or(DateTimeMode::Full),
                field.decimal_comma.unwrap_or(self.source.decimal_comma),
            );
            column.key = match (&field.key, &field.path) {
                (Some(key), _) => KeySegment::from_json(key),
                (None, Some(path)) => path_to_segment(path).ok(),
                (None, None) => None,
            };
            if let Some(path) = field.output_path(&natural_key) {
                resolved.field_paths.push((i, path));
            }
            resolved.columns.push(column);
        }

        for (i, pattern) in self.patterns.iter().enumerate() {
            let format = match (&pattern.format, pattern.field_type) {
                (Some(fmt), _) => parse_format(fmt).unwrap_or(Format::Auto),
                (None, Some(field_type)) => field_type.to_format(None),
                (None, None) => Format::Auto,
            };
            // expand_pattern_entry matches the natural keys case-insensitively by prefix
            match key_pattern::expand_pattern_entry(&pattern.source, &pattern.target, &format, natural_keys) {
                None => errors.push(mapping_error(
                    format!("patterns[{i}]"),
                    "not a supported pattern: use prefix_[n] -> list[], prefix_{key} -> object.{key} or prefix_[n] -> list[].{n}{value}",
                )),
                Some(columns) if columns.is_empty() => {
                    errors.push(mapping_error(format!("patterns[{i}].source"), "matches no column in this sheet"))
                }
                Some(columns) => {
                    for column in columns {
                        let source = column.source_key_name();
                        if claimed.insert(source) {
                            resolved.columns.push(column);
                        }
                    }
                }
            }
        }

        for (i, entry) in self.exclude.iter().enumerate() {
            if let Some((start, end)) = entry.split_once("..") {
                let position = |identifier: &str| {
                    resolve_source(identifier.trim(), natural_keys)
                        .and_then(|key| natural_keys.iter().position(|k| *k == key))
                };
                match (position(start), position(end)) {
                    (Some(from), Some(to)) if from <= to => {
                        for key in &natural_keys[from..=to] {
                            if claimed.insert(key.clone()) {
                                resolved.columns.push(key_pattern::build_suppression_column(key));
                            }
                        }
                    }
                    (Some(_), Some(_)) => errors.push(mapping_error(format!("exclude[{i}]"), "the range is reversed")),
                    _ => errors.push(mapping_error(format!("exclude[{i}]"), "a range end matches no column")),
                }
            } else if let Some(key) = resolve_source(entry, natural_keys) {
                // an unmatched single column is ignored, as in spread-cli: the same mapping
                // may be reused for files that lack an optional column
                if claimed.insert(key.clone()) {
                    resolved.columns.push(key_pattern::build_suppression_column(&key));
                }
            }
        }

        if self.unmapped == Unmapped::Drop {
            for key in natural_keys {
                if !claimed.contains(key) {
                    resolved.columns.push(key_pattern::build_suppression_column(key));
                }
            }
        }

        resolved.excluded_paths = self
            .exclude_paths
            .iter()
            .map(|p| p.split('.').map(|s| s.trim().to_string()).collect())
            .collect();
        if !errors.is_empty() {
            // sort by position in the document so errors read top to bottom
            errors.sort_by_key(|e| error_order(&e.path));
            return Err(errors);
        }
        resolved.filter = self.filter.as_ref().map(ImportFilter::compile).transpose()?;
        Ok(resolved)
    }
}

fn error_order(path: &str) -> (usize, usize) {
    let section = ["source", "primary_key", "fields", "patterns", "exclude", "exclude_paths", "filter"]
        .iter()
        .position(|s| path.starts_with(s))
        .unwrap_or(0);
    let index = path.split_once('[').and_then(|(_, rest)| rest.split(']').next()?.parse().ok()).unwrap_or(0);
    (section, index)
}

/// Matches a column reference against the sheet's natural keys.
pub fn resolve_source(source: &str, natural_keys: &[String]) -> Option<String> {
    let trimmed = source.trim();
    if let Some(found) = natural_keys.iter().find(|k| k.eq_ignore_ascii_case(trimmed)) {
        return Some(found.clone());
    }
    let snake = trimmed.to_snake_case();
    if let Some(found) = natural_keys.iter().find(|k| **k == snake) {
        return Some(found.clone());
    }
    // A1 letters: a..z, aa..zz, aaa..
    if !trimmed.is_empty() && trimmed.len() <= 3 && trimmed.chars().all(|c| c.is_ascii_alphabetic()) {
        let index = trimmed
            .to_ascii_lowercase()
            .bytes()
            .fold(0usize, |acc, b| acc * 26 + (b - b'a' + 1) as usize);
        return natural_keys.get(index - 1).cloned();
    }
    // numbered: 4, c4, c04
    key_pattern::resolve_suppression_key(trimmed, natural_keys)
}

/// `a.b.c` -> Object(a, Object(b, Simple(c))); a final `name[]` appends to an array.
pub fn path_to_segment(path: &str) -> Result<KeySegment, String> {
    let parts: Vec<&str> = path.split('.').map(str::trim).collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!("'{path}' has an empty segment"));
    }
    let last = parts.len() - 1;
    let mut segment: Option<KeySegment> = None;
    for (i, part) in parts.iter().enumerate().rev() {
        let (name, is_array) = match part.strip_suffix("[]") {
            Some(name) => (name, true),
            None => (*part, false),
        };
        if is_array && i != last {
            return Err(format!("'{path}': only the last segment can append to an array"));
        }
        if !is_identifier(name) {
            return Err(format!("'{path}': '{name}' must use letters, digits and underscores"));
        }
        segment = Some(match segment {
            None if is_array => KeySegment::PlainArray(Arc::from(name)),
            None => KeySegment::Simple(Arc::from(name)),
            Some(next) => KeySegment::Object(Arc::from(name), Arc::new(next)),
        });
    }
    segment.ok_or_else(|| "empty path".to_string())
}

fn parse_format(fmt: &str) -> Result<Format, String> {
    let parsed = Format::from_str(fmt).map_err(|e| e.to_string())?;
    let base = fmt.split('[').next().unwrap_or("").trim().to_lowercase();
    if parsed.to_string().starts_with("auto") && base != "auto" {
        return Err(format!("'{fmt}' is not a recognised format"));
    }
    if let Format::Array(inner, _) = &parsed
        && inner.to_string() == "auto" && !base.is_empty() && base != "auto" {
            return Err(format!("'{fmt}' is not a recognised format"));
        }
    Ok(parsed)
}

fn is_identifier(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A dotted path of identifiers, safe to embed in a SurrealQL idiom.
pub fn is_valid_path(path: &str) -> bool {
    path.split('.').all(is_identifier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn keys() -> Vec<String> {
        ["sku", "unit_price", "colour", "width", "height", "depth", "sales_2024", "sales_2025"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn resolves_sources_by_key_header_text_letter_and_number() {
        let keys = keys();
        assert_eq!(resolve_source("SKU", &keys).as_deref(), Some("sku"));
        assert_eq!(resolve_source("Unit Price", &keys).as_deref(), Some("unit_price"));
        assert_eq!(resolve_source("B", &keys).as_deref(), Some("unit_price"));
        assert_eq!(resolve_source("c03", &keys).as_deref(), Some("colour"));
        assert_eq!(resolve_source("4", &keys).as_deref(), Some("width"));
        assert_eq!(resolve_source("nope_nope", &keys), None);
    }

    #[test]
    fn builds_nested_and_array_paths() {
        assert_eq!(
            path_to_segment("size.width").unwrap(),
            KeySegment::Object(Arc::from("size"), Arc::new(KeySegment::Simple(Arc::from("width"))))
        );
        assert_eq!(path_to_segment("tags[]").unwrap(), KeySegment::PlainArray(Arc::from("tags")));
        assert!(path_to_segment("a[].b").is_err());
        assert!(path_to_segment("a..b").is_err());
        assert!(path_to_segment("a.b-c").is_err());
    }

    #[test]
    fn resolves_fields_patterns_excludes_and_unmapped_drop() {
        let doc: MappingDoc = serde_json::from_value(json!({
            "fields": [
                { "source": "Unit Price", "path": "price", "type": "number", "precision": 2 },
                { "source": "sku", "required": true }
            ],
            "patterns": [{ "source": "sales_[year]", "target": "sales[].{year}{amount}" }],
            "exclude": ["width..depth"],
            "unmapped": "drop",
            "primary_key": ["sku"]
        }))
        .unwrap();
        let resolved = doc.resolve(&keys()).unwrap();
        let excluded: Vec<String> = resolved
            .columns
            .iter()
            .filter(|c| c.key == Some(KeySegment::Excluded))
            .map(|c| c.source_key_name())
            .collect();
        assert_eq!(excluded, vec!["width", "height", "depth", "colour"]);
        assert_eq!(resolved.columns.len(), 2 + 2 + 4);
        assert_eq!(resolved.field_paths, vec![(0, "price".to_string()), (1, "sku".to_string())]);
    }

    #[test]
    fn reports_unknown_sources_formats_and_bad_filters() {
        let doc: MappingDoc = serde_json::from_value(json!({
            "fields": [{ "source": "missing" }, { "source": "sku", "format": "wibble" }],
            "filter": "price >>> 3"
        }))
        .unwrap();
        let errors = doc.resolve(&keys()).unwrap_err();
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["fields[0].source", "fields[1].format", "filter"]);
        let unmatched = MappingDoc { fields: vec![FieldMapping { source: "missing".into(), ..Default::default() }], ..Default::default() };
        assert_eq!(unmatched.resolve(&keys()).unwrap_err()[0].path, "fields[0].source");
    }

    #[test]
    fn compiles_widget_filter_trees() {
        use spreadsheet_to_json::indexmap::IndexMap;
        let doc: MappingDoc = serde_json::from_value(json!({
            "filter": { "all": [
                { "field": "price", "op": "gte", "value": 10 },
                { "any": [
                    { "field": "name", "op": "starts_with", "value": "wid" },
                    { "not": { "field": "size.w", "op": "in", "value": [1, 2] } }
                ]}
            ]}
        }))
        .unwrap();
        let condition = doc.filter.as_ref().unwrap().compile().unwrap();
        let row = |v: Value| -> IndexMap<String, Value> { serde_json::from_value(v).unwrap() };
        assert!(condition.evaluate(&row(json!({ "price": 12, "name": "Widget", "size": { "w": 1 } }))));
        assert!(condition.evaluate(&row(json!({ "price": 12, "name": "Gadget", "size": { "w": 3 } }))));
        assert!(!condition.evaluate(&row(json!({ "price": 12, "name": "Gadget", "size": { "w": 2 } }))));
        assert!(!condition.evaluate(&row(json!({ "price": 5, "name": "Widget" }))));

        let bad: MappingDoc = serde_json::from_value(json!({
            "filter": { "all": [{ "field": "price", "op": "between", "value": 3 }] }
        }))
        .unwrap();
        assert_eq!(bad.validate().unwrap_err()[0].path, "filter.all[0].value");
    }

    #[test]
    fn rejects_unknown_properties() {
        let err = serde_json::from_value::<MappingDoc>(json!({ "feilds": [] })).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn accepts_library_format_strings() {
        for fmt in ["d2", "int", "truthy(si,no)", "dt:%d/%m/%Y", "int[](|)", "text[]", "auto"] {
            assert!(parse_format(fmt).is_ok(), "{fmt}");
        }
    }
}
