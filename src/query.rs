//! Translates GET /datasets/{id} query parameters into a parameterised SurrealQL query.
//!
//! Filters come in either of two JSON shapes:
//! - MongoDB style: `{"price": {"$gte": 10}, "country": {"$in": ["GB", "IE"]}, "$or": [...]}`
//! - the rule tree used by the import filter, convenient for query-builder widgets:
//!   `{"all": [{"field": "price", "op": "gte", "value": 10}, {"any": [...]}]}`
//!
//! Field paths are validated identifiers embedded as quoted idioms (`data.`size`.`w``);
//! every value is a bound parameter, never query text.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
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
    pub rows_sql: String,
    pub count_sql: String,
    pub vars: Map<String, Value>,
    pub start: u32,
    pub limit: u32,
    pub uses_filter: bool,
    pub uses_sort: bool,
    pub uses_search: bool,
}

struct Builder<'a> {
    vars: Map<String, Value>,
    conditions: usize,
    spec: &'a [FieldSpec],
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::BadRequest(message.into())
}

impl<'a> Builder<'a> {
    fn bind(&mut self, value: Value) -> String {
        let name = format!("p{}", self.vars.len());
        self.vars.insert(name.clone(), value);
        format!("${name}")
    }

    fn field(&mut self, path: &str, purpose: &str) -> ApiResult<String> {
        if !is_valid_path(path) {
            return Err(bad(format!("'{path}' is not a field path (use letters, digits, underscores and dots)")));
        }
        if let Some(spec) = self.spec.iter().find(|s| s.path == path) {
            let allowed = if purpose == "sort" { spec.sortable } else { spec.filterable };
            if !allowed {
                return Err(ApiError::Forbidden(format!("'{path}' cannot be used to {purpose}")));
            }
        }
        self.conditions += 1;
        if self.conditions > MAX_CONDITIONS {
            return Err(bad(format!("a query can use at most {MAX_CONDITIONS} conditions")));
        }
        Ok(format!("data.{}", path.split('.').map(|s| format!("`{s}`")).collect::<Vec<_>>().join(".")))
    }

    fn lower_text(field: &str) -> String {
        format!("string::lowercase(<string>({field} ?? ''))")
    }

    /// One comparison on one field.
    fn compare(&mut self, path: &str, op: &str, value: &Value, case_sensitive: bool) -> ApiResult<String> {
        let f = self.field(path, "filter")?;
        let text_op = |this: &mut Self, sql_fn: &str| -> ApiResult<String> {
            let text = value.as_str().ok_or_else(|| bad(format!("'{op}' on '{path}' needs a string")))?;
            Ok(if case_sensitive {
                let p = this.bind(json!(text));
                format!("{sql_fn}(<string>({f} ?? ''), {p})")
            } else {
                let p = this.bind(json!(text.to_lowercase()));
                format!("{sql_fn}({}, {p})", Self::lower_text(&f))
            })
        };
        let list = |this: &mut Self| -> ApiResult<String> {
            match value {
                Value::Array(items) if !items.is_empty() => Ok(this.bind(value.clone())),
                _ => Err(bad(format!("'{op}' on '{path}' needs a non-empty list"))),
            }
        };
        let not_empty = format!("({f} != NONE AND {f} != NULL AND {f} != '')");
        Ok(match op {
            "eq" if value.is_string() && !case_sensitive => {
                let p = self.bind(json!(value.as_str().unwrap_or_default().to_lowercase()));
                format!("{} = {p}", Self::lower_text(&f))
            }
            "eq" => format!("{f} = {}", self.bind(value.clone())),
            "ne" => format!("{f} != {}", self.bind(value.clone())),
            "gt" => format!("{f} > {}", self.bind(value.clone())),
            "gte" => format!("{f} >= {}", self.bind(value.clone())),
            "lt" => format!("{f} < {}", self.bind(value.clone())),
            "lte" => format!("{f} <= {}", self.bind(value.clone())),
            "between" | "not_between" => {
                let Some([lo, hi]) = value.as_array().map(|a| a.as_slice()).and_then(|a| <&[Value; 2]>::try_from(a).ok()) else {
                    return Err(bad(format!("'{op}' on '{path}' needs a [low, high] pair")));
                };
                let (lo, hi) = (self.bind(lo.clone()), self.bind(hi.clone()));
                let range = format!("({f} >= {lo} AND {f} <= {hi})");
                if op == "between" { range } else { format!("!{range}") }
            }
            "starts_with" => text_op(self, "string::starts_with")?,
            "ends_with" => text_op(self, "string::ends_with")?,
            "contains" => text_op(self, "string::contains")?,
            "regex" => {
                let pattern = value.as_str().ok_or_else(|| bad(format!("'regex' on '{path}' needs a string")))?;
                let pattern = if case_sensitive { pattern.to_string() } else { format!("(?i){pattern}") };
                format!("string::matches(<string>({f} ?? ''), {})", self.bind(json!(pattern)))
            }
            "in" => format!("{f} INSIDE {}", list(self)?),
            "not_in" => format!("{f} NOTINSIDE {}", list(self)?),
            "any" => format!("{f} CONTAINSANY {}", list(self)?),
            "all" => format!("{f} CONTAINSALL {}", list(self)?),
            "has" => format!("{f} CONTAINS {}", self.bind(value.clone())),
            "size" => format!("array::len({f} ?? []) = {}", self.bind(value.clone())),
            "not_empty" => not_empty,
            "empty" => format!("!{not_empty}"),
            "exists" => {
                let exists = format!("({f} != NONE AND {f} != NULL)");
                if value.as_bool().unwrap_or(true) { exists } else { format!("!{exists}") }
            }
            other => return Err(bad(format!("unknown operator '{other}'"))),
        })
    }

    fn join(parts: Vec<String>, glue: &str) -> String {
        match parts.len() {
            0 => "true".to_string(),
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
            FilterNode::Not { not } => format!("!({})", self.rule_tree(not)?),
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
                "$nor" => format!("!{}", logical(self, " OR ")?),
                "$not" => {
                    let inner = value.as_object().ok_or_else(|| bad("'$not' needs an object"))?;
                    format!("!({})", self.mongo(inner)?)
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

pub fn build(params: &QueryParams, spec: &[FieldSpec], default_limit: u32, max_limit: u32) -> ApiResult<BuiltQuery> {
    let mut builder = Builder { vars: Map::new(), conditions: 0, spec };
    let mut where_parts = vec!["dataset = type::record('datasets', $dataset)".to_string()];
    if !params.include_invalid.unwrap_or(false) {
        where_parts.push("valid = true".to_string());
    }

    let mut uses_filter = false;
    if let Some(filter) = params.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        let value: Value = serde_json::from_str(filter).map_err(|e| bad(format!("filter is not valid JSON: {e}")))?;
        let condition = if is_rule_tree(&value) {
            let node: FilterNode = serde_json::from_value(value).map_err(|e| bad(format!("invalid filter rule: {e}")))?;
            builder.rule_tree(&node)?
        } else {
            let map = value.as_object().ok_or_else(|| bad("filter must be a JSON object"))?;
            builder.mongo(map)?
        };
        where_parts.push(condition);
        uses_filter = true;
    }

    let uses_search = match params.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(q) => {
            if !spec.iter().any(|s| s.searchable) {
                return Err(bad("this dataset has no searchable fields; mark fields as searchable in its mapping"));
            }
            let p = builder.bind(json!(q));
            where_parts.push(format!("search_text @@ {p}"));
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
        let field = builder.field(path, "sort")?;
        order.push(format!("{field} {}", if desc { "DESC" } else { "ASC" }));
    }
    let uses_sort = !order.is_empty();
    order.push("row_num ASC".to_string());

    let limit = params.limit.unwrap_or(default_limit).clamp(1, max_limit);
    let start = match (params.start, params.page) {
        (Some(start), _) => start,
        (None, Some(page)) => page.saturating_sub(1).saturating_mul(limit),
        (None, None) => 0,
    };
    let where_sql = where_parts.join(" AND ");
    Ok(BuiltQuery {
        rows_sql: format!(
            "SELECT VALUE data FROM dataset_rows WHERE {where_sql} ORDER BY {} LIMIT {limit} START {start};",
            order.join(", ")
        ),
        count_sql: format!("RETURN (SELECT count() FROM dataset_rows WHERE {where_sql} GROUP ALL)[0].count ?? 0;"),
        vars: builder.vars,
        start,
        limit,
        uses_filter,
        uses_sort,
        uses_search,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(filter: Option<Value>, sort: Option<&str>) -> QueryParams {
        QueryParams { filter: filter.map(|f| f.to_string()), sort: sort.map(str::to_string), ..Default::default() }
    }

    #[test]
    fn builds_mongo_style_filters_with_bound_values() {
        let built = build(
            &params(Some(json!({ "price": { "$gte": 10, "$lt": 20 }, "country": { "$in": ["GB"] }, "$or": [{ "a": 1 }, { "b.c": "x" }] })), Some("-price,name")),
            &[],
            100,
            1000,
        )
        .unwrap();
        assert!(built.rows_sql.contains("(data.`price` >= $p0 AND data.`price` < $p1)"), "{}", built.rows_sql);
        assert!(built.rows_sql.contains("data.`country` INSIDE $p2"));
        assert!(built.rows_sql.contains("(data.`a` = $p3 OR data.`b`.`c` = $p4)"));
        assert!(built.rows_sql.contains("ORDER BY data.`price` DESC, data.`name` ASC, row_num ASC"));
        assert_eq!(built.vars["p4"], json!("x"));
        assert!(built.uses_filter && built.uses_sort && !built.uses_search);
    }

    #[test]
    fn builds_rule_trees_like_the_import_filter() {
        let built = build(
            &params(Some(json!({ "any": [{ "field": "name", "op": "starts_with", "value": "Wid" }, { "not": { "field": "qty", "op": "between", "value": [1, 5] } }] })), None),
            &[],
            100,
            1000,
        )
        .unwrap();
        assert!(built.rows_sql.contains("string::starts_with(string::lowercase(<string>(data.`name` ?? '')), $p0)"));
        assert!(built.rows_sql.contains("!((data.`qty` >= $p1 AND data.`qty` <= $p2))"));
        assert_eq!(built.vars["p0"], json!("wid"));
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
            (Some(json!({ "a": { "$where": 1 } })), None),
            (Some(json!({ "secret": 1 })), None),
            (None, Some("secret")),
            (None, Some("data`x")),
        ] {
            assert!(build(&params(filter.clone(), sort), &spec, 100, 1000).is_err(), "{filter:?} {sort:?}");
        }
    }

    #[test]
    fn paginates_by_page_or_start() {
        let p = QueryParams { page: Some(3), limit: Some(20), ..Default::default() };
        let built = build(&p, &[], 100, 1000).unwrap();
        assert_eq!((built.start, built.limit), (40, 20));
        let p = QueryParams { limit: Some(5000), ..Default::default() };
        assert_eq!(build(&p, &[], 100, 1000).unwrap().limit, 1000);
    }
}
