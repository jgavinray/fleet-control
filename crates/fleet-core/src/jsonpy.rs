//! Python's `json.loads` accepts three bare tokens that `serde_json` rejects:
//! `NaN`, `Infinity` and `-Infinity`. A reply carrying one is valid to the
//! Python tool and unparseable to the Rust port, so a response is first parsed
//! as-is and, on failure, scanned for those tokens outside string literals.
//!
//! The scan is string-aware (an escaped quote never closes a string) and
//! position-aware: Python only calls its `parse_constant` hook where a value is
//! allowed, so a bare token in key position (`{NaN: 1}`) stays untouched and
//! the reply remains unparseable, exactly as in Python.
//!
//! A replaced token becomes a sentinel string ([`NAN_TOK`], [`INF_TOK`],
//! [`NEGINF_TOK`]). [`is_nonfinite`] recognizes the sentinels, and
//! [`number_text`] renders them the way `str(float)` does (`nan`, `inf`,
//! `-inf`) so every check that prints or branches on a number agrees with the
//! Python tool.

use serde_json::Value;

/// Sentinel for a bare `NaN` token.
pub const NAN_TOK: &str = "__pynan__";
/// Sentinel for a bare `Infinity` token.
pub const INF_TOK: &str = "__pyinf__";
/// Sentinel for a bare `-Infinity` token.
pub const NEGINF_TOK: &str = "__pyneginf__";

const NAN: &[u8] = b"NaN";
const INFINITY: &[u8] = b"Infinity";
const NEG_INFINITY: &[u8] = b"-Infinity";

/// Parse JSON the way Python's `json.loads` does, including its three
/// non-standard constants. `None` means Python would also raise.
pub fn parse(text: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str(text) {
        return Some(value);
    }
    serde_json::from_str(&replace_nonfinite(text)).ok()
}

/// True when `value` is one of the non-finite sentinels.
pub fn is_nonfinite(value: &Value) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    text == NAN_TOK || text == INF_TOK || text == NEGINF_TOK
}

/// Python's `str()` for a number position. Finite values keep the existing
/// rendering; a sentinel renders as the float `str()` would (`nan`, `inf`,
/// `-inf`).
pub fn number_text(value: &Value) -> String {
    match value {
        Value::Number(number) if number.is_i64() || number.is_u64() => number.to_string(),
        Value::Number(number) => number
            .as_f64()
            .map(python_float)
            .unwrap_or_else(|| number.to_string()),
        Value::String(s) if s == NAN_TOK => "nan".to_string(),
        Value::String(s) if s == INF_TOK => "inf".to_string(),
        Value::String(s) if s == NEGINF_TOK => "-inf".to_string(),
        other => crate::py_str(other),
    }
}

/// Python's `str(float)`: the shortest round-trip digits, fixed notation for
/// decimal exponents in `-4..16`, scientific otherwise, with a signed,
/// zero-padded two-digit exponent. Rust's `Debug` for `f64` has the same
/// digits and thresholds; only the exponent spelling differs.
fn python_float(value: f64) -> String {
    let debug = format!("{value:?}");
    let Some((mantissa, exponent)) = debug.split_once('e') else {
        return debug;
    };
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.abs())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    Array,
    Object,
}

/// The delimiter rule: a token is bare only when it does not touch an
/// identifier character, a digit, a dot, a plus or a minus.
fn boundary(byte: Option<u8>) -> bool {
    match byte {
        None => true,
        Some(byte) => {
            !(byte.is_ascii_alphanumeric()
                || byte == b'_'
                || byte == b'.'
                || byte == b'+'
                || byte == b'-')
        }
    }
}

/// Replace the bare non-finite tokens with quoted sentinels. Bytes inside
/// strings are copied untouched; the container stack decides whether a value
/// (where Python accepts a constant) or a key is expected.
fn replace_nonfinite(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_is_value = false;
    let mut expect_value = true;
    let mut stack: Vec<Container> = Vec::new();

    while index < bytes.len() {
        let byte = bytes[index];

        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
                if string_is_value {
                    expect_value = false;
                }
            }
            index += 1;
            continue;
        }

        match byte {
            b'"' => {
                in_string = true;
                string_is_value = expect_value;
                index += 1;
                continue;
            }
            b'{' => {
                stack.push(Container::Object);
                expect_value = false;
            }
            b'[' => {
                stack.push(Container::Array);
                expect_value = true;
            }
            b'}' | b']' => {
                stack.pop();
                expect_value = false;
            }
            b':' => expect_value = true,
            b',' => expect_value = stack.last() == Some(&Container::Array),
            _ => {}
        }

        if expect_value && boundary(index.checked_sub(1).map(|before| bytes[before])) {
            let matched = if bytes[index..].starts_with(NEG_INFINITY) {
                Some((NEG_INFINITY, NEGINF_TOK))
            } else if bytes[index..].starts_with(NAN) {
                Some((NAN, NAN_TOK))
            } else if bytes[index..].starts_with(INFINITY) {
                Some((INFINITY, INF_TOK))
            } else {
                None
            };
            if let Some((token, sentinel)) = matched {
                let after = index + token.len();
                if boundary(bytes.get(after).copied()) {
                    out.push_str(&text[copied..index]);
                    out.push('"');
                    out.push_str(sentinel);
                    out.push('"');
                    copied = after;
                    index = after;
                    expect_value = false;
                    continue;
                }
            }
        }

        index += 1;
    }

    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_json_is_parsed_without_rewriting() {
        assert_eq!(
            parse(r#"{"a": 1, "b": [true, null]}"#),
            Some(json!({"a": 1, "b": [true, null]}))
        );
        assert_eq!(parse(""), None);
        assert_eq!(parse("not json"), None);
    }

    #[test]
    fn bare_tokens_become_sentinels() {
        let value = parse(r#"{"a": NaN, "b": [Infinity, -Infinity]}"#).expect("Python accepts");
        assert_eq!(value["a"], json!(NAN_TOK));
        assert_eq!(value["b"], json!([INF_TOK, NEGINF_TOK]));
        assert!(is_nonfinite(&value["a"]));
        assert!(is_nonfinite(&value["b"][0]));
        assert!(is_nonfinite(&value["b"][1]));
        assert!(!is_nonfinite(&json!(1)));
    }

    #[test]
    fn tokens_inside_strings_and_quoted_keys_are_untouched() {
        let value = parse(r#"{"NaN": "NaN", "a": "xNaN", "b": "Infinity"}"#).expect("valid JSON");
        assert_eq!(value["NaN"], json!("NaN"));
        assert_eq!(value["a"], json!("xNaN"));
        assert_eq!(value["b"], json!("Infinity"));
        assert!(!is_nonfinite(&value["NaN"]));
    }

    #[test]
    fn escaped_quotes_do_not_end_a_string_early() {
        let value = parse(r#"{"a": "say \"NaN\"", "b": NaN}"#).expect("valid after rewriting");
        assert_eq!(value["a"], json!("say \"NaN\""));
        assert_eq!(value["b"], json!(NAN_TOK));
    }

    #[test]
    fn tokens_touching_a_word_number_or_sign_are_not_tokens() {
        for text in [
            r#"{"a": NaNx}"#,
            r#"{"a": xNaN}"#,
            r#"{"a": NaN.5}"#,
            r#"{"a": -NaN}"#,
            r#"{"a": NaN1}"#,
            r#"{"a": +Infinity}"#,
            "NaNx",
            "xNaN",
            "NaN.5",
        ] {
            assert_eq!(parse(text), None, "{text:?} is invalid in Python too");
        }
    }

    #[test]
    fn a_bare_token_in_key_position_is_not_a_value() {
        // Python raises "Expecting property name enclosed in double quotes"
        // for these; replacing the token would wrongly turn them into keys.
        assert_eq!(parse("{NaN: 1}"), None);
        assert_eq!(parse(r#"{"a": NaN, NaN: 1}"#), None);
    }

    #[test]
    fn whitespace_and_newlines_around_tokens_are_ignored() {
        let value = parse("{\n  \"a\":\n  NaN\n}").expect("valid after rewriting");
        assert_eq!(value["a"], json!(NAN_TOK));
    }

    #[test]
    fn number_text_matches_python_str() {
        assert_eq!(number_text(&json!(NAN_TOK)), "nan");
        assert_eq!(number_text(&json!(INF_TOK)), "inf");
        assert_eq!(number_text(&json!(NEGINF_TOK)), "-inf");
        assert_eq!(number_text(&json!(1)), "1");
        assert_eq!(number_text(&json!(1.0)), "1.0");
        assert_eq!(number_text(&json!(-1.0)), "-1.0");
        assert_eq!(number_text(&json!(1e10)), "10000000000.0");
        assert_eq!(number_text(&json!(1e16)), "1e+16");
        assert_eq!(number_text(&json!(1e-5)), "1e-05");
        assert_eq!(number_text(&json!(0.0001)), "0.0001");
        assert_eq!(number_text(&json!(null)), "None");
        assert_eq!(number_text(&json!("plain")), "plain");
    }
}
