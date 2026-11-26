//! Translates GET /datasets/{id} query parameters into a parameterised SQL query over
//! `dataset_rows`, whose `data` column holds each row as JSON.
//!
//! Filters come in either of two JSON shapes:
//! - MongoDB style: `{"price": {"$gte": 10}, "country": {"$in": ["GB", "IE"]}, "$or": [...]}`
//! - the rule tree used by the import filter, convenient for query-builder widgets:
//!   `{"all": [{"field": "price", "op": "gte", "value": 10}, {"any": [...]}]}`
//!
//! Field paths are validated identifiers written into the SQL as JSON paths
//! (`data ->> '$."size"."w"'`); every value is a bound parameter, never query text.
//!
//! Comparisons keep to one type: a number is only greater or less than a number, text
//! than text (which orders ISO dates correctly). A condition on a missing field is
//! false, so its negation is true.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    auth::Constraints,
    error::{ApiError, ApiResult},
    sheet::{
        mapping::{FilterNode, FilterRule, is_valid_path},
        spec::FieldSpec,
    },
};

const MAX_CONDITIONS: usize = 50;

#[derive(Debug, Default, Deserialize)]
pub struct QueryParams {
    /// JSON filter, MongoDB style or rule tree.
    pub filter: Option<String>,
    /// `price,-name` or `price:desc,name:asc`
    pub sort: Option<String>,
    /// Full-text search over fields marked searchable.
    pub q: Option<String>,
    pub limit: Option<u32>,
    pub start: Option<u32>,
    /// 1-based page, as an alternative to `start`.
    pub page: Option<u32>,
    /// Include rows flagged invalid during import (hidden by default).
    pub include_invalid: Option<bool>,
}

pub struct BuiltQuery {
    /// The conditions, with `?` for each of `binds` in order.
    pub where_sql: String,
    pub binds: Vec<Value>,
    order_sql: String,
    pub start: u32,
    pub limit: u32,
    pub uses_filter: bool,
    pub uses_sort: bool,
    pub uses_search: bool,
}

impl BuiltQuery {
    /// One page of rows, each as JSON text.
    pub fn rows_sql(&self) -> String {
        format!(
            "SELECT json(data) FROM dataset_rows WHERE {} ORDER BY {} LIMIT {} OFFSET {}",
            self.where_sql, self.order_sql, self.limit, self.start
        )
    }

    pub fn count_sql(&self) -> String {
        format!("SELECT count(*) FROM dataset_rows WHERE {}", self.where_sql)
    }
}

struct Builder<'a> {
    binds: Vec<Value>,
    conditions: usize,
    spec: &'a [FieldSpec],
    /// False while building a filter the service imposes, which may use any field.
    check_flags: bool,
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::BadRequest(message.into())
}

/// `size.w` -> `'$."size"."w"'`. Paths are validated identifiers, so they hold no quotes.
fn json_path(path: &str) -> String {
    format!("'$.{}'", path.split('.').map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join("."))
}

/// A field's value as SQL: a number, text, 1 or 0 for true or false, NULL when missing
/// or null, and JSON text for a list or object.
fn field_sql(path: &str) -> String {
    format!("(data ->> {})", json_path(path))
}

/// The words of a search, each quoted, for FTS5: every word must be present.
fn fts_query(q: &str) -> String {
    q.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(" ")
}

impl<'a> Builder<'a> {
    fn bind(&mut self, value: Value) -> &'static str {
        self.binds.push(value);
        "?"
    }

    fn binds(&mut self, values: &[Value]) -> String {
        values.iter().map(|v| self.bind(v.clone())).collect::<Vec<_>>().join(", ")
    }

    /// Checks that `path` may be used to filter or sort, and counts it.
    fn check(&mut self, path: &str, purpose: &str) -> ApiResult<()> {
        if !is_valid_path(path) {
            return Err(bad(format!("'{path}' is not a field path (use letters, digits, underscores and dots)")));
        }
        if self.check_flags && let Some(spec) = self.spec.iter().find(|s| s.path == path) {
            let allowed = if purpose == "sort" { spec.sortable } else { spec.filterable };
            if !allowed {
                return Err(ApiError::Forbidden(format!("'{path}' cannot be used to {purpose}")));
            }
        }
        self.conditions += 1;
        if self.conditions > MAX_CONDITIONS {
            return Err(bad(format!("a query can use at most {MAX_CONDITIONS} conditions")));
        }
        Ok(())
    }

    /// One comparison on one field.
    fn compare(&mut self, path: &str, op: &str, value: &Value, case_sensitive: bool) -> ApiResult<String> {
        self.check(path, "filter")?;
        let f = field_sql(path);
        let at = json_path(path);
        // the field as text, lower-cased unless the comparison is case-sensitive
        let text = if case_sensitive { format!("CAST(coalesce({f}, '') AS TEXT)") } else { format!("lower_u(coalesce({f}, ''))") };
        let needle = |this: &mut Self| -> ApiResult<Option<&'static str>> {
            let s = value.as_str().ok_or_else(|| bad(format!("'{op}' on '{path}' needs a string")))?;
            if s.is_empty() {
                return Ok(None); // every text starts with, ends with and contains ""
            }
            Ok(Some(this.bind(json!(if case_sensitive { s.to_string() } else { s.to_lowercase() }))))
        };
        let list = |this: &mut Self| -> ApiResult<(String, usize)> {
            match value {
                Value::Array(items) if !items.is_empty() => {
                    let mut distinct: Vec<Value> = vec![];
                    for item in items {
                        if !distinct.contains(item) {
                            distinct.push(item.clone());
                        }
                    }
                    Ok((this.binds(&distinct), distinct.len()))
                }
                _ => Err(bad(format!("'{op}' on '{path}' needs a non-empty list"))),
            }
        };
        // an ordering comparison only holds between two numbers or two texts
        let ordered = |this: &mut Self, sql_op: &str, bound: &Value| -> ApiResult<String> {
            let kind = match bound {
                Value::Number(_) => "IN ('integer', 'real')",
                Value::String(_) => "= 'text'",
                _ => return Err(bad(format!("'{op}' on '{path}' needs a number or a string"))),
            };
            Ok(format!("(typeof({f}) {kind} AND {f} {sql_op} {})", this.bind(bound.clone())))
        };
        let sql = match op {
            "eq" if value.is_string() && !case_sensitive => {
                format!("{text} = {}", self.bind(json!(value.as_str().unwrap_or_default().to_lowercase())))
            }
            "eq" => format!("{f} IS {}", self.bind(value.clone())),
            "ne" => format!("{f} IS NOT {}", self.bind(value.clone())),
            "gt" => ordered(self, ">", value)?,
            "gte" => ordered(self, ">=", value)?,
            "lt" => ordered(self, "<", value)?,
            "lte" => ordered(self, "<=", value)?,
            "between" | "not_between" => {
                let Some([lo, hi]) = value.as_array().map(|a| a.as_slice()).and_then(|a| <&[Value; 2]>::try_from(a).ok()) else {
                    return Err(bad(format!("'{op}' on '{path}' needs a [low, high] pair")));
                };
                let range = format!("({} AND {})", ordered(self, ">=", lo)?, ordered(self, "<=", hi)?);
                if op == "between" { range } else { format!("NOT {range}") }
            }
            "starts_with" => needle(self)?.map_or("1".into(), |p| format!("instr({text}, {p}) = 1")),
            "contains" => needle(self)?.map_or("1".into(), |p| format!("instr({text}, {p}) > 0")),
            "ends_with" => match needle(self)? {
                None => "1".into(),
                Some(p) => {
                    let again = self.binds.last().cloned().unwrap_or_default();
                    format!("substr({text}, -length({p})) = {}", self.bind(again))
                }
            },
            "regex" => {
                let pattern = value.as_str().ok_or_else(|| bad(format!("'regex' on '{path}' needs a string")))?;
                regex_check(pattern).map_err(|e| bad(format!("'regex' on '{path}': {e}")))?;
                let pattern = if case_sensitive { pattern.to_string() } else { format!("(?i){pattern}") };
                format!("CAST(coalesce({f}, '') AS TEXT) REGEXP {}", self.bind(json!(pattern)))
            }
            "in" => format!("{f} IN ({})", list(self)?.0),
            "not_in" => format!("({f} IS NULL OR {f} NOT IN ({}))", list(self)?.0),
            // the list operators look inside a field that holds a list
            "any" => format!("EXISTS (SELECT 1 FROM json_each(data, {at}) WHERE value IN ({}))", list(self)?.0),
            "all" => {
                let (params, count) = list(self)?;
                format!("(SELECT count(DISTINCT value) FROM json_each(data, {at}) WHERE value IN ({params})) = {count}")
            }
            "has" => format!("EXISTS (SELECT 1 FROM json_each(data, {at}) WHERE value IS {})", self.bind(value.clone())),
            "size" => format!("coalesce(json_array_length(data, {at}), 0) = {}", self.bind(value.clone())),
            "not_empty" => format!("({f} IS NOT NULL AND {f} != '')"),
            "empty" => format!("({f} IS NULL OR {f} = '')"),
            "exists" => {
                if value.as_bool().unwrap_or(true) { format!("{f} IS NOT NULL") } else { format!("{f} IS NULL") }
            }
            other => return Err(bad(format!("unknown operator '{other}'"))),
        };
        // never NULL: a comparison with a missing field is false, and NOT of it true
        Ok(format!("coalesce({sql}, 0)"))
    }

    fn join(parts: Vec<String>, glue: &str) -> String {
        match parts.len() {
            0 => "1".to_string(),
            1 => parts.into_iter().next().unwrap(),
            _ => format!("({})", parts.join(glue)),
        }
    }

    fn rule_tree(&mut self, node: &FilterNode) -> ApiResult<String> {
        Ok(match node {
            FilterNode::All { all } => {
                let parts = all.iter().map(|n| self.rule_tree(n)).collect::<ApiResult<Vec<_>>>()?;
                Self::join(parts, " AND ")
            }
            FilterNode::Any { any } => {
                let parts = any.iter().map(|n| self.rule_tree(n)).collect::<ApiResult<Vec<_>>>()?;
                Self::join(parts, " OR ")
            }
            FilterNode::Not { not } => format!("NOT ({})", self.rule_tree(not)?),
            FilterNode::Rule(FilterRule { field, op, value, case_sensitive }) => {
                let op = serde_json::to_value(op).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                // the import filter's default is case-insensitive; keep it for queries
                let case_sensitive = *case_sensitive || !matches!(op.as_str(), "eq" | "starts_with" | "ends_with" | "contains");
                self.compare(field, &op, value, case_sensitive)?
            }
        })
    }

    /// `{"a": 1, "b": {"$gt": 2}, "$or": [...]}` -> AND of its entries.
    fn mongo(&mut self, filter: &Map<String, Value>) -> ApiResult<String> {
        let mut parts = vec![];
        for (key, value) in filter {
            let logical = |this: &mut Self, glue: &str| -> ApiResult<String> {
                let items = value.as_array().filter(|a| !a.is_empty()).ok_or_else(|| bad(format!("'{key}' needs a list of conditions")))?;
                let sub = items
                    .iter()
                    .map(|item| item.as_object().ok_or_else(|| bad(format!("'{key}' items must be objects"))).and_then(|o| this.mongo(o)))
                    .collect::<ApiResult<Vec<_>>>()?;
                Ok(Self::join(sub, glue))
            };
            parts.push(match key.as_str() {
                "$and" => logical(self, " AND ")?,
                "$or" => logical(self, " OR ")?,
                "$nor" => format!("NOT ({})", logical(self, " OR ")?),
                "$not" => {
                    let inner = value.as_object().ok_or_else(|| bad("'$not' needs an object"))?;
                    format!("NOT ({})", self.mongo(inner)?)
                }
                op if op.starts_with('$') => return Err(bad(format!("unknown operator '{op}'"))),
                path => match value {
                    Value::Object(ops) if !ops.is_empty() && ops.keys().all(|k| k.starts_with('$')) => {
                        let case_insensitive = ops.get("$options").and_then(Value::as_str).is_some_and(|o| o.contains('i'));
                        let mut field_parts = vec![];
                        for (op, arg) in ops.iter().filter(|(k, _)| *k != "$options") {
                            let (name, cs) = match op.as_str() {
                                "$eq" => ("eq", true),
                                "$ne" => ("ne", true),
                                "$gt" => ("gt", true),
                                "$gte" => ("gte", true),
                                "$lt" => ("lt", true),
                                "$lte" => ("lte", true),
                                "$in" => ("in", true),
                                "$nin" => ("not_in", true),
                                "$all" => ("all", true),
                                "$size" => ("size", true),
                                "$exists" => ("exists", true),
                                "$regex" => ("regex", !case_insensitive),
                                "$contains" => ("contains", false),
                                "$startsWith" => ("starts_with", false),
                                "$endsWith" => ("ends_with", false),
                                "$between" => ("between", true),
                                "$has" => ("has", true),
                                other => return Err(bad(format!("unknown operator '{other}'"))),
                            };
                            field_parts.push(self.compare(path, name, arg, cs)?);
                        }
                        Self::join(field_parts, " AND ")
                    }
                    other => self.compare(path, "eq", other, true)?,
                },
            });
        }
        Ok(Self::join(parts, " AND "))
    }
}

fn is_rule_tree(value: &Value) -> bool {
    value.as_object().is_some_and(|o| ["all", "any", "not", "field"].iter().any(|k| o.contains_key(*k)))
}

/// A pattern the `regex` operator can run.
fn regex_check(pattern: &str) -> Result<(), String> {
    if pattern.len() > 500 {
        return Err("the pattern is longer than 500 characters".into());
    }
    Ok(())
}

impl Builder<'_> {
    /// A filter in either JSON shape, as one condition.
    fn filter(&mut self, value: Value) -> ApiResult<String> {
        if is_rule_tree(&value) {
            let node: FilterNode = serde_json::from_value(value).map_err(|e| bad(format!("invalid filter rule: {e}")))?;
            self.rule_tree(&node)
        } else {
            let map = value.as_object().ok_or_else(|| bad("filter must be a JSON object"))?;
            self.mongo(map)
        }
    }
}

/// Builds the query for the rows of `dataset_id`. `constraints` are what an access
/// token imposes: its filter always applies, on any field, and its page size caps `limit`.
pub fn build(
    dataset_id: &str,
    params: &QueryParams,
    spec: &[FieldSpec],
    default_limit: u32,
    max_limit: u32,
    constraints: &Constraints,
) -> ApiResult<BuiltQuery> {
    let mut builder = Builder { binds: vec![json!(dataset_id)], conditions: 0, spec, check_flags: true };
    let mut where_parts = vec!["dataset_id = ?".to_string()];
    if !params.include_invalid.unwrap_or(false) {
        where_parts.push("valid = 1".to_string());
    }
    if let Some(imposed) = &constraints.filter {
        builder.check_flags = false;
        let condition = builder.filter(imposed.clone()).map_err(|e| ApiError::Forbidden(format!("this access token's filter cannot be applied: {e}")))?;
        where_parts.push(condition);
        builder.check_flags = true;
        builder.conditions = 0;
    }

    let mut uses_filter = false;
    if let Some(filter) = params.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        let value: Value = serde_json::from_str(filter).map_err(|e| bad(format!("filter is not valid JSON: {e}")))?;
        where_parts.push(builder.filter(value)?);
        uses_filter = true;
    }

    let uses_search = match params.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(q) => {
            if !spec.iter().any(|s| s.searchable) {
                return Err(bad("this dataset has no searchable fields; mark fields as searchable in its mapping"));
            }
            let words = fts_query(q);
            if words.is_empty() {
                where_parts.push("0".to_string());
            } else {
                let p = builder.bind(json!(words));
                where_parts.push(format!("id IN (SELECT rowid FROM dataset_rows_fts WHERE dataset_rows_fts MATCH {p})"));
            }
            true
        }
        None => false,
    };

    let mut order = vec![];
    for entry in params.sort.as_deref().unwrap_or_default().split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (path, desc) = match entry.split_once(':') {
            Some((path, dir)) => (path.trim(), dir.trim().to_lowercase().starts_with("desc")),
            None => match entry.strip_prefix('-') {
                Some(path) => (path, true),
                None => (entry.strip_prefix('+').unwrap_or(entry), false),
            },
        };
        builder.check(path, "sort")?;
        // rows with nothing in the field come last, whichever way the rest are sorted
        let f = field_sql(path);
        order.push(format!("({f} IS NULL OR {f} = '') ASC, {f} {}", if desc { "DESC" } else { "ASC" }));
    }
    let uses_sort = !order.is_empty();
    order.push("row_num ASC".to_string());

    let max_limit = constraints.max_page_size.map_or(max_limit, |m| m.clamp(1, max_limit));
    let limit = params.limit.unwrap_or(default_limit).clamp(1, max_limit);
    let start = match (params.start, params.page) {
        (Some(start), _) => start,
        (None, Some(page)) => page.saturating_sub(1).saturating_mul(limit),
        (None, None) => 0,
    };
    Ok(BuiltQuery {
        where_sql: where_parts.join(" AND "),
        binds: builder.binds,
        order_sql: order.join(", "),
        start,
        limit,
        uses_filter,
        uses_sort,
        uses_search,
    })
}

/// Keeps only `fields` of a row: top-level keys or dotted paths.
pub fn project(row: &Map<String, Value>, fields: &[String]) -> Map<String, Value> {
    fn place(target: &mut Map<String, Value>, segments: &[&str], value: &Value) {
        match segments {
            [] => {}
            [leaf] => {
                target.insert(leaf.to_string(), value.clone());
            }
            [first, rest @ ..] => {
                if let Some(inner) = target.entry(*first).or_insert_with(|| json!({})).as_object_mut() {
                    place(inner, rest, value);
                }
            }
        }
    }
    let mut out = Map::new();
    for path in fields {
        if let Some(value) = crate::importer::value_at(row, path) {
            place(&mut out, &path.split('.').collect::<Vec<_>>(), value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(filter: Option<Value>, sort: Option<&str>) -> QueryParams {
        QueryParams { filter: filter.map(|f| f.to_string()), sort: sort.map(str::to_string), ..Default::default() }
    }

    fn built(filter: Option<Value>, sort: Option<&str>) -> BuiltQuery {
        build("ds", &params(filter, sort), &[], 100, 1000, &Constraints::default()).unwrap()
    }

    #[test]
    fn builds_mongo_style_filters_with_bound_values() {
        let q = built(
            Some(json!({ "price": { "$gte": 10, "$lt": 20 }, "country": { "$in": ["GB"] }, "$or": [{ "a": 1 }, { "b.c": "x" }] })),
            Some("-price,name"),
        );
        let sql = q.rows_sql();
        let price = r#"(data ->> '$."price"')"#;
        assert!(sql.contains(&format!("(typeof({price}) IN ('integer', 'real') AND {price} >= ?)")), "{sql}");
        assert!(sql.contains(r#"(data ->> '$."country"') IN (?)"#), "{sql}");
        assert!(sql.contains(r#"(data ->> '$."b"."c"') IS ?"#), "{sql}");
        assert!(sql.contains(r#", (data ->> '$."price"') DESC, ((data ->> '$."name"') IS NULL OR (data ->> '$."name"') = '') ASC, (data ->> '$."name"') ASC, row_num ASC LIMIT 100 OFFSET 0"#), "{sql}");
        assert_eq!(q.binds, vec![json!("ds"), json!(10), json!(20), json!("GB"), json!(1), json!("x")]);
        assert!(q.uses_filter && q.uses_sort && !q.uses_search);
    }

    #[test]
    fn builds_rule_trees_like_the_import_filter() {
        let q = built(
            Some(json!({ "any": [{ "field": "name", "op": "starts_with", "value": "Wid" }, { "not": { "field": "qty", "op": "between", "value": [1, 5] } }] })),
            None,
        );
        assert!(q.where_sql.contains(r#"instr(lower_u(coalesce((data ->> '$."name"'), '')), ?) = 1"#), "{}", q.where_sql);
        assert!(q.where_sql.contains("NOT (coalesce(("), "{}", q.where_sql);
        assert_eq!(q.binds, vec![json!("ds"), json!("wid"), json!(1), json!(5)]);
    }

    #[test]
    fn rejects_unsafe_paths_unknown_operators_and_disallowed_fields() {
        let spec = vec![FieldSpec {
            path: "secret".into(),
            field_type: "string".into(),
            required: false,
            searchable: false,
            filterable: false,
            sortable: false,
        }];
        for (filter, sort) in [
            (Some(json!({ "a;DELETE": 1 })), None),
            (Some(json!({ "a'": 1 })), None),
            (Some(json!({ "a": { "$where": 1 } })), None),
            (Some(json!({ "secret": 1 })), None),
            (None, Some("secret")),
            (None, Some("data\"x")),
        ] {
            assert!(build("ds", &params(filter.clone(), sort), &spec, 100, 1000, &Constraints::default()).is_err(), "{filter:?} {sort:?}");
        }
    }

    #[test]
    fn paginates_by_page_or_start() {
        let p = QueryParams { page: Some(3), limit: Some(20), ..Default::default() };
        let q = build("ds", &p, &[], 100, 1000, &Constraints::default()).unwrap();
        assert_eq!((q.start, q.limit), (40, 20));
        let p = QueryParams { limit: Some(5000), ..Default::default() };
        assert_eq!(build("ds", &p, &[], 100, 1000, &Constraints::default()).unwrap().limit, 1000);
    }

    #[test]
    fn a_token_imposes_its_filter_page_size_and_fields() {
        let spec = vec![FieldSpec {
            path: "region".into(),
            field_type: "string".into(),
            required: false,
            searchable: false,
            filterable: false,
            sortable: true,
        }];
        let constraints = Constraints { filter: Some(json!({ "region": "north" })), fields: None, max_page_size: Some(25) };
        let q = build("ds", &QueryParams { limit: Some(500), ..Default::default() }, &spec, 100, 1000, &constraints).unwrap();
        assert!(q.where_sql.contains(r#"(data ->> '$."region"') IS ?"#), "an imposed filter may use a field callers may not");
        assert_eq!((q.limit, q.uses_filter), (25, false));

        let row = json!({ "sku": "A", "price": { "amount": 2, "currency": "GBP" }, "cost": 1 });
        let kept = project(row.as_object().unwrap(), &["sku".into(), "price.amount".into(), "missing".into()]);
        assert_eq!(Value::Object(kept), json!({ "sku": "A", "price": { "amount": 2 } }));
    }

    #[test]
    fn search_words_are_quoted() {
        assert_eq!(fts_query(r#"red "widget" OR x*"#), r#""red" "widget" "OR" "x""#);
        assert_eq!(fts_query(" -- "), "");
    }
}
