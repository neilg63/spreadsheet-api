//! Field specs and the JSON Schema definition stored in api_schema_versions.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use spreadsheet_to_json::indexmap::IndexMap;

use super::mapping::{MappingDoc, ResolvedMapping};

/// One entry of api_schema_versions.field_spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldSpec {
    pub path: String,
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub searchable: bool,
    #[serde(default = "yes")]
    pub filterable: bool,
    #[serde(default = "yes")]
    pub sortable: bool,
}

fn yes() -> bool {
    true
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && s.chars().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn value_type(value: &Value) -> Option<&'static str> {
    Some(match value {
        Value::Null | Value::Object(_) => return None,
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::String(s) if is_date(s) => "date",
        Value::String(s) if s.len() > 10 && is_date(&s[..10]) && s.as_bytes()[10] == b'T' => "datetime",
        Value::String(_) => "string",
        Value::Array(_) => "array",
    })
}

fn merge_types(a: &'static str, b: &'static str) -> &'static str {
    match (a, b) {
        _ if a == b => a,
        ("integer", "number") | ("number", "integer") => "number",
        ("date", "datetime") | ("datetime", "date") => "datetime",
        _ => "string",
    }
}

/// Infers leaf paths and types from mapped rows. Nested objects are walked; arrays are leaves.
#[derive(Debug, Default, Clone)]
pub struct SpecInference {
    types: IndexMap<String, Option<&'static str>>,
}

impl SpecInference {
    pub fn observe(&mut self, row: &Map<String, Value>) {
        self.walk("", row);
    }

    fn walk(&mut self, prefix: &str, obj: &Map<String, Value>) {
        for (key, value) in obj {
            let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
            if let Value::Object(inner) = value {
                self.walk(&path, inner);
                continue;
            }
            let observed = value_type(value);
            let slot = self.types.entry(path).or_insert(None);
            *slot = match (*slot, observed) {
                (None, t) => t,
                (Some(t), None) => Some(t),
                (Some(a), Some(b)) => Some(merge_types(a, b)),
            };
        }
    }

    /// Inferred types, overridden by what the mapping declares.
    pub fn finish(&self, doc: &MappingDoc, resolved: Option<&ResolvedMapping>) -> Vec<FieldSpec> {
        let mut specs: IndexMap<String, FieldSpec> = self
            .types
            .iter()
            .map(|(path, t)| {
                let spec = FieldSpec {
                    path: path.clone(),
                    field_type: t.unwrap_or("string").to_string(),
                    required: false,
                    searchable: false,
                    filterable: true,
                    sortable: true,
                };
                (path.clone(), spec)
            })
            .collect();
        if let Some(resolved) = resolved {
            for (index, path) in &resolved.field_paths {
                let field = &doc.fields[*index];
                let spec = specs.entry(path.clone()).or_insert_with(|| FieldSpec {
                    path: path.clone(),
                    field_type: "string".into(),
                    required: false,
                    searchable: false,
                    filterable: true,
                    sortable: true,
                });
                if let Some(t) = field.field_type.and_then(|t| t.spec_type()) {
                    spec.field_type = t.to_string();
                }
                spec.required |= field.required;
                spec.searchable = field.searchable.unwrap_or(spec.searchable);
                spec.filterable = field.filterable.unwrap_or(spec.filterable);
                spec.sortable = field.sortable.unwrap_or(spec.sortable);
            }
        }
        for path in &doc.primary_key {
            let spec = specs.entry(path.clone()).or_insert_with(|| FieldSpec {
                path: path.clone(),
                field_type: "string".into(),
                required: true,
                searchable: false,
                filterable: true,
                sortable: true,
            });
            spec.required = true;
        }
        specs.into_values().collect()
    }
}

pub fn infer(rows: &[Value], doc: &MappingDoc, resolved: Option<&ResolvedMapping>) -> Vec<FieldSpec> {
    let mut inference = SpecInference::default();
    for row in rows {
        if let Value::Object(obj) = row {
            inference.observe(obj);
        }
    }
    inference.finish(doc, resolved)
}

fn schema_type(field_type: &str) -> Value {
    match field_type {
        "date" => json!({ "type": "string", "format": "date" }),
        "datetime" => json!({ "type": "string", "format": "date-time" }),
        "geo" => json!({ "type": "object" }),
        other => json!({ "type": other }),
    }
}

fn insert_property(node: &mut Value, segments: &[&str], spec: &FieldSpec) {
    let Some((first, rest)) = segments.split_first() else { return };
    if spec.required {
        let required = node.as_object_mut().unwrap().entry("required").or_insert_with(|| json!([]));
        if !required.as_array().unwrap().iter().any(|r| r == first) {
            required.as_array_mut().unwrap().push(json!(first));
        }
    }
    let properties = node["properties"].as_object_mut().unwrap();
    if rest.is_empty() {
        properties.insert(first.to_string(), schema_type(&spec.field_type));
    } else {
        let child = properties
            .entry(first.to_string())
            .or_insert_with(|| json!({ "type": "object", "properties": {} }));
        insert_property(child, rest, spec);
    }
}

fn property_at<'a>(node: &'a mut Value, segments: &[&str]) -> Option<&'a mut Value> {
    let Some((first, rest)) = segments.split_first() else { return Some(node) };
    let child = node.get_mut("properties")?.get_mut(*first)?;
    property_at(child, rest)
}

/// JSON Schema (draft 2020-12) for one output row, with OpenAPI-style `x-` extensions:
/// `x-source` (the column a property comes from), `x-primary-key`, and `x-mapping`
/// (the complete mapping document, so a version is self-describing).
pub fn json_schema(specs: &[FieldSpec], doc: &MappingDoc, resolved: Option<&ResolvedMapping>) -> Value {
    let mut root = json!({ "type": "object", "properties": {} });
    for spec in specs {
        let segments: Vec<&str> = spec.path.split('.').collect();
        insert_property(&mut root, &segments, spec);
    }
    if let Some(resolved) = resolved {
        for (index, path) in &resolved.field_paths {
            let field = &doc.fields[*index];
            let segments: Vec<&str> = path.split('.').collect();
            if let Some(obj) = property_at(&mut root, &segments).and_then(Value::as_object_mut) {
                obj.insert("x-source".into(), json!(field.source));
                if let Some(description) = &field.description {
                    obj.insert("description".into(), json!(description));
                }
                if let Some(default) = &field.default {
                    obj.insert("default".into(), default.clone());
                }
            }
        }
    }
    let obj = root.as_object_mut().unwrap();
    let mut definition = Map::new();
    definition.insert("$schema".into(), json!("https://json-schema.org/draft/2020-12/schema"));
    if let Some(title) = &doc.title {
        definition.insert("title".into(), json!(title));
    }
    if let Some(description) = &doc.description {
        definition.insert("description".into(), json!(description));
    }
    definition.append(obj);
    if !doc.primary_key.is_empty() {
        definition.insert("x-primary-key".into(), json!(doc.primary_key));
    }
    definition.insert("x-mapping".into(), serde_json::to_value(doc).unwrap_or_default());
    Value::Object(definition)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_and_merges_types_across_rows() {
        let rows = vec![
            json!({ "sku": "A1", "qty": 1, "price": 2, "when": "2026-01-02", "size": { "w": 1 }, "tags": ["x"], "note": null }),
            json!({ "sku": "A2", "qty": 3, "price": 2.5, "when": "2026-01-02T10:00:00.000Z", "size": { "w": "big" }, "tags": [], "note": null }),
        ];
        let doc: MappingDoc = serde_json::from_value(json!({ "primary_key": ["sku"] })).unwrap();
        let specs = infer(&rows, &doc, None);
        let types: Vec<(&str, &str, bool)> =
            specs.iter().map(|s| (s.path.as_str(), s.field_type.as_str(), s.required)).collect();
        assert_eq!(
            types,
            vec![
                ("sku", "string", true),
                ("qty", "integer", false),
                ("price", "number", false),
                ("when", "datetime", false),
                ("size.w", "string", false),
                ("tags", "array", false),
                ("note", "string", false),
            ]
        );
        let schema = json_schema(&specs, &doc, None);
        assert_eq!(schema["properties"]["size"]["properties"]["w"]["type"], "string");
        assert_eq!(schema["properties"]["when"]["format"], "date-time");
        assert_eq!(schema["required"], json!(["sku"]));
        assert_eq!(schema["x-primary-key"], json!(["sku"]));
    }
}
