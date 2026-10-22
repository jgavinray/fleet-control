//! TensorFold feature probes, ported 1:1 from `check/fleet_check.py`.
//!
//! Every probe answers `(result, reason)` with result "yes" | "no" | "error".
//! `classify_refusal` turns a non-200 answer into that pair: a 4xx (or 501)
//! means the engine does not offer the feature, a 5xx whose body says
//! "unsupported" means the same, and anything else is a real error.
//!
//! `BgSender` is Python's `send()` wrapper: while the engine accepts
//! `priority: "background"` it sends probes that way with the shorter
//! `--bg-timeout`; one timeout drops it back to plain priority for the rest of
//! the run and leaves a note behind.

use serde_json::{json, Value};

use crate::http::{Client, Resp};
use crate::registry::EngineConfig;
use crate::result::{CheckResult, Feature};

/// The sanity question every engine must answer with 42.
pub const SANITY_PROMPT: &str = "What is 17 plus 25? Reply with the number only.";

/// A 5xx body containing one of these means "not supported", not "broken".
const UNSUPPORTED: [&str; 5] = [
    "required",
    "not supported",
    "unsupported",
    "not implemented",
    "libtf",
];

/// Python's `chat_body`: one user message, fixed sampling, thinking off, plus
/// the probe-specific keys (`guided_choice`, `logprobs`, `priority`).
pub fn chat_body(model: &str, content: &str, max_tokens: u64, extra: &[(&str, Value)]) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [{"role": "user", "content": content}],
        "max_tokens": max_tokens,
        "temperature": 0,
        "chat_template_kwargs": {"enable_thinking": false},
    });
    let obj = body.as_object_mut().expect("chat body is an object");
    for (key, value) in extra {
        obj.insert((*key).to_string(), value.clone());
    }
    body
}

/// `r.json["choices"][0]["message"]["content"]`; None when the path is absent
/// or the content is null. Python returns non-string content as-is and then
/// fails on `.strip()`; we treat it as null instead of panicking.
pub fn chat_text(r: &Resp) -> Option<String> {
    r.json
        .as_ref()?
        .get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("content")?
        .as_str()
        .map(str::to_string)
}

/// Turn a non-200 answer into `(result, reason)` for a feature probe.
pub fn classify_refusal(r: &Resp) -> (String, String) {
    if r.status == 0 {
        return ("error".to_string(), r.error.clone());
    }
    if (400..500).contains(&r.status) || r.status == 501 {
        return ("no".to_string(), r.why(110));
    }
    if r.status >= 500 {
        let lower = r.text.to_lowercase();
        if UNSUPPORTED.iter().any(|needle| lower.contains(needle)) {
            return ("no".to_string(), r.why(110));
        }
    }
    ("error".to_string(), r.why(110))
}

/// Record one feature. Drift is true only when the result is not "skipped",
/// the registry names an expectation, and the two differ.
pub fn record_feature(
    res: &mut CheckResult,
    cfg: &EngineConfig,
    name: &str,
    result: &str,
    reason: &str,
) {
    res.features.insert(
        name.to_string(),
        Feature::record(result, reason, cfg.expected(name)),
    );
}

/// Python truthiness, for `if lp:` and `if hj.get("warming")`.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python's `repr()` for a string: the quote Python would pick, with the
/// escapes it would use. Feature reasons are compared verbatim by the gate.
pub fn py_repr(s: &str) -> String {
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

/// The `send()` wrapper of `check_tensorfold`: it owns the background-priority
/// state shared by the three probes, and the result it writes notes into.
pub struct BgSender<'a> {
    client: &'a mut Client,
    result: &'a mut CheckResult,
    bg: bool,
    bg_timeout: f64,
}

impl<'a> BgSender<'a> {
    pub fn new(
        client: &'a mut Client,
        result: &'a mut CheckResult,
        bg: bool,
        bg_timeout: f64,
    ) -> Self {
        Self {
            client,
            result,
            bg,
            bg_timeout,
        }
    }

    /// The result record this sender writes notes into, for recording feature
    /// outcomes between sends.
    pub fn result(&mut self) -> &mut CheckResult {
        self.result
    }

    /// POST through the background-priority state machine. A timeout while
    /// background priority is on leaves a note, re-sends the probe plain, and
    /// turns background priority off for the remaining probes.
    pub fn send(&mut self, path: &str, body: &Value) -> Resp {
        if !self.bg {
            return self.client.post(path, body);
        }
        let mut prioritized = body.clone();
        if let Some(obj) = prioritized.as_object_mut() {
            obj.insert("priority".to_string(), json!("background"));
        }
        let r = self
            .client
            .post_with_timeout(path, &prioritized, self.bg_timeout);
        if r.status == 0 && r.error.contains("timed out") {
            self.bg = false;
            self.result.notes.push(format!(
                "a background-priority probe waited {}s behind other work; it and the remaining probes were re-sent at normal priority",
                self.bg_timeout
            ));
            return self.client.post(path, body);
        }
        r
    }
}

/// `guided_choice` probe: a two-way choice the engine must pick from.
pub fn probe_guided(send: &mut BgSender<'_>, model: &str) -> (String, String) {
    let choices = ["apple", "granite"];
    let body = chat_body(
        model,
        "Which of these is a fruit?",
        8,
        &[("guided_choice", json!(choices))],
    );
    let r = send.send("/v1/chat/completions", &body);
    if !r.ok() {
        return classify_refusal(&r);
    }
    let Some(text) = chat_text(&r) else {
        return (
            "no".to_string(),
            "HTTP 200 but content was null (guidance ignored, or token budget spent thinking)"
                .to_string(),
        );
    };
    let trimmed = text.trim();
    if choices.contains(&trimmed) {
        return ("yes".to_string(), format!("answered {}", py_repr(trimmed)));
    }
    let shown: String = trimmed.chars().take(40).collect();
    (
        "no".to_string(),
        format!(
            "HTTP 200 but answered {}, not one of the choices (ignored)",
            py_repr(&shown)
        ),
    )
}

/// `background_priority` probe: does the engine accept `priority: "background"`?
pub fn probe_background(c: &mut Client, model: &str) -> (String, String) {
    let body = chat_body(model, "Say ok.", 4, &[("priority", json!("background"))]);
    let r = c.post("/v1/chat/completions", &body);
    if r.ok() {
        return ("yes".to_string(), "accepted (HTTP 200)".to_string());
    }
    classify_refusal(&r)
}

/// `logprobs` probe: one token, with log probabilities requested.
pub fn probe_logprobs(send: &mut BgSender<'_>, model: &str) -> (String, String) {
    let body = chat_body(
        model,
        "Say ok.",
        1,
        &[("logprobs", json!(true)), ("top_logprobs", json!(1))],
    );
    let r = send.send("/v1/chat/completions", &body);
    if !r.ok() {
        return classify_refusal(&r);
    }
    let lp = r
        .json
        .as_ref()
        .and_then(|j| j.get("choices"))
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("logprobs"));
    if lp.is_some_and(truthy) {
        ("yes".to_string(), "logprobs returned".to_string())
    } else {
        (
            "no".to_string(),
            "HTTP 200 but no logprobs in the answer (silently ignored)".to_string(),
        )
    }
}

/// `prompt_logprobs` probe: the completions endpoint with prompt logprobs.
pub fn probe_prompt_logprobs(send: &mut BgSender<'_>, model: &str) -> (String, String) {
    let body = json!({
        "model": model,
        "prompt": "The sky is",
        "max_tokens": 1,
        "temperature": 0,
        "prompt_logprobs": 1,
    });
    let r = send.send("/v1/completions", &body);
    if !r.ok() {
        return classify_refusal(&r);
    }
    let json = r.json.as_ref();
    let mut found = json.and_then(|j| j.get("prompt_logprobs"));
    if found.is_none() {
        found = json
            .and_then(|j| j.get("choices"))
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("prompt_logprobs"));
    }
    if found.is_some_and(truthy) {
        ("yes".to_string(), "prompt_logprobs returned".to_string())
    } else {
        (
            "no".to_string(),
            "HTTP 200 but no prompt_logprobs field (silently ignored)".to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::CheckResult;

    fn resp(status: u16, text: &str) -> Resp {
        Resp::new(status, text.to_string(), 0.0, "")
    }

    #[test]
    fn refusal_classification_follows_python() {
        assert_eq!(
            classify_refusal(&Resp::new(
                0,
                String::new(),
                0.0,
                "URLError: [Errno 61] Connection refused"
            )),
            (
                "error".to_string(),
                "URLError: [Errno 61] Connection refused".to_string()
            )
        );
        assert_eq!(
            classify_refusal(&resp(400, r#"{"error": "guided_choice not supported"}"#)),
            (
                "no".to_string(),
                "HTTP 400: guided_choice not supported".to_string()
            )
        );
        assert_eq!(classify_refusal(&resp(499, "")).0, "no");
        assert_eq!(classify_refusal(&resp(501, "")).0, "no");
        assert_eq!(
            classify_refusal(&resp(500, r#"{"error": "model not supported"}"#)),
            (
                "no".to_string(),
                "HTTP 500: model not supported".to_string()
            )
        );
        assert_eq!(classify_refusal(&resp(503, "LIBTF: no such op")).0, "no");
        assert_eq!(classify_refusal(&resp(502, "field required")).0, "no");
        assert_eq!(classify_refusal(&resp(500, "it is unsupported")).0, "no");
        assert_eq!(classify_refusal(&resp(500, "not implemented yet")).0, "no");
        assert_eq!(
            classify_refusal(&resp(500, r#"{"error": "boom"}"#)),
            ("error".to_string(), "HTTP 500: boom".to_string())
        );
    }

    #[test]
    fn record_feature_drift_rules() {
        let mut cfg = EngineConfig::default();
        cfg.expected_features
            .insert("logprobs".to_string(), "yes".to_string());

        let mut res = CheckResult::new("e", "http://x", "h", "tensorfold");
        record_feature(&mut res, &cfg, "logprobs", "no", "absent");
        let feature = &res.features["logprobs"];
        assert_eq!(feature.result, "no");
        assert_eq!(feature.expected.as_deref(), Some("yes"));
        assert!(feature.drift);

        record_feature(&mut res, &cfg, "logprobs", "yes", "returned");
        assert!(!res.features["logprobs"].drift);

        record_feature(&mut res, &cfg, "logprobs", "skipped", "shared engine busy");
        assert!(!res.features["logprobs"].drift);

        record_feature(&mut res, &cfg, "guided_choice", "no", "refused");
        assert_eq!(res.features["guided_choice"].expected, None);
        assert!(!res.features["guided_choice"].drift);
    }

    #[test]
    fn chat_body_matches_python_shape() {
        let body = chat_body(
            "M-1",
            SANITY_PROMPT,
            16,
            &[("priority", json!("background"))],
        );
        assert_eq!(body["model"], json!("M-1"));
        assert_eq!(body["messages"][0]["role"], json!("user"));
        assert_eq!(body["messages"][0]["content"], json!(SANITY_PROMPT));
        assert_eq!(body["max_tokens"], json!(16));
        assert_eq!(body["temperature"], json!(0));
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"],
            json!(false)
        );
        assert_eq!(body["priority"], json!("background"));
    }

    #[test]
    fn python_repr_quotes_and_escapes() {
        assert_eq!(py_repr("apple"), "'apple'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("line\nbreak\t"), "'line\\nbreak\\t'");
        assert_eq!(py_repr(""), "''");
    }
}
