//! Ported from spread-cli (https://github.com/neilg63/spreadsheet_to_json_cli, src/filter_pattern.rs, MIT)
//! so the API accepts the same mapping/filter syntax as the CLI. Keep in sync with upstream.
//!
//! Parses the `--filter` SQL-like boolean expression DSL into the library's own
//! `FilterCondition` tree, matching `spread-cli`'s established split: the library owns
//! the reusable filter mechanism (`FilterCondition`/`FilterMatch`/`ColumnMatch`), this
//! module owns the human-typed text syntax on top of it, exactly like `key_pattern.rs`
//! does for `--keys`.
//!
//! Grammar (keywords case-insensitive; `NOT`/`AND`/`OR`/etc. also accepted lowercase/mixed):
//!
//! ```text
//! expr       := or_expr
//! or_expr    := and_expr ("OR" and_expr)*
//! and_expr   := unary ("AND" unary)*
//! unary      := "NOT" unary | atom
//! atom       := "(" expr ")" | comparison
//! comparison := field ("NOT")? op value
//!             | field ("NOT")? ("IN" | "ANY") "(" value ("," value)* ")"
//! field      := identifier("." identifier)*, e.g. first_name or size.width --
//!               matched against the row's *final* mapped keys (after --keys renames,
//!               including into nested objects), not raw source columns
//! op         := "=" | "!=" | "<>" | ">=" | "<=" | ">" | "<" | "LIKE" | "ILIKE"
//! value      := 'quoted string' | bare word | bare number
//! ```
//!
//! `LIKE`/`ILIKE` use SQL `%` wildcard semantics, translated to the library's own
//! `StartsWith`/`EndsWith`/`Contains`/`Exact` rather than kept as a literal pattern
//! string: `'a%'` -> starts-with, `'%os'` -> ends-with, `'%foo%'` -> contains, `'exact'`
//! (no `%`) -> exact match. A `%` in the *middle* of a pattern (`'a%b'`) isn't
//! supported -- that needs two independent anchors, which none of `StartsWith`/
//! `EndsWith`/`Contains` alone can express, and SQL's `_` single-character wildcard
//! isn't supported either. Both are out of scope for the same "narrow range of
//! conditions" reasoning the library's own `FilterMatch` was scoped to; a malformed
//! pattern is a clear parse error, not a silent partial match.
//!
//! `IN (...)`/`NOT IN (...)` map to the library's `OneOf`/`Not(OneOf)` -- the row's own
//! (scalar) value must equal one of the listed values. `ANY (...)`/`NOT ANY (...)` map
//! to `AnyOneOf` -- the row's own value must itself be an *array* (e.g. a `PlainArray`-
//! mapped column) sharing at least one element with the listed values; a non-array row
//! value never matches `ANY`, it isn't a looser synonym for `IN`.
//!
//! Values don't strictly need quoting (`IN (us, gb, ca)` works) -- quotes are only
//! *required* to group a value containing a space or a comma into one token rather than
//! having it split apart or read as a field/operator. An unquoted value can't reuse a
//! reserved word (`and`, `in`, ...) as a literal, same as unquoted identifiers in SQL --
//! quote it if that's genuinely needed.
//!
//! `BETWEEN`/regex aren't covered by this pass -- `BETWEEN`'s own `AND` keyword
//! collides grammatically with the top-level boolean `AND` and needs deliberate
//! handling, and regex was already scoped out of the library's default feature set in
//! the original filter design.

use enclose_strings::{CapturedSegment, EscapeStyle, ScanOptions, SimpleExtract};
use spreadsheet_to_json::serde_json::{json, Value};
use spreadsheet_to_json::{ColumnMatch, FilterCondition, FilterMatch, MatchMode};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Number(Value),
    Str(String),
    And,
    Or,
    Not,
    Like,
    Ilike,
    In,
    Any,
    Op(&'static str),
    LParen,
    RParen,
    Comma,
}

/// Splits `input` into quoted-string and plain-text spans up front -- using SQL's own
/// `''`-doubling convention, via `enclose-strings`' `EscapeStyle::Doubled` -- so the
/// char-by-char scan below never has to deal with quote characters or escaping itself.
/// A stray `'` surviving into a plain-text span means the quote it opened was never
/// closed (an unterminated enclosure is folded into the trailing plain-text span rather
/// than raised as an error at the split stage).
fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    for segment in input.extract_enclosures_with('\'', '\'', ScanOptions::escaped(EscapeStyle::Doubled)) {
        match segment {
            CapturedSegment::Enclosure(content) => tokens.push(Token::Str(content.into_owned())),
            CapturedSegment::Outside(text) => {
                if text.contains('\'') {
                    return Err(format!("unterminated string literal in --filter: \"{input}\""));
                }
                tokenize_plain(text, input, &mut tokens)?;
            }
        }
    }
    Ok(tokens)
}

/// Tokenizes a quote-free span (everything but the DSL's own `'...'` literals, which
/// `tokenize` peels off first).
fn tokenize_plain(span: &str, input: &str, tokens: &mut Vec<Token>) -> Result<(), String> {
    let chars: Vec<char> = span.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => { tokens.push(Token::LParen); i += 1; }
            ')' => { tokens.push(Token::RParen); i += 1; }
            '>' | '<' | '!' | '=' => {
                let two: Option<&'static str> = if i + 1 < chars.len() {
                    match (c, chars[i + 1]) {
                        ('>', '=') => Some(">="),
                        ('<', '=') => Some("<="),
                        ('!', '=') => Some("!="),
                        ('<', '>') => Some("<>"),
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(op) = two {
                    tokens.push(Token::Op(op));
                    i += 2;
                } else {
                    let op = match c {
                        '>' => ">",
                        '<' => "<",
                        '=' => "=",
                        _ => return Err(format!("unexpected '!' in --filter (did you mean '!='?): \"{input}\"")),
                    };
                    tokens.push(Token::Op(op));
                    i += 1;
                }
            }
            c if c.is_ascii_digit() || (c == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) => {
                let start = i;
                i += 1;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                // A row's own integer-typed values (via Format::Integer) are backed by
                // an i64 Number, not f64 -- Exact/OneOf/!= compare via plain Value
                // equality (not numeric equality), so a literal like "18" has to parse
                // as the same i64-backed representation, not always f64, or an
                // otherwise-correct "age != 18" would never equal a genuine age of 18
                // and "!=" would incorrectly report true for every row.
                let value = if text.contains('.') {
                    let n = text.parse::<f64>().map_err(|_| format!("invalid number '{text}' in --filter"))?;
                    json!(n)
                } else {
                    let n = text.parse::<i64>().map_err(|_| format!("invalid number '{text}' in --filter"))?;
                    json!(n)
                };
                tokens.push(Token::Number(value));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                i += 1;
                // "." is allowed mid-identifier for dot-path field references
                // (size.width) -- a leading/trailing/doubled dot (empty path segment)
                // is caught and rejected once the parser splits this token on ".",
                // not here; the tokenizer's job is just to keep the whole reference as
                // one token rather than splitting on every dot as a separate character.
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.') {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                tokens.push(match text.to_lowercase().as_str() {
                    "and" => Token::And,
                    "or" => Token::Or,
                    "not" => Token::Not,
                    "like" => Token::Like,
                    "ilike" => Token::Ilike,
                    "in" => Token::In,
                    "any" => Token::Any,
                    _ => Token::Ident(text),
                });
            }
            ',' => { tokens.push(Token::Comma); i += 1; }
            other => return Err(format!("unexpected character '{other}' in --filter: \"{input}\"")),
        }
    }
    Ok(())
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn expect(&mut self, expected: &Token, context: &str) -> Result<(), String> {
        match self.next() {
            Some(t) if &t == expected => Ok(()),
            Some(t) => Err(format!("expected {expected:?} {context}, found {t:?}")),
            None => Err(format!("expected {expected:?} {context}, found end of --filter")),
        }
    }

    fn parse_expr(&mut self) -> Result<FilterCondition, String> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Or)) {
            self.next();
            let right = self.parse_and()?;
            left = match left {
                FilterCondition::Or(mut conditions) => {
                    conditions.push(right);
                    FilterCondition::Or(conditions)
                }
                other => FilterCondition::Or(vec![other, right]),
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<FilterCondition, String> {
        let mut left = self.parse_unary()?;
        while matches!(self.peek(), Some(Token::And)) {
            self.next();
            let right = self.parse_unary()?;
            left = match left {
                FilterCondition::And(mut conditions) => {
                    conditions.push(right);
                    FilterCondition::And(conditions)
                }
                other => FilterCondition::And(vec![other, right]),
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<FilterCondition, String> {
        if matches!(self.peek(), Some(Token::Not)) {
            self.next();
            let inner = self.parse_unary()?;
            return Ok(FilterCondition::Not(Arc::new(inner)));
        }
        self.parse_atom()
    }

    fn parse_atom(&mut self) -> Result<FilterCondition, String> {
        if matches!(self.peek(), Some(Token::LParen)) {
            self.next();
            let inner = self.parse_expr()?;
            self.expect(&Token::RParen, "to close '('")?;
            return Ok(inner);
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<FilterCondition, String> {
        let field = match self.next() {
            Some(Token::Ident(name)) => name,
            other => return Err(format!("expected a field name in --filter, found {other:?}")),
        };
        // "size.width" -- one dotted reference into a --keys-nested object, tokenized
        // as a single Ident (see tokenize's "." handling) and split apart here. A
        // leading/trailing/doubled dot leaves an empty segment, rejected explicitly
        // rather than building a ColumnMatch that could never match anything.
        let field_path: Vec<String> = field.split('.').map(str::to_string).collect();
        if field_path.iter().any(|s| s.is_empty()) {
            return Err(format!("invalid field reference '{field}' in --filter (empty segment between dots)"));
        }

        // NOT LIKE/NOT ILIKE/NOT IN/NOT ANY: the only place a comparison-level "NOT"
        // appears -- distinct from the unary prefix "NOT" above, which only ever
        // precedes a whole parenthesised/atomic sub-expression, never sits
        // mid-comparison. "!=" / "<>" fold into the same flag below rather than
        // building their own Not(...) wrapper, so there's exactly one negation path.
        let mut negate = if matches!(self.peek(), Some(Token::Not)) {
            self.next();
            true
        } else {
            false
        };

        let condition = match self.next() {
            Some(Token::Op(op)) => {
                let value = self.parse_value()?;
                match op {
                    "=" => FilterMatch::Exact(value, MatchMode::Cs),
                    "!=" | "<>" => {
                        negate = true;
                        FilterMatch::Exact(value, MatchMode::Cs)
                    }
                    ">" => FilterMatch::Gt(value),
                    ">=" => FilterMatch::Gte(value),
                    "<" => FilterMatch::Lt(value),
                    "<=" => FilterMatch::Lte(value),
                    _ => unreachable!("tokenizer only ever produces the operators handled above"),
                }
            }
            Some(t @ Token::Like) | Some(t @ Token::Ilike) => {
                let mode = if t == Token::Ilike { MatchMode::Ci } else { MatchMode::Cs };
                let pattern = match self.next() {
                    Some(Token::Str(s)) => s,
                    other => return Err(format!("expected a quoted pattern after LIKE/ILIKE, found {other:?}")),
                };
                like_to_filter_match(&pattern, mode)?
            }
            Some(Token::In) => FilterMatch::OneOf(self.parse_value_list()?),
            Some(Token::Any) => FilterMatch::AnyOneOf(self.parse_value_list()?),
            other => return Err(format!(
                "expected a comparison operator (=, !=, >, >=, <, <=, LIKE, ILIKE, IN, ANY) after field '{field}', found {other:?}"
            )),
        };

        let column_match = match field_path.as_slice() {
            [single] => ColumnMatch::new(single.as_str(), condition),
            path => ColumnMatch::at_path(&path.iter().map(String::as_str).collect::<Vec<_>>(), condition),
        };
        let base = FilterCondition::Match(column_match);
        Ok(if negate { FilterCondition::Not(Arc::new(base)) } else { base })
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        match self.next() {
            Some(Token::Str(s)) => Ok(json!(s)),
            Some(Token::Number(v)) => Ok(v),
            // A bare word is tolerated as a plain string value -- quoting is only
            // actually needed to group characters a bare word can't represent on its
            // own (spaces, commas) or to use a value that collides with a reserved
            // word (in, and, ...), same as unquoted identifiers in SQL.
            Some(Token::Ident(s)) => Ok(json!(s)),
            other => Err(format!("expected a value (quoted string, bare word, or number), found {other:?}")),
        }
    }

    /// `"(" value ("," value)* ")"` for `IN`/`ANY`.
    fn parse_value_list(&mut self) -> Result<Vec<Value>, String> {
        self.expect(&Token::LParen, "to start an IN/ANY value list")?;
        let mut values = vec![self.parse_value()?];
        loop {
            match self.next() {
                Some(Token::Comma) => values.push(self.parse_value()?),
                Some(Token::RParen) => break,
                other => return Err(format!("expected ',' or ')' in IN/ANY value list, found {other:?}")),
            }
        }
        Ok(values)
    }
}

/// Translates a SQL LIKE/ILIKE `%`-wildcard pattern into the library's own
/// StartsWith/EndsWith/Contains/Exact -- see the module doc comment for the exact
/// mapping and what's deliberately unsupported (`%` in the middle, `_`).
fn like_to_filter_match(pattern: &str, mode: MatchMode) -> Result<FilterMatch, String> {
    let leading = pattern.starts_with('%');
    let trailing = pattern.ends_with('%');
    let trimmed = pattern.trim_start_matches('%').trim_end_matches('%');
    if trimmed.contains('%') {
        return Err(format!(
            "LIKE pattern '{pattern}' has '%' in the middle -- only a single leading and/or \
             trailing '%' is supported (StartsWith/EndsWith/Contains), not a two-anchor pattern"
        ));
    }
    Ok(match (leading, trailing) {
        (true, true) => FilterMatch::Contains(json!(trimmed), mode),
        (true, false) => FilterMatch::EndsWith(json!(trimmed), mode),
        (false, true) => FilterMatch::StartsWith(json!(trimmed), mode),
        (false, false) => FilterMatch::Exact(json!(trimmed), mode),
    })
}

/// Parses one `--filter` string into a `FilterCondition` tree.
pub fn parse_filter(input: &str) -> Result<FilterCondition, String> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err("--filter value is empty".to_string());
    }
    let mut parser = Parser { tokens, pos: 0 };
    let condition = parser.parse_expr()?;
    if parser.pos != parser.tokens.len() {
        return Err(format!(
            "unexpected trailing content in --filter after a complete expression: \"{input}\" \
             (unbalanced parentheses, or a missing AND/OR between clauses?)"
        ));
    }
    Ok(condition)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spreadsheet_to_json::indexmap::IndexMap;

    fn row(pairs: &[(&str, Value)]) -> IndexMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn test_and_logic_on_mapped_fields() {
        let condition = parse_filter("first_name ILIKE 'a%' and last_name  ILIKE '%os' ").unwrap();
        assert!(condition.evaluate(&row(&[("first_name", json!("Amos")), ("last_name", json!("Santos"))])));
        assert!(!condition.evaluate(&row(&[("first_name", json!("Bob")), ("last_name", json!("Santos"))])));
        assert!(!condition.evaluate(&row(&[("first_name", json!("Amos")), ("last_name", json!("Smith"))])));
        // ILIKE is case-insensitive
        assert!(condition.evaluate(&row(&[("first_name", json!("AMOS")), ("last_name", json!("SANTOS"))])));
    }

    #[test]
    fn test_or_logic_on_mapped_fields() {
        let condition = parse_filter("first_name ILIKE 'a%'  or last_name  ILIKE '%os'").unwrap();
        assert!(condition.evaluate(&row(&[("first_name", json!("Amos")), ("last_name", json!("Smith"))])));
        assert!(condition.evaluate(&row(&[("first_name", json!("Bob")), ("last_name", json!("Santos"))])));
        assert!(!condition.evaluate(&row(&[("first_name", json!("Bob")), ("last_name", json!("Smith"))])));
    }

    #[test]
    fn test_and_with_nested_or_and_numeric_comparison() {
        let condition = parse_filter("(first_name ILIKE 'a%' or last_name  ILIKE '%os' ) and age>=18").unwrap();
        assert!(condition.evaluate(&row(&[("first_name", json!("Amos")), ("last_name", json!("Smith")), ("age", json!(20))])));
        assert!(!condition.evaluate(&row(&[("first_name", json!("Amos")), ("last_name", json!("Smith")), ("age", json!(16))])));
        assert!(!condition.evaluate(&row(&[("first_name", json!("Bob")), ("last_name", json!("Smith")), ("age", json!(20))])));
    }

    #[test]
    fn test_unbalanced_parens_is_a_clear_error_not_silently_accepted() {
        let err = parse_filter("first_name ILIKE 'a%'  or last_name  ILIKE '%os' )").unwrap_err();
        assert!(err.contains("trailing") || err.contains("expected"), "got: {err}");
    }

    #[test]
    fn test_not_negates_a_parenthesised_group() {
        let condition = parse_filter("NOT (age >= 18)").unwrap();
        assert!(condition.evaluate(&row(&[("age", json!(16))])));
        assert!(!condition.evaluate(&row(&[("age", json!(20))])));
    }

    #[test]
    fn test_not_like_and_not_equal() {
        let not_like = parse_filter("first_name NOT ILIKE 'a%'").unwrap();
        assert!(not_like.evaluate(&row(&[("first_name", json!("Bob"))])));
        assert!(!not_like.evaluate(&row(&[("first_name", json!("Amos"))])));

        let not_eq = parse_filter("age != 18").unwrap();
        assert!(not_eq.evaluate(&row(&[("age", json!(20))])));
        assert!(!not_eq.evaluate(&row(&[("age", json!(18))])));
    }

    #[test]
    fn test_like_pattern_with_middle_wildcard_is_rejected() {
        let err = parse_filter("first_name ILIKE 'a%b'").unwrap_err();
        assert!(err.contains("middle"), "got: {err}");
    }

    #[test]
    fn test_exact_string_and_no_wildcard_like_are_equivalent() {
        let eq = parse_filter("engine_type = 'electric'").unwrap();
        let like = parse_filter("engine_type LIKE 'electric'").unwrap();
        let r = row(&[("engine_type", json!("electric"))]);
        assert!(eq.evaluate(&r));
        assert!(like.evaluate(&r));
    }

    #[test]
    fn test_in_matches_a_quoted_or_bare_value_list() {
        let quoted = parse_filter("country_code IN ('us', 'gb', 'ca')").unwrap();
        let bare = parse_filter("country_code IN (us, gb, ca)").unwrap();
        assert!(quoted.evaluate(&row(&[("country_code", json!("gb"))])));
        assert!(bare.evaluate(&row(&[("country_code", json!("gb"))])));
        assert!(!quoted.evaluate(&row(&[("country_code", json!("fr"))])));
    }

    #[test]
    fn test_not_in_negates_the_membership_check() {
        let condition = parse_filter("country_code NOT IN (us, gb)").unwrap();
        assert!(condition.evaluate(&row(&[("country_code", json!("fr"))])));
        assert!(!condition.evaluate(&row(&[("country_code", json!("us"))])));
    }

    #[test]
    fn test_any_matches_when_the_row_array_intersects_the_list() {
        let condition = parse_filter("codes ANY (us, ca)").unwrap();
        assert!(condition.evaluate(&row(&[("codes", json!(["us", "de"]))])));
        assert!(!condition.evaluate(&row(&[("codes", json!(["fr", "de"]))])));
        // a scalar field never satisfies ANY, even if it equals a candidate
        assert!(!condition.evaluate(&row(&[("codes", json!("us"))])));
    }

    #[test]
    fn test_dot_path_field_reaches_a_nested_keys_built_object() {
        let condition = parse_filter("size.width < 10").unwrap();
        let narrow = row(&[("size", json!({"width": 5, "height": 20}))]);
        let wide = row(&[("size", json!({"width": 15, "height": 20}))]);
        assert!(condition.evaluate(&narrow));
        assert!(!condition.evaluate(&wide));
    }

    #[test]
    fn test_malformed_dot_path_is_a_clear_error() {
        // A dot with nothing alphabetic before it isn't part of any token at all --
        // caught at tokenization, before path-splitting ever runs.
        let err = parse_filter(".width < 10").unwrap_err();
        assert!(err.contains("unexpected character"), "got: {err}");

        // A trailing or doubled dot tokenizes fine (it's still "alphabetic-started"),
        // but splits into an empty path segment -- caught explicitly when the field
        // reference is parsed.
        for expr in ["size. < 10", "size..width < 10"] {
            let err = parse_filter(expr).unwrap_err();
            assert!(err.contains("empty segment"), "expr '{expr}' got: {err}");
        }
    }

    #[test]
    fn test_doubled_single_quote_inside_a_string_literal_is_a_literal_quote() {
        let condition = parse_filter("last_name = 'O''Brien'").unwrap();
        assert!(condition.evaluate(&row(&[("last_name", json!("O'Brien"))])));
        assert!(!condition.evaluate(&row(&[("last_name", json!("OBrien"))])));
    }

    #[test]
    fn test_unterminated_string_literal_is_a_clear_error() {
        let err = parse_filter("first_name = 'Amos").unwrap_err();
        assert!(err.contains("unterminated string literal"), "got: {err}");
    }
}
