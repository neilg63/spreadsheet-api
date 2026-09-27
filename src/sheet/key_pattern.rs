//! Ported from spread-cli (https://github.com/neilg63/spreadsheet_to_json_cli, src/key_pattern.rs, MIT)
//! so the API accepts the same mapping/filter syntax as the CLI. Keep in sync with upstream.
//!
//! Parses the three placeholder-capture `--keys` patterns spread-cli supports on top of
//! the library's plain exact-match `Column` overrides:
//!
//! - `file_[n]:files[]` -- sequential columns into a plain array (`KeySegment::PlainArray`)
//! - `sales_{region}:sales.{region}` -- columns into a dynamic-keyed object (`KeySegment::Object`)
//! - `sales_[year]:sales[].{year}{amount}` -- columns into an array of `{year, amount}`
//!   items (`KeySegment::Array` + `KeySegment::InnerObject`)
//!
//! `[name]`/`{name}` in the *source* pattern is a capture: `[]` types it as an integer
//! (`Identifier::Integer`), `{}` as a string (`Identifier::String`) -- the mnemonic being
//! JSON's own shape (arrays are int-indexed, objects are string-keyed). The type only
//! matters when the capture is actually referenced on the target side; a capture used
//! purely for matching (like `[n]` in `files[]`, never referenced) accepts any suffix
//! regardless of its bracket type.
//!
//! Two shorthands for the common purely-positional case (never referenced on the target
//! side, so naming it at all is just ceremony): `download_*` (glob-style, probably the
//! most familiar spelling to anyone who's used a shell) and `download_[]` (empty
//! brackets, matching the same convention `files[]` already uses on the target side) are
//! both equivalent to an anonymous, unreferenced `[n]`.
//!
//! On the *target* side, a `{name}` matching the capture's own name binds that capture's
//! value; a `{name}` that *doesn't* match is the leaf -- the cell's own value lands there,
//! under a field literally named `name`. This needs the real column headers to expand
//! against (`natural_keys`), which is why this can't run until after a first,
//! header-only pass -- see `main.rs`.

use enclose_strings::SimpleExtract;
use spreadsheet_to_json::{Column, DateTimeMode, Format, Identifier, KeySegment};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq)]
enum CaptureType {
    Int,
    Str,
}

/// Not a name any target spec would realistically use, so there's no real risk of it
/// colliding with a genuine `{name}` reference -- but even if it did, referencing an
/// anonymous capture by this name is harmless (just binds whatever the anonymous match
/// happened to be), not something worth guarding against.
const ANONYMOUS_CAPTURE: &str = "*";

struct SourcePattern {
    prefix: String,
    capture_name: String,
    capture_type: CaptureType,
}

/// `None` if `source` has no `[name]`/`{name}`/`*`/`[]` capture at all -- callers fall
/// back to the existing exact-match `Column` handling for those, unchanged.
fn parse_source_pattern(source: &str) -> Option<SourcePattern> {
    let lower = source.to_lowercase();
    // Glob-style shorthand for an anonymous, purely-positional capture -- requires a
    // real prefix before the "*", same as every other form; a bare "*" alone would
    // match every column in the sheet, almost certainly never what's intended.
    if let Some(prefix) = lower.strip_suffix('*') {
        if !prefix.is_empty() {
            return Some(SourcePattern { prefix: prefix.to_string(), capture_name: ANONYMOUS_CAPTURE.to_string(), capture_type: CaptureType::Str });
        }
    }
    if lower.ends_with(']') {
        if let Some(open) = lower.find('[') {
            if open == 0 {
                return None; // no prefix at all -- same reasoning as the bare "*" guard above
            }
            let name = lower.extract_from_brackets()?;
            let (capture_name, capture_type) = if name.is_empty() {
                (ANONYMOUS_CAPTURE.to_string(), CaptureType::Str) // "download_[]" shorthand
            } else {
                (name, CaptureType::Int)
            };
            return Some(SourcePattern { prefix: lower[..open].to_string(), capture_name, capture_type });
        }
    }
    if lower.ends_with('}') {
        if let Some(open) = lower.find('{') {
            let name = lower.extract_from_braces().filter(|n| !n.is_empty())?;
            return Some(SourcePattern { prefix: lower[..open].to_string(), capture_name: name, capture_type: CaptureType::Str });
        }
    }
    None
}

/// One `.`-separated segment of a target-path spec.
enum TargetToken {
    /// A plain literal segment -- descend into (or create) a nested object under this
    /// key. e.g. the "sales" in "sales.{region}".
    Literal(String),
    /// `name[]` -- opens an array. Terminal (`files[]` alone) means `PlainArray`;
    /// followed by a `Braces` segment means `Array` + possibly `InnerObject`.
    ArrayOpen(String),
    /// One or more adjacent `{name}` groups with no separator between them, e.g.
    /// `{year}{amount}` is `Braces(["year", "amount"])`.
    Braces(Vec<String>),
}

/// Splits `target` on `.` and classifies each segment. `None` if any segment doesn't
/// parse as one of the three recognised shapes.
fn parse_target_tokens(target: &str) -> Option<Vec<TargetToken>> {
    target.split('.').map(parse_target_segment).collect()
}

fn parse_target_segment(segment: &str) -> Option<TargetToken> {
    if let Some(name) = segment.strip_suffix("[]") {
        if !name.is_empty() {
            return Some(TargetToken::ArrayOpen(name.to_string()));
        }
    }
    if segment.starts_with('{') {
        return parse_brace_groups(segment).map(TargetToken::Braces);
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

/// Builds the concrete `KeySegment` for one matched column, given its own bound capture
/// value. `capture_name` is what the source pattern called it (e.g. "region"); a
/// `{name}` on the target side is a reference to that capture only if `name` matches --
/// anything else is the leaf, using `name` as a literal field name.
fn build_key_segment(tokens: &[TargetToken], capture_name: &str, capture_id: &Identifier) -> Option<KeySegment> {
    let (first, rest) = tokens.split_first()?;
    match first {
        TargetToken::Literal(name) => {
            let next = build_key_segment(rest, capture_name, capture_id)?;
            Some(KeySegment::Object(Arc::from(name.as_str()), Arc::new(next)))
        }
        TargetToken::ArrayOpen(name) => {
            if rest.is_empty() {
                return Some(KeySegment::PlainArray(Arc::from(name.as_str())));
            }
            let (TargetToken::Braces(names), rest2) = rest.split_first()? else { return None };
            if !rest2.is_empty() {
                return None; // scoped grammar: nothing follows the array's brace-group segment
            }
            build_array_with_braces(name, names, capture_name, capture_id)
        }
        TargetToken::Braces(names) => {
            if !rest.is_empty() {
                return None; // braces are only a supported terminal outside an array context
            }
            let [name] = names.as_slice() else { return None };
            (name == capture_name).then(|| KeySegment::Simple(Arc::from(capture_id.to_string().as_str())))
        }
    }
}

/// `names[0]` becomes `Array`'s own discriminator (must be the known capture -- that's
/// what identifies which real-world entity, e.g. a year, each item represents); any
/// remaining names chain as `InnerObject` bindings, ending in the first name that
/// *doesn't* match the capture (the leaf, the cell's own value).
fn build_array_with_braces(container: &str, names: &[String], capture_name: &str, capture_id: &Identifier) -> Option<KeySegment> {
    let (first, rest) = names.split_first()?;
    if first != capture_name {
        return None;
    }
    let chain = build_inner_object_chain(rest, capture_name, capture_id)?;
    Some(KeySegment::Array(Arc::from(container), capture_id.clone(), Arc::from(first.as_str()), Arc::new(chain)))
}

fn build_inner_object_chain(names: &[String], capture_name: &str, capture_id: &Identifier) -> Option<KeySegment> {
    let (name, rest) = names.split_first()?;
    if name == capture_name {
        let next = build_inner_object_chain(rest, capture_name, capture_id)?;
        Some(KeySegment::InnerObject(capture_id.clone(), Arc::from(name.as_str()), Arc::new(next)))
    } else if rest.is_empty() {
        Some(KeySegment::Simple(Arc::from(name.as_str())))
    } else {
        None // an unrecognised name with more following would need its own captured
             // value, which a single-capture pattern doesn't have -- not supported
    }
}

/// Expands one placeholder `--keys` entry (`source` containing a `[name]`/`{name}`
/// capture) against the real, already-known natural column keys, producing one fully
/// resolved `Column` per matching column -- each with an exact `source_key` and a
/// concrete (non-templated) `KeySegment`. Returns `None` if `source` has no capture at
/// all (not a pattern entry) or the parsed target spec doesn't fit one of the three
/// supported shapes.
pub fn expand_pattern_entry(source: &str, target: &str, format: &Format, natural_keys: &[String]) -> Option<Vec<Column>> {
    let pattern = parse_source_pattern(source)?;
    let tokens = parse_target_tokens(target)?;

    // Whether build_key_segment succeeds depends only on the template's structure and
    // capture_name -- never on which value was actually captured -- so a placeholder
    // identifier validates the whole target spec once, up front. Without this, a
    // genuinely malformed target (e.g. an array-open not followed by a brace-group)
    // would only fail inside the per-column filter_map below, where its failure is
    // silently skipped rather than surfaced -- every column would simply vanish from
    // the shape check, with zero indication anything was wrong, rather than the clear
    // error this should be.
    let placeholder_id = Identifier::from_string("__validate__");
    build_key_segment(&tokens, &pattern.capture_name, &placeholder_id)?;

    let columns: Vec<Column> = natural_keys
        .iter()
        .filter_map(|natural_key| {
            let remainder = natural_key.strip_prefix(pattern.prefix.as_str())?;
            if remainder.is_empty() {
                return None;
            }
            let capture_id = match pattern.capture_type {
                CaptureType::Int => Identifier::from_int_str(remainder),
                CaptureType::Str => Identifier::from_string(remainder),
            };
            let key_segment = build_key_segment(&tokens, &pattern.capture_name, &capture_id)
                .expect("already validated the same template above");
            let mut col = Column::from_source_key_with_format(natural_key, None, format.clone(), None, DateTimeMode::Full, false);
            col.key = Some(key_segment);
            Some(col)
        })
        .collect();

    Some(columns)
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
