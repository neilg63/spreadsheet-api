//! Finds the pattern rules a sheet's own column names invite: columns that start the
//! same (`sales_west`, `sales_east`) or end the same (`beef_consumption_per_capita`,
//! `fish_consumption_per_capita`). Each suggestion is a ready-made `patterns` entry of a
//! mapping document, for a client to offer as is or as a starting point.
//!
//! Names are compared a word at a time, split at `_`, so `sale_price` and `sales_tax`
//! share nothing.

use serde::Serialize;
use std::collections::BTreeMap;

/// The most suggestions one sheet gets, the widest groups first.
const MAX_SUGGESTIONS: usize = 12;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PatternSuggestion {
    /// The source pattern: `sales_{name}`, `{name}_price`, `file_[n]`.
    pub source: String,
    /// The target that goes with it: `sales.{name}`, `files[]`, `sales[].{year}{value}`.
    pub target: String,
    /// Where the text the columns share stands in their names.
    pub shared: Shared,
    /// The text the columns share, without the `_` that joins it to the rest.
    pub shared_text: String,
    /// The name the pattern gives the part that differs.
    pub capture: String,
    /// The columns the pattern matches, in sheet order.
    pub columns: Vec<String>,
    /// The part of each column's name that differs, in the same order.
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Shared {
    Start,
    End,
}

/// The pattern rules `natural_keys` suggest. A group needs two columns; where the same
/// columns share more than one stretch of text on a side, the longest wins.
pub fn suggest(natural_keys: &[String]) -> Vec<PatternSuggestion> {
    let mut starts: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut ends: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, key) in natural_keys.iter().enumerate() {
        for (cut, _) in key.match_indices('_') {
            let (start, end) = (&key[..cut], &key[cut + 1..]);
            // a doubled or outer underscore leaves nothing to share or nothing to capture
            if start.is_empty() || end.is_empty() || start.ends_with('_') || end.starts_with('_') {
                continue;
            }
            starts.entry(start).or_default().push(i);
            ends.entry(end).or_default().push(i);
        }
    }

    let mut suggestions: Vec<PatternSuggestion> = Vec::new();
    for (shared, groups) in [(Shared::Start, starts), (Shared::End, ends)] {
        let mut longest: BTreeMap<Vec<usize>, &str> = BTreeMap::new();
        for (text, members) in groups {
            if members.len() < 2 {
                continue;
            }
            let kept = longest.entry(members).or_insert(text);
            if text.len() > kept.len() {
                *kept = text;
            }
        }
        for (members, text) in longest {
            let columns: Vec<String> = members.iter().map(|i| natural_keys[*i].clone()).collect();
            let values: Vec<String> = columns
                .iter()
                .map(|c| match shared {
                    Shared::Start => c[text.len() + 1..].to_string(),
                    Shared::End => c[..c.len() - text.len() - 1].to_string(),
                })
                .collect();
            suggestions.push(build(shared, text, columns, values));
        }
    }
    suggestions.sort_by(|a, b| {
        b.columns.len().cmp(&a.columns.len()).then(b.shared_text.len().cmp(&a.shared_text.len())).then(a.source.cmp(&b.source))
    });
    suggestions.truncate(MAX_SUGGESTIONS);
    suggestions
}

/// Years become a list of `{year, value}` objects, other numbers a plain list, and
/// anything else an object keyed by the part that differs.
fn build(shared: Shared, text: &str, columns: Vec<String>, values: Vec<String>) -> PatternSuggestion {
    let numbers: Option<Vec<i64>> = values.iter().map(|v| v.parse().ok()).collect();
    let (capture, placeholder, target) = match numbers {
        Some(n) if n.iter().all(|y| (1900..=2100).contains(y)) => ("year", "[year]".to_string(), format!("{text}[].{{year}}{{value}}")),
        Some(_) => ("n", "[n]".to_string(), format!("{}[]", plural(text))),
        None => ("name", "{name}".to_string(), format!("{text}.{{name}}")),
    };
    let source = match shared {
        Shared::Start => format!("{text}_{placeholder}"),
        Shared::End => format!("{placeholder}_{text}"),
    };
    PatternSuggestion { source, target, shared, shared_text: text.to_string(), capture: capture.to_string(), columns, values }
}

fn plural(name: &str) -> String {
    if name.ends_with('s') { name.to_string() } else { format!("{name}s") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn finds_columns_that_start_or_end_the_same() {
        let keys = strs(&["country", "sales_west", "sales_east", "beef_consumption_per_capita", "fish_consumption_per_capita"]);
        let found = suggest(&keys);
        let pairs: Vec<(&str, &str)> = found.iter().map(|s| (s.source.as_str(), s.target.as_str())).collect();
        assert_eq!(pairs, [("{name}_consumption_per_capita", "consumption_per_capita.{name}"), ("sales_{name}", "sales.{name}")]);
        assert_eq!(found[0].values, strs(&["beef", "fish"]));
        assert_eq!(found[1].columns, strs(&["sales_west", "sales_east"]));
        assert_eq!(found[1].shared, Shared::Start);
    }

    #[test]
    fn numbers_suggest_lists_and_years_suggest_records() {
        let found = suggest(&strs(&["file_1", "file_2", "file_3", "sales_2024", "sales_2025"]));
        let pairs: Vec<(&str, &str)> = found.iter().map(|s| (s.source.as_str(), s.target.as_str())).collect();
        assert_eq!(pairs, [("file_[n]", "files[]"), ("sales_[year]", "sales[].{year}{value}")]);
    }

    #[test]
    fn names_are_compared_a_word_at_a_time_and_nested_groups_are_both_offered() {
        assert!(suggest(&strs(&["sale_price", "sales_tax", "id", "a", "b"])).is_empty());
        let found = suggest(&strs(&["q1_sales_north", "q1_sales_south", "q1_costs_north"]));
        let sources: Vec<&str> = found.iter().map(|s| s.source.as_str()).collect();
        assert_eq!(sources, ["q1_{name}", "q1_sales_{name}", "{name}_north"]);
    }

    #[test]
    fn every_suggestion_is_a_pattern_the_mapper_accepts() {
        let keys = strs(&["sales_west", "sales_east", "beef_per_capita", "fish_per_capita", "file_1", "file_2", "gdp_2020", "gdp_2021"]);
        for s in suggest(&keys) {
            let columns = crate::sheet::key_pattern::expand_pattern_entry(&s.source, &s.target, &spreadsheet_to_json::Format::Auto, &keys)
                .unwrap_or_else(|| panic!("{} -> {} was rejected", s.source, s.target));
            let matched: Vec<String> = columns.iter().map(|c| c.source_key_name()).collect();
            assert_eq!(matched, s.columns, "{}", s.source);
        }
    }
}
