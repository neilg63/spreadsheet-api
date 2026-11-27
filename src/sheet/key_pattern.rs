//! Started as a port of spread-cli's src/key_pattern.rs
//! (https://github.com/neilg63/spreadsheet_to_json_cli, MIT), and accepts everything
//! that does. It has since gone further than upstream: a capture may stand anywhere in
//! the source pattern, there may be several, and a target key may refer to them.
//!
//! Placeholder patterns map every column whose name fits to one place in the output:
//!
//! - `file_[n]` -> `files[]` -- sequential columns into a plain array (`KeySegment::PlainArray`)
//! - `sales_{region}` -> `sales.{region}` -- columns into an object keyed by the capture
//! - `sales_[year]` -> `sales[].{year}{amount}` -- columns into an array of
//!   `{year, amount}` items (`KeySegment::Array` + `KeySegment::InnerObject`); the `.`
//!   may be left out, `sales[]{year}{amount}`
//! - `{fuel}_price` -> `price.{fuel}` -- the same, for columns that share an ending
//! - `{fuel}_price_note_{n}` -> `price_{fuel}` -- a rename that keeps one capture and
//!   drops the other
//!
//! `[name]`/`{name}` in the *source* pattern is a capture: `[]` types it as an integer
//! (`Identifier::Integer`), `{}` as a string (`Identifier::String`) -- the mnemonic being
//! JSON's own shape (arrays are int-indexed, objects are string-keyed). The type only
//! matters when the capture is referenced on the target side. `*` and `[]` are an
//! anonymous capture that nothing refers to: `download_*`, `download_[]`.
//!
//! On the *target* side, `{name}` refers to the capture of that name. Inside an array's
//! brace group a `{name}` that matches no capture is the leaf -- the cell's own value
//! lands there, under a field literally named `name`. This needs the real column
//! headers to expand against (`natural_keys`).

use enclose_strings::SimpleExtract;
use spreadsheet_to_json::{Column, DateTimeMode, Format, Identifier, KeySegment};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq)]
enum CaptureType {
    Int,
    Str,
}

/// Not a name any target spec would realistically use, so there's no real risk of it
/// colliding with a genuine `{name}` reference.
const ANONYMOUS_CAPTURE: &str = "*";

/// One piece of a source pattern: text every matching column shares, or a capture.
#[derive(Debug, Clone, PartialEq)]
enum SourcePart {
    Literal(String),
    Capture { name: String, capture_type: CaptureType },
}

/// The captures of one matched column, by name, in the order the source pattern has them.
type Captures = [(String, Identifier)];

fn capture<'a>(captures: &'a Captures, name: &str) -> Option<&'a Identifier> {
    captures.iter().find(|(n, _)| n == name).map(|(_, id)| id)
}

/// Splits `source` into literals and captures. A capture may stand anywhere: after the
/// shared text (`sales_[year]`), before it (`{fuel}_price`), or both
/// (`{fuel}_price_note_{n}`). `None` if `source` has no capture at all (callers fall back
/// to exact-match handling), no literal text at all (it would match every column), two
/// captures with nothing between them (where one ends is undecidable), a named capture
/// used twice, or an unclosed bracket.
fn parse_source_pattern(source: &str) -> Option<Vec<SourcePart>> {
    let lower = source.to_lowercase();
    let mut parts: Vec<SourcePart> = Vec::new();
    let mut literal = String::new();
    let mut rest = lower.as_str();
    while let Some(ch) = rest.chars().next() {
        let found = match ch {
            // Glob-style shorthand for an anonymous, purely positional capture.
            '*' => Some((ANONYMOUS_CAPTURE.to_string(), CaptureType::Str, 1)),
            '[' => {
                let close = rest.find(']')?;
                let name = &rest[1..close];
                // "download_[]" is the same anonymous capture as "download_*"
                Some(if name.is_empty() { (ANONYMOUS_CAPTURE.to_string(), CaptureType::Str, close + 1) } else { (name.to_string(), CaptureType::Int, close + 1) })
            }
            '{' => {
                let close = rest.find('}')?;
                let name = &rest[1..close];
                if name.is_empty() {
                    return None;
                }
                Some((name.to_string(), CaptureType::Str, close + 1))
            }
            _ => None,
        };
        match found {
            Some((name, capture_type, consumed)) => {
                if !literal.is_empty() {
                    parts.push(SourcePart::Literal(std::mem::take(&mut literal)));
                } else if matches!(parts.last(), Some(SourcePart::Capture { .. })) {
                    return None;
                }
                let repeated = name != ANONYMOUS_CAPTURE && parts.iter().any(|p| matches!(p, SourcePart::Capture { name: n, .. } if *n == name));
                if repeated {
                    return None;
                }
                parts.push(SourcePart::Capture { name, capture_type });
                rest = &rest[consumed..];
            }
            None => {
                literal.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
    }
    if !literal.is_empty() {
        parts.push(SourcePart::Literal(literal));
    }
    let has = |capture: bool| parts.iter().any(|p| matches!(p, SourcePart::Capture { .. }) == capture);
    (has(true) && has(false)).then_some(parts)
}

/// Matches `text` against `parts`, pushing each capture's text onto `out` in order.
/// A capture is never empty and takes as little as it can, so `{a}_x_{b}` splits
/// `p_x_q_x_r` into `p` and `q_x_r`.
fn match_parts<'a>(parts: &[SourcePart], text: &'a str, out: &mut Vec<&'a str>) -> bool {
    let Some((first, rest)) = parts.split_first() else { return text.is_empty() };
    match first {
        SourcePart::Literal(literal) => text.strip_prefix(literal.as_str()).is_some_and(|after| match_parts(rest, after, out)),
        SourcePart::Capture { .. } => match rest.first() {
            None => {
                if text.is_empty() {
                    return false;
                }
                out.push(text);
                true
            }
            Some(SourcePart::Literal(literal)) => {
                for (pos, _) in text.match_indices(literal.as_str()).filter(|(pos, _)| *pos > 0) {
                    out.push(&text[..pos]);
                    if match_parts(rest, &text[pos..], out) {
                        return true;
                    }
                    out.pop();
                }
                false
            }
            Some(SourcePart::Capture { .. }) => false, // parse_source_pattern never produces this
        },
    }
}

/// The captures `natural_key` gives for `parts`, or `None` if it does not match.
fn capture_values(parts: &[SourcePart], natural_key: &str) -> Option<Vec<(String, Identifier)>> {
    let mut values = Vec::new();
    if !match_parts(parts, natural_key, &mut values) {
        return None;
    }
    let captures = parts.iter().filter_map(|p| match p {
        SourcePart::Capture { name, capture_type } => Some((name, capture_type)),
        SourcePart::Literal(_) => None,
    });
    Some(
        captures
            .zip(values)
            .map(|((name, capture_type), value)| {
                let id = match capture_type {
                    CaptureType::Int => Identifier::from_int_str(value),
                    CaptureType::Str => Identifier::from_string(value),
                };
                (name.clone(), id)
            })
            .collect(),
    )
}

/// One `.`-separated segment of a target-path spec.
enum TargetToken {
    /// A key to descend into (or create), e.g. the "sales" in "sales.{region}", or the
    /// field itself when it comes last. It may hold `{name}` references to captures,
    /// filled in per column: `price_{fuel}`.
    Literal(String),
    /// `name[]` -- opens an array. Terminal (`files[]` alone) means `PlainArray`;
    /// followed by a `Braces` segment means `Array` + possibly `InnerObject`.
    ArrayOpen(String),
    /// One or more adjacent `{name}` groups with no separator between them, e.g.
    /// `{year}{amount}` is `Braces(["year", "amount"])`.
    Braces(Vec<String>),
}

/// Splits `target` on `.` and classifies each segment. `None` if any segment is empty.
/// The `.` between a list and its item may be left out: `sales[]{cc}{value}` reads the
/// same as `sales[].{cc}{value}`.
fn parse_target_tokens(target: &str) -> Option<Vec<TargetToken>> {
    target.trim().replace("[]{", "[].{").split('.').map(parse_target_segment).collect()
}

fn parse_target_segment(segment: &str) -> Option<TargetToken> {
    if let Some(name) = segment.strip_suffix("[]") {
        if !name.is_empty() {
            return Some(TargetToken::ArrayOpen(name.to_string()));
        }
    }
    if segment.starts_with('{') {
        // "{fuel}_price" is not a run of brace groups: it is a key with a reference in it
        if let Some(groups) = parse_brace_groups(segment) {
            return Some(TargetToken::Braces(groups));
        }
    }
    if !segment.is_empty() {
        return Some(TargetToken::Literal(segment.to_string()));
    }
    None
}

/// Consumes a run of adjacent `{name}` groups (no nesting, no separator between them --
/// the shape this DSL deliberately stays within). `None` if `segment` isn't *entirely*
/// made of such groups.
fn parse_brace_groups(mut segment: &str) -> Option<Vec<String>> {
    let mut groups = Vec::new();
    while !segment.is_empty() {
        if !segment.starts_with('{') {
            return None;
        }
        let content = segment.extract_from_braces().filter(|c| !c.is_empty())?;
        let consumed = content.len() + 2; // the '{' and '}' themselves
        if consumed > segment.len() {
            return None;
        }
        segment = &segment[consumed..];
        groups.push(content);
    }
    if groups.is_empty() { None } else { Some(groups) }
}

/// Fills the `{name}` references in a key with the column's captures: `price_{fuel}`
/// becomes `price_petrol`. `None` if a reference names no capture or a brace is unclosed.
fn fill_key(template: &str, captures: &Captures) -> Option<String> {
    let mut key = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        key.push_str(&rest[..open]);
        let close = open + rest[open..].find('}')?;
        key.push_str(&capture(captures, &rest[open + 1..close])?.to_string());
        rest = &rest[close + 1..];
    }
    key.push_str(rest);
    Some(key)
}

/// Builds the concrete `KeySegment` for one matched column, given its captures. A
/// `{name}` on the target side is a reference to the capture of that name; inside an
/// array's brace group, a name that matches no capture is the leaf, a field literally
/// named `name` that receives the cell's own value.
fn build_key_segment(tokens: &[TargetToken], captures: &Captures) -> Option<KeySegment> {
    let (first, rest) = tokens.split_first()?;
    match first {
        TargetToken::Literal(template) => {
            let key = fill_key(template, captures)?;
            if rest.is_empty() {
                return Some(KeySegment::Simple(Arc::from(key.as_str())));
            }
            let next = build_key_segment(rest, captures)?;
            Some(KeySegment::Object(Arc::from(key.as_str()), Arc::new(next)))
        }
        TargetToken::ArrayOpen(template) => {
            let name = fill_key(template, captures)?;
            if rest.is_empty() {
                return Some(KeySegment::PlainArray(Arc::from(name.as_str())));
            }
            let (TargetToken::Braces(names), rest2) = rest.split_first()? else { return None };
            if !rest2.is_empty() {
                return None; // scoped grammar: nothing follows the array's brace-group segment
            }
            build_array_with_braces(&name, names, captures)
        }
        TargetToken::Braces(names) => {
            if !rest.is_empty() {
                return None; // braces are only a supported terminal outside an array context
            }
            let [name] = names.as_slice() else { return None };
            capture(captures, name).map(|id| KeySegment::Simple(Arc::from(id.to_string().as_str())))
        }
    }
}

/// `names[0]` becomes `Array`'s own discriminator (must be a capture -- that's what
/// identifies which real-world entity, e.g. a year, each item represents); any remaining
/// names chain as `InnerObject` bindings, ending in the first name that *isn't* a
/// capture (the leaf, the cell's own value).
fn build_array_with_braces(container: &str, names: &[String], captures: &Captures) -> Option<KeySegment> {
    let (first, rest) = names.split_first()?;
    let id = capture(captures, first)?;
    let chain = build_inner_object_chain(rest, captures)?;
    Some(KeySegment::Array(Arc::from(container), id.clone(), Arc::from(first.as_str()), Arc::new(chain)))
}

fn build_inner_object_chain(names: &[String], captures: &Captures) -> Option<KeySegment> {
    let (name, rest) = names.split_first()?;
    if let Some(id) = capture(captures, name) {
        let next = build_inner_object_chain(rest, captures)?;
        Some(KeySegment::InnerObject(id.clone(), Arc::from(name.as_str()), Arc::new(next)))
    } else if rest.is_empty() {
        Some(KeySegment::Simple(Arc::from(name.as_str())))
    } else {
        None // a name that is no capture, with more following, has no value to bind
    }
}

/// Expands one placeholder `--keys` entry (`source` containing at least one capture)
/// against the real, already-known natural column keys, producing one fully resolved
/// `Column` per matching column -- each with an exact `source_key` and a concrete
/// (non-templated) `KeySegment`. Returns `None` if `source` is not a usable pattern or
/// the target spec doesn't fit a supported shape.
pub fn expand_pattern_entry(source: &str, target: &str, format: &Format, natural_keys: &[String]) -> Option<Vec<Column>> {
    let parts = parse_source_pattern(source)?;
    let tokens = parse_target_tokens(target)?;

    // Whether build_key_segment succeeds depends only on the template's structure and
    // the capture names -- never on which values were captured -- so placeholder values
    // validate the whole target spec once, up front. Without this, a malformed target
    // would fail per column and every column would silently vanish.
    let placeholders: Vec<(String, Identifier)> = parts
        .iter()
        .filter_map(|p| match p {
            SourcePart::Capture { name, .. } => Some((name.clone(), Identifier::from_string("__validate__"))),
            SourcePart::Literal(_) => None,
        })
        .collect();
    build_key_segment(&tokens, &placeholders)?;

    let columns: Vec<Column> = natural_keys
        .iter()
        .filter_map(|natural_key| {
            let captures = capture_values(&parts, natural_key)?;
            let key_segment = build_key_segment(&tokens, &captures).expect("already validated the same template above");
            let mut col = Column::from_source_key_with_format(natural_key, None, format.clone(), None, DateTimeMode::Full, false);
            col.key = Some(key_segment);
            Some(col)
        })
        .collect();

    Some(columns)
}

/// The columns a source pattern matches, in sheet order, for rules that need no target
/// (leaving columns out). `None` if `source` is not a pattern.
pub fn matching_keys(source: &str, natural_keys: &[String]) -> Option<Vec<String>> {
    let parts = parse_source_pattern(source)?;
    Some(natural_keys.iter().filter(|key| capture_values(&parts, key).is_some()).cloned().collect())
}

/// Resolves a `--keys '-identifier'` column-suppression entry against the real, already
/// known column keys (`natural_keys`, from the same header-only peek pattern entries
/// use) -- matching either the natural (snake_cased) header key or, in `-a`/`-R` mode,
/// the A1 letter or R1C1 number those modes rename every column to. `natural_keys`
/// already reflects whichever colstyle is active by the time it's fetched (the peek read
/// uses the same `OptionSet`/`field_mode` as the real one), so this never needs to know
/// which mode is in effect -- it just matches against what's actually there, plus a
/// number/`cNN` fallback for R1C1 mode.
///
/// `None` if `identifier` doesn't match any column -- callers should silently ignore
/// this the same way an unmatched ordinary `--keys` source_key already is (a typo, or
/// the wrong sheet/file), not treat it as an error.
pub fn resolve_suppression_key(identifier: &str, natural_keys: &[String]) -> Option<String> {
    if let Some(found) = natural_keys.iter().find(|k| k.eq_ignore_ascii_case(identifier)) {
        return Some(found.clone());
    }
    // R1C1 fallback: an optional leading "c"/"C" (c4, c04, c004 are all the same column)
    // followed by a plain 1-based column number -- lets "-4" suppress column 4 without
    // needing to know the sheet's actual zero-padding width up front.
    let digits = identifier.strip_prefix(['c', 'C']).unwrap_or(identifier);
    let num: usize = digits.parse().ok()?;
    let idx = num.checked_sub(1)?;
    natural_keys.get(idx).cloned()
}

/// Builds the `KeySegment::Excluded` column that actually suppresses `natural_key` from
/// the output, once `resolve_suppression_key` has matched it against the real headers.
pub fn build_suppression_column(natural_key: &str) -> Column {
    let mut col = Column::from_source_key_with_format(natural_key, None, Format::Auto, None, DateTimeMode::Full, false);
    col.key = Some(KeySegment::Excluded);
    col
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(cols: &[Column]) -> Vec<String> {
        cols.iter().map(|c| c.source_key_name()).collect()
    }

    #[test]
    fn test_plain_array_pattern_matches_any_suffix_regardless_of_bracket_type() {
        let natural_keys = vec!["file_1".to_string(), "file_2".to_string(), "file_a".to_string(), "other".to_string()];
        let cols = expand_pattern_entry("file_[n]", "files[]", &Format::Auto, &natural_keys).unwrap();
        assert_eq!(keys(&cols), vec!["file_1".to_string(), "file_2".to_string(), "file_a".to_string()]);
        for col in &cols {
            assert!(matches!(&col.key, Some(KeySegment::PlainArray(name)) if &**name == "files"));
        }
    }

    #[test]
    fn test_glob_star_shorthand_is_equivalent_to_an_anonymous_bracket_capture() {
        let natural_keys = vec!["download_1".to_string(), "download_2".to_string(), "other".to_string()];
        let cols = expand_pattern_entry("download_*", "downloads[]", &Format::Auto, &natural_keys).unwrap();
        assert_eq!(keys(&cols), vec!["download_1".to_string(), "download_2".to_string()]);
        for col in &cols {
            assert!(matches!(&col.key, Some(KeySegment::PlainArray(name)) if &**name == "downloads"));
        }
    }

    #[test]
    fn test_empty_brackets_shorthand_is_equivalent_to_an_anonymous_bracket_capture() {
        let natural_keys = vec!["download_1".to_string(), "download_2".to_string(), "other".to_string()];
        let cols = expand_pattern_entry("download_[]", "downloads[]", &Format::Auto, &natural_keys).unwrap();
        assert_eq!(keys(&cols), vec!["download_1".to_string(), "download_2".to_string()]);
        for col in &cols {
            assert!(matches!(&col.key, Some(KeySegment::PlainArray(name)) if &**name == "downloads"));
        }
    }

    #[test]
    fn test_bare_star_or_empty_brackets_with_no_prefix_at_all_is_rejected() {
        // Matching every column in the sheet is almost certainly never intended.
        assert!(parse_source_pattern("*").is_none());
        assert!(parse_source_pattern("[]").is_none());
    }

    #[test]
    fn test_object_pattern_uses_captured_value_as_the_key() {
        let natural_keys = vec!["sales_north_west".to_string(), "sales_south_east".to_string()];
        let cols = expand_pattern_entry("sales_{region}", "sales.{region}", &Format::Float, &natural_keys).unwrap();
        assert_eq!(cols.len(), 2);
        let seg = cols[0].key.as_ref().unwrap();
        match seg {
            KeySegment::Object(key, next) => {
                assert_eq!(&**key, "sales");
                assert!(matches!(&**next, KeySegment::Simple(k) if &**k == "north_west"));
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    #[test]
    fn test_array_inner_object_pattern_types_the_capture_as_integer() {
        let natural_keys = vec!["sales_2024".to_string(), "sales_2025".to_string()];
        let cols = expand_pattern_entry("sales_[year]", "sales[].{year}{amount}", &Format::Float, &natural_keys).unwrap();
        assert_eq!(cols.len(), 2);
        let seg = cols[0].key.as_ref().unwrap();
        match seg {
            KeySegment::Array(container, id, key_field, next) => {
                assert_eq!(&**container, "sales");
                assert_eq!(*id, Identifier::Integer(2024));
                assert_eq!(&**key_field, "year");
                assert!(matches!(&**next, KeySegment::Simple(k) if &**k == "amount"));
            }
            other => panic!("expected Array, got {other:?}"),
        }
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_capture_before_the_shared_text_groups_by_suffix() {
        let natural_keys = strs(&["petrol_price", "diesel_price", "price", "petrol_tax"]);
        let cols = expand_pattern_entry("{fuel}_price", "price.{fuel}", &Format::Float, &natural_keys).unwrap();
        assert_eq!(keys(&cols), strs(&["petrol_price", "diesel_price"]));
        match cols[1].key.as_ref().unwrap() {
            KeySegment::Object(key, next) => {
                assert_eq!(&**key, "price");
                assert!(matches!(&**next, KeySegment::Simple(k) if &**k == "diesel"));
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    #[test]
    fn test_captures_at_both_ends_and_a_reference_inside_the_target_key() {
        let natural_keys = strs(&[
            "petrol_current_fuel_price_index_numbers_note_1",
            "solid_fuels_current_fuel_price_index_numbers_note_2",
            "petrol_real_terms_fuel_price_index_numbers_note_3",
        ]);
        let cols =
            expand_pattern_entry("{fuel}_current_fuel_price_index_numbers_note_{n}", "current_price_{fuel}", &Format::Float, &natural_keys).unwrap();
        assert_eq!(cols.len(), 2);
        assert!(matches!(cols[0].key.as_ref().unwrap(), KeySegment::Simple(k) if &**k == "current_price_petrol"));
        assert!(matches!(cols[1].key.as_ref().unwrap(), KeySegment::Simple(k) if &**k == "current_price_solid_fuels"));
    }

    #[test]
    fn test_capture_in_the_middle_and_an_integer_capture_at_the_start() {
        let natural_keys = strs(&["2024_sales", "2025_sales", "q1_north_total"]);
        let cols = expand_pattern_entry("[year]_sales", "sales[].{year}{amount}", &Format::Float, &natural_keys).unwrap();
        assert_eq!(cols.len(), 2);
        assert!(matches!(cols[0].key.as_ref().unwrap(), KeySegment::Array(c, id, f, _) if &**c == "sales" && *id == Identifier::Integer(2024) && &**f == "year"));

        let cols = expand_pattern_entry("q1_{region}_total", "totals.{region}", &Format::Float, &natural_keys).unwrap();
        assert_eq!(keys(&cols), strs(&["q1_north_total"]));
    }

    #[test]
    fn test_a_capture_takes_as_little_as_it_can_and_is_never_empty() {
        let parts = parse_source_pattern("{a}_x_{b}").unwrap();
        let mut out = Vec::new();
        assert!(match_parts(&parts, "p_x_q_x_r", &mut out));
        assert_eq!(out, vec!["p", "q_x_r"]);
        assert!(capture_values(&parts, "_x_r").is_none());
        assert!(capture_values(&parts, "p_x_").is_none());
    }

    #[test]
    fn test_unusable_source_patterns_and_unknown_references_are_rejected() {
        assert!(parse_source_pattern("{a}{b}_x").is_none()); // nothing between the captures
        assert!(parse_source_pattern("{a}_x_{a}").is_none()); // the same name twice
        assert!(parse_source_pattern("{a}").is_none()); // matches every column
        assert!(parse_source_pattern("x_{a").is_none()); // unclosed
        let natural_keys = strs(&["petrol_price"]);
        assert!(expand_pattern_entry("{fuel}_price", "price_{grade}", &Format::Auto, &natural_keys).is_none());
    }

    #[test]
    fn test_the_dot_between_a_list_and_its_item_is_optional() {
        let natural_keys = strs(&["sales_de", "sales_fr"]);
        let cols = expand_pattern_entry("sales_{cc}", "sales[]{cc}{value}", &Format::Float, &natural_keys).unwrap();
        assert!(matches!(cols[1].key.as_ref().unwrap(), KeySegment::Array(c, id, f, next)
            if &**c == "sales" && *id == Identifier::from_string("fr") && &**f == "cc" && matches!(&**next, KeySegment::Simple(k) if &**k == "value")));
    }

    #[test]
    fn test_matching_keys_lists_the_columns_a_pattern_takes() {
        let natural_keys = strs(&["id", "price_note", "tax_note", "note"]);
        assert_eq!(matching_keys("*_note", &natural_keys), Some(strs(&["price_note", "tax_note"])));
        assert_eq!(matching_keys("{x}_missing", &natural_keys), Some(vec![]));
        assert_eq!(matching_keys("note", &natural_keys), None);
    }

    #[test]
    fn test_non_pattern_source_returns_none() {
        assert!(expand_pattern_entry("weight", "weight_kg", &Format::Float, &["weight".to_string()]).is_none());
    }

    #[test]
    fn test_prefix_matching_is_case_insensitive() {
        let natural_keys = vec!["file_1".to_string()];
        let cols = expand_pattern_entry("FILE_[n]", "files[]", &Format::Auto, &natural_keys).unwrap();
        assert_eq!(cols.len(), 1);
    }

    #[test]
    fn test_resolve_suppression_matches_natural_key_case_insensitively() {
        let natural_keys = vec!["id".to_string(), "colour".to_string(), "title".to_string()];
        assert_eq!(resolve_suppression_key("colour", &natural_keys), Some("colour".to_string()));
        assert_eq!(resolve_suppression_key("COLOUR", &natural_keys), Some("colour".to_string()));
        assert_eq!(resolve_suppression_key("nope", &natural_keys), None);
    }

    #[test]
    fn test_resolve_suppression_matches_a1_letter_directly_in_a1_mode() {
        let natural_keys = vec!["a".to_string(), "b".to_string(), "c".to_string(), "d".to_string()];
        assert_eq!(resolve_suppression_key("d", &natural_keys), Some("d".to_string()));
        assert_eq!(resolve_suppression_key("D", &natural_keys), Some("d".to_string()));
    }

    #[test]
    fn test_resolve_suppression_falls_back_to_a_plain_number_for_r1c1_mode() {
        let natural_keys = vec!["c01".to_string(), "c02".to_string(), "c03".to_string(), "c04".to_string()];
        assert_eq!(resolve_suppression_key("4", &natural_keys), Some("c04".to_string()));
        assert_eq!(resolve_suppression_key("c4", &natural_keys), Some("c04".to_string()));
        assert_eq!(resolve_suppression_key("c04", &natural_keys), Some("c04".to_string()));
        assert_eq!(resolve_suppression_key("c004", &natural_keys), Some("c04".to_string()));
    }

    #[test]
    fn test_resolve_suppression_number_fallback_out_of_range_is_none() {
        let natural_keys = vec!["c01".to_string(), "c02".to_string()];
        assert_eq!(resolve_suppression_key("0", &natural_keys), None);
        assert_eq!(resolve_suppression_key("9", &natural_keys), None);
    }
}
