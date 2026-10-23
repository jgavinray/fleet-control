//! Shared library for `fleet-check-rs`, the Rust port of `check/fleet_check.py`.
//!
//! The Python tool is the specification. This crate holds the pieces every
//! engine check needs: the registry loader, the counting HTTP client, the result
//! record, and the small helpers the checks share (health shape, load string,
//! sanity answer).

pub mod check_engine;
pub mod http;
pub mod jsonpy;
pub mod kinds_rest;
pub mod kinds_tensorfold;
pub mod opts;
pub mod probes;
pub mod registry;
pub mod result;

pub use http::{Client, Resp};
pub use registry::{
    apply_local_address, load_local_addresses, load_registry, load_registry_with, parse_registry,
    EngineConfig, Registry, RegistryError, RegistryParseError,
};
pub use result::{exit_code, round3, CheckResult, Feature, Sanity, Status};

use serde_json::{Map, Value};

/// TensorFold engines report health in two shapes: `{ok: true}` or
/// `{status: "ok"}`. Anything else is not healthy, and `"ok"` wins when present
/// (Python returns `j["ok"] is True` as soon as the key exists).
pub fn health_up(j: &Value) -> bool {
    let Some(obj) = j.as_object() else {
        return false;
    };
    match obj.get("ok") {
        Some(Value::Bool(ok)) => *ok,
        Some(_) => false,
        None => obj.get("status").and_then(Value::as_str) == Some("ok"),
    }
}

/// Python truthiness: null, false, zero, the empty string, and empty
/// containers are falsy; everything else is truthy.
pub fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python's `str()` for the shapes a health body or a reply carries. Strings
/// stay bare; containers render the way `str()` renders them, which is `repr`.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(_) | Value::Object(_) => py_repr(v),
    }
}

/// Python's `repr()` for a string: the quote Python would pick, with the
/// escapes it would use. Feature reasons are compared verbatim by the gate.
pub fn py_repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's `repr()` for the values a choice can hold.
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => py_repr_str(s),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, value)| format!("{}: {}", py_repr_str(key), py_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `(text, in-flight count)` from whichever counters the health body exposes:
/// `requests_running` / `requests_waiting`, falling back to
/// `live.connections` / `live.waiting`. Empty text and 0 when the body exposes
/// neither.
///
/// The counters are kept raw, as Python keeps them: a negative counter still
/// means busy and prints with its sign, and `1.0` prints as `1.0`. Python adds
/// the two raw values, so the count is an `f64` here; the callers only use its
/// truthiness.
pub fn describe_load(h: &Value) -> (String, f64) {
    let Some(obj) = h.as_object() else {
        return (String::new(), 0.0);
    };
    let live = obj.get("live").and_then(Value::as_object);

    let running = counter(obj, live, "requests_running", "connections");
    let waiting = counter(obj, live, "requests_waiting", "waiting");
    if running.is_none() && waiting.is_none() {
        return (String::new(), 0.0);
    }

    let running = normalized(running.unwrap_or(Value::Null));
    let waiting = normalized(waiting.unwrap_or(Value::Null));
    if py_truthy(&running) || py_truthy(&waiting) {
        (
            format!(
                "busy ({} running, {} waiting)",
                jsonpy::number_text(&running),
                jsonpy::number_text(&waiting)
            ),
            counter_value(&running) + counter_value(&waiting),
        )
    } else {
        ("idle".to_string(), 0.0)
    }
}

/// Python's `value or 0`: a falsy counter becomes the integer 0, which prints
/// as "0".
fn normalized(value: Value) -> Value {
    if py_truthy(&value) {
        value
    } else {
        Value::from(0)
    }
}

fn counter(
    obj: &Map<String, Value>,
    live: Option<&Map<String, Value>>,
    key: &str,
    live_key: &str,
) -> Option<Value> {
    let present = |value: Option<&Value>| value.filter(|value| !value.is_null()).cloned();
    present(obj.get(key)).or_else(|| present(live.and_then(|live| live.get(live_key))))
}

fn counter_value(value: &Value) -> f64 {
    match value {
        Value::Bool(true) => 1.0,
        Value::Bool(false) => 0.0,
        Value::String(s) if s == jsonpy::NAN_TOK => f64::NAN,
        Value::String(s) if s == jsonpy::INF_TOK => f64::INFINITY,
        Value::String(s) if s == jsonpy::NEGINF_TOK => f64::NEG_INFINITY,
        other => other.as_f64().unwrap_or(0.0),
    }
}

/// Python's `re.fullmatch(r"\W*42\W*", text)`: the answer is 42, with only
/// non-word characters before and after it.
pub fn sanity_answer_ok(text: &str) -> bool {
    match text.find("42") {
        Some(start) => {
            text[..start].chars().all(|c| !is_word_char(c))
                && text[start + 2..].chars().all(|c| !is_word_char(c))
        }
        None => false,
    }
}

/// Python's `\w`: alphanumeric (Unicode) or underscore.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn python_repr_quotes_and_escapes() {
        assert_eq!(py_repr_str("apple"), "'apple'");
        assert_eq!(py_repr_str("it's"), "\"it's\"");
        assert_eq!(py_repr_str("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr_str("line\nbreak\t"), "'line\\nbreak\\t'");
        assert_eq!(py_repr_str(""), "''");
    }

    #[test]
    fn describe_load_renders_nonfinite_counters_like_python() {
        let nan = json!({
            "requests_running": jsonpy::NAN_TOK,
            "requests_waiting": 0,
        });
        let (text, in_flight) = describe_load(&nan);
        assert_eq!(text, "busy (nan running, 0 waiting)");
        assert!(in_flight.is_nan(), "NaN is truthy, like Python's float nan");

        let negative = json!({
            "requests_running": jsonpy::NEGINF_TOK,
            "requests_waiting": 0,
        });
        let (text, in_flight) = describe_load(&negative);
        assert_eq!(text, "busy (-inf running, 0 waiting)");
        assert_eq!(in_flight, f64::NEG_INFINITY);

        let waiting = json!({
            "requests_running": 0,
            "requests_waiting": jsonpy::INF_TOK,
        });
        let (text, in_flight) = describe_load(&waiting);
        assert_eq!(text, "busy (0 running, inf waiting)");
        assert_eq!(in_flight, f64::INFINITY);

        assert!(py_truthy(&json!(jsonpy::NAN_TOK)));
        assert!(py_truthy(&json!(jsonpy::NEGINF_TOK)));
    }

    #[test]
    fn py_repr_handles_non_string_choices() {
        assert_eq!(py_repr(&json!("say \"hi\"")), "'say \"hi\"'");
        assert_eq!(py_repr(&json!(5)), "5");
        assert_eq!(py_repr(&json!(1.5)), "1.5");
        assert_eq!(py_repr(&json!(null)), "None");
        assert_eq!(py_repr(&json!(true)), "True");
        assert_eq!(py_repr(&json!(["a'b", 2])), "[\"a'b\", 2]");
        assert_eq!(py_repr(&json!({"k": "v"})), "{'k': 'v'}");
    }
}
