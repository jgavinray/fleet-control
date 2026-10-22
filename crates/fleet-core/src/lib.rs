//! Shared library for `fleet-check-rs`, the Rust port of `check/fleet_check.py`.
//!
//! The Python tool is the specification. This crate holds the pieces every
//! engine check needs: the registry loader, the counting HTTP client, the result
//! record, and the small helpers the checks share (health shape, load string,
//! sanity answer).
//!
//! Modules `probes` and `kinds` (the feature probes and the per-kind checks)
//! belong to later slices and are deliberately not declared here.

pub mod http;
pub mod kinds_rest;
pub mod kinds_tensorfold;
pub mod opts;
pub mod probes;
pub mod registry;
pub mod result;

pub use http::{Client, Resp};
pub use registry::{
    apply_local_address, load_local_addresses, load_registry, load_registry_with, parse_registry,
    EngineConfig, Registry, RegistryError,
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

/// `(text, in-flight count)` from whichever counters the health body exposes:
/// `requests_running` / `requests_waiting`, falling back to
/// `live.connections` / `live.waiting`. Empty text and 0 when the body exposes
/// neither.
pub fn describe_load(h: &Value) -> (String, u64) {
    let Some(obj) = h.as_object() else {
        return (String::new(), 0);
    };
    let live = obj.get("live").and_then(Value::as_object);

    let running = counter(obj, live, "requests_running", "connections");
    let waiting = counter(obj, live, "requests_waiting", "waiting");
    if running.is_none() && waiting.is_none() {
        return (String::new(), 0);
    }

    let running = running.unwrap_or(0);
    let waiting = waiting.unwrap_or(0);
    if running != 0 || waiting != 0 {
        (
            format!("busy ({running} running, {waiting} waiting)"),
            running + waiting,
        )
    } else {
        ("idle".to_string(), 0)
    }
}

fn counter(
    obj: &Map<String, Value>,
    live: Option<&Map<String, Value>>,
    key: &str,
    live_key: &str,
) -> Option<u64> {
    let number = |value: Option<&Value>| -> Option<u64> {
        let value = value.filter(|value| !value.is_null())?;
        Some(value.as_u64().unwrap_or(0))
    };
    number(obj.get(key)).or_else(|| number(live.and_then(|live| live.get(live_key))))
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
