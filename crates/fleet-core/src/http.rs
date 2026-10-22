//! The counting HTTP client, mirroring `Resp` and `Client` in
//! `check/fleet_check.py`.
//!
//! Every request carries `Accept: application/json`; a request that sends a
//! body also carries `Content-Type: application/json`. Non-2xx answers are
//! captured as ordinary responses (status as-is, body as text), because the
//! checks turn those bodies into reasons. A response with `status == 0` means
//! the engine never answered at all (refused, DNS failure, timeout).

use std::time::{Duration, Instant};

use serde_json::Value;
use ureq::Agent;

/// One HTTP answer. `status` 0 means no answer at all (refused, timeout).
#[derive(Clone, Debug)]
pub struct Resp {
    pub status: u16,
    pub text: String,
    pub elapsed: f64,
    pub error: String,
    pub json: Option<Value>,
}

impl Resp {
    /// Build a response record. `text` is parsed as JSON when it is not empty;
    /// an unparseable body leaves `json` as `None`, like Python's `Resp`.
    pub fn new(status: u16, text: String, elapsed: f64, error: impl Into<String>) -> Self {
        let json = if text.is_empty() {
            None
        } else {
            serde_json::from_str(&text).ok()
        };
        Self {
            status,
            text,
            elapsed,
            error: error.into(),
            json,
        }
    }

    /// Python's `Resp.ok`: exactly HTTP 200.
    pub fn ok(&self) -> bool {
        self.status == 200
    }

    /// A short reason for a failed answer, for the report.
    pub fn why(&self, limit: usize) -> String {
        if self.status == 0 {
            truncate(&self.error, limit)
        } else {
            format!("HTTP {}: {}", self.status, self.message(limit))
        }
    }

    /// The message text out of an error body: `error.message`, `error` as a
    /// string, `detail.message`, `detail` (object json-dumped or string),
    /// `message`, else the trimmed body. Whitespace is collapsed and the result
    /// is truncated to `limit` characters.
    pub fn message(&self, limit: usize) -> String {
        let msg = self
            .extract_message()
            .unwrap_or_else(|| self.text.trim().to_string());
        truncate(&collapse_ws(&msg), limit)
    }

    fn extract_message(&self) -> Option<String> {
        let obj = self.json.as_ref()?.as_object()?;
        let mut msg: Option<String> = None;

        match obj.get("error") {
            Some(Value::Object(err)) => {
                msg = err.get("message").and_then(value_as_string);
            }
            Some(Value::String(err)) => msg = Some(err.clone()),
            _ => {}
        }

        if msg.is_none() {
            match obj.get("detail") {
                Some(Value::Object(detail)) => {
                    msg = Some(match detail.get("message") {
                        Some(m) if truthy(m) => value_to_string(m),
                        _ => py_dumps(&Value::Object(detail.clone())),
                    });
                }
                Some(Value::Null) | None => {}
                Some(detail) => {
                    msg = Some(match detail {
                        Value::String(s) => s.clone(),
                        other => py_dumps(other),
                    });
                }
            }
        }

        if msg.is_none() {
            msg = obj.get("message").and_then(value_as_string);
        }

        msg
    }
}

/// Sequential HTTP client that counts requests, like Python's `Client`.
///
/// `count` is incremented once per attempt, before the attempt is made, so a
/// refused or timed-out request still counts. The gate compares this number.
pub struct Client {
    /// Base URL with no trailing slash.
    pub base: String,
    /// Default per-request timeout in seconds.
    pub timeout: f64,
    /// Requests attempted so far.
    pub count: u32,
    agent: Agent,
}

impl Client {
    pub fn new(base: &str, timeout: f64) -> Self {
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(duration(timeout)))
            .build()
            .into();
        Self {
            base: base.trim_end_matches('/').to_string(),
            timeout,
            count: 0,
            agent,
        }
    }

    pub fn get(&mut self, path: &str) -> Resp {
        self.request("GET", path, None, None)
    }

    pub fn post(&mut self, path: &str, body: &Value) -> Resp {
        self.request("POST", path, Some(body), None)
    }

    /// POST with a per-request timeout override (the background-priority and
    /// jev-glm checks need one).
    pub fn post_with_timeout(&mut self, path: &str, body: &Value, timeout: f64) -> Resp {
        self.request("POST", path, Some(body), Some(timeout))
    }

    /// Issue one request. `path_or_url` is used as-is when it starts with
    /// `http`, otherwise it is appended to `base`. `count` is incremented once
    /// here, before the attempt, including failed attempts.
    pub fn request(
        &mut self,
        method: &str,
        path_or_url: &str,
        body: Option<&Value>,
        timeout: Option<f64>,
    ) -> Resp {
        let url = if path_or_url.starts_with("http") {
            path_or_url.to_string()
        } else {
            format!("{}{}", self.base, path_or_url)
        };
        self.count += 1;
        let started = Instant::now();

        let timeout = match timeout {
            Some(t) if t > 0.0 => t,
            _ => self.timeout,
        };

        let outcome = match method {
            "GET" => {
                let builder = self.agent.get(&url).header("Accept", "application/json");
                builder
                    .config()
                    .timeout_global(Some(duration(timeout)))
                    .build()
                    .call()
            }
            "POST" => {
                let data = body
                    .map(|body| serde_json::to_string(body).unwrap_or_else(|_| "null".to_string()));
                let mut builder = self.agent.post(&url).header("Accept", "application/json");
                if data.is_some() {
                    builder = builder.header("Content-Type", "application/json");
                }
                let builder = builder
                    .config()
                    .timeout_global(Some(duration(timeout)))
                    .build();
                match data {
                    Some(data) => builder.send(data.as_str()),
                    None => builder.send_empty(),
                }
            }
            other => {
                return Resp::new(
                    0,
                    String::new(),
                    started.elapsed().as_secs_f64(),
                    format!("URLError: unsupported method {other}"),
                );
            }
        };

        match outcome {
            Ok(response) => {
                let status = response.status().as_u16();
                let elapsed = started.elapsed().as_secs_f64();
                match response.into_body().read_to_string() {
                    Ok(text) => Resp::new(status, text, elapsed, String::new()),
                    Err(e) => Resp::new(0, String::new(), elapsed, error_text(&e)),
                }
            }
            Err(e) => Resp::new(
                0,
                String::new(),
                started.elapsed().as_secs_f64(),
                error_text(&e),
            ),
        }
    }
}

fn duration(secs: f64) -> Duration {
    Duration::from_secs_f64(secs.max(0.0))
}

/// Python's error strings for the cases the checks look at. Timeouts carry
/// "timed out", which the background-priority logic tests for.
fn error_text(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "URLError: timed out".to_string(),
        ureq::Error::Io(io) => io_error_text(io),
        ureq::Error::HostNotFound => {
            "URLError: [Errno 8] nodename nor servname provided, or not known".to_string()
        }
        ureq::Error::StatusCode(code) => format!("HTTPError: HTTP Error {code}"),
        ureq::Error::ConnectionFailed => "URLError: connection failed".to_string(),
        ureq::Error::BadUri(uri) => format!("URLError: invalid URL {uri}"),
        other => format!("URLError: {other}"),
    }
}

fn io_error_text(e: &std::io::Error) -> String {
    let raw = e.to_string();
    let message = match raw.rfind(" (os error ") {
        Some(i) if raw.ends_with(')') => raw[..i].to_string(),
        _ => raw,
    };
    match e.raw_os_error() {
        Some(code) => format!("URLError: [Errno {code}] {message}"),
        None => format!("URLError: {message}"),
    }
}

/// Collapse runs of whitespace the way `" ".join(s.split())` does in Python.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, limit: usize) -> String {
    s.chars().take(limit).collect()
}

/// Python truthiness, for the `detail.get("message") or json.dumps(detail)`
/// fallback.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `str()` of a JSON value, matching Python's rendering for scalars.
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

fn value_as_string(v: &Value) -> Option<String> {
    if v.is_null() {
        None
    } else {
        Some(value_to_string(v))
    }
}

/// `json.dumps(value)` with Python's default separators: ", " and ": ".
pub fn py_dumps(v: &Value) -> String {
    match v {
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(py_dumps).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", py_dumps(&Value::String(k.clone())), py_dumps(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        other => other.to_string(),
    }
}
