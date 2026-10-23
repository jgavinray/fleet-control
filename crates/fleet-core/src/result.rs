//! The per-engine result record, mirroring `Result` in
//! `check/fleet_check.py`: worst-wins failures, drift detection, verdict and
//! the exact JSON shape the gate compares.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{json, Map, Value};

use crate::jsonpy;

/// Engine state. Rank order: down (3) > wrong (2) > slow (1) > ok / skipped-busy (0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Ok,
    Slow,
    Wrong,
    Down,
    SkippedBusy,
}

impl Status {
    /// Python's `Result.RANK`. A failure only ever raises the status.
    pub fn rank(self) -> u8 {
        match self {
            Status::Ok | Status::SkippedBusy => 0,
            Status::Slow => 1,
            Status::Wrong => 2,
            Status::Down => 3,
        }
    }

    /// The string Python puts in the `status` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Slow => "slow",
            Status::Wrong => "wrong",
            Status::Down => "down",
            Status::SkippedBusy => "skipped-busy",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The sanity answer record: result ("n/a" | "pass" | "fail" | "error" |
/// "skipped"), a short detail, and the seconds spent (None when no request was
/// made).
#[derive(Clone, Debug)]
pub struct Sanity {
    pub result: String,
    pub detail: String,
    pub latency_s: Option<f64>,
}

impl Default for Sanity {
    fn default() -> Self {
        Self {
            result: "n/a".to_string(),
            detail: String::new(),
            latency_s: None,
        }
    }
}

impl Sanity {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "result": self.result,
            "detail": self.detail,
            "latency_s": self.latency_s,
        })
    }
}

/// One measured feature. `drift` is true when a result differs from what the
/// registry expects; a "skipped" result never drifts.
///
/// `expected` is the registry's raw value, like Python's `expected_features`
/// entry: only an equal string matches the measured result, so a number or a
/// list always drifts.
#[derive(Clone, Debug, PartialEq)]
pub struct Feature {
    pub result: String,
    pub reason: String,
    pub expected: Option<Value>,
    pub drift: bool,
}

impl Feature {
    /// Record a feature result the way Python's `record_feature` does.
    pub fn record(
        result: impl Into<String>,
        reason: impl Into<String>,
        expected: Option<&Value>,
    ) -> Self {
        let result = result.into();
        let expected = expected.cloned();
        let drift = result != "skipped"
            && expected
                .as_ref()
                .is_some_and(|expected| &Value::String(result.clone()) != expected);
        Self {
            result,
            reason: reason.into(),
            expected,
            drift,
        }
    }

    pub fn to_json(&self) -> Value {
        // Python's `json.dumps` writes a float nan/inf as the bare token, which
        // is not valid JSON; the Python tool's own --json output would then be
        // unreadable by the harness. Such a registry value is untestable
        // end-to-end, so a sentinel renders as the token in a JSON string here.
        let expected = match self.expected.as_ref().and_then(Value::as_str) {
            Some(jsonpy::NAN_TOK) => Value::String("NaN".to_string()),
            Some(jsonpy::INF_TOK) => Value::String("Infinity".to_string()),
            Some(jsonpy::NEGINF_TOK) => Value::String("-Infinity".to_string()),
            _ => self.expected.clone().unwrap_or(Value::Null),
        };
        json!({
            "result": self.result,
            "reason": self.reason,
            "expected": expected,
            "drift": self.drift,
        })
    }
}

/// Everything one engine check learned, plus the registry fields that go into
/// the report (`url`, `host`, `kind`).
#[derive(Clone, Debug)]
pub struct CheckResult {
    pub name: String,
    pub url: String,
    pub host: String,
    pub kind: String,
    pub status: Status,
    pub model: String,
    pub version: String,
    pub load: String,
    pub sanity: Sanity,
    pub features: BTreeMap<String, Feature>,
    pub problems: Vec<String>,
    pub notes: Vec<String>,
    pub requests: u32,
    /// Set when Python would die on an uncaught exception for this engine
    /// (a broken `model_ids` shape, a non-string id list). The run prints it
    /// to stderr and exits 1 instead of printing a report.
    pub fatal: Option<String>,
}

impl CheckResult {
    pub fn new(
        name: impl Into<String>,
        url: impl Into<String>,
        host: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            host: host.into(),
            kind: kind.into(),
            status: Status::Ok,
            model: String::new(),
            version: String::new(),
            load: String::new(),
            sanity: Sanity::new(),
            features: BTreeMap::new(),
            problems: Vec::new(),
            notes: Vec::new(),
            requests: 0,
            fatal: None,
        }
    }

    /// Build a result for one registry engine.
    pub fn for_engine(cfg: &crate::registry::EngineConfig) -> Self {
        Self::new(
            cfg.name.clone(),
            cfg.url.clone(),
            cfg.host.clone(),
            cfg.kind.clone(),
        )
    }

    /// Record a problem. The worst status wins: down, then wrong, then slow.
    pub fn fail(&mut self, status: Status, why: impl Into<String>) {
        if status.rank() > self.status.rank() {
            self.status = status;
        }
        self.problems.push(why.into());
    }

    /// Names of the features whose result differs from the registry.
    pub fn drifted(&self) -> Vec<&str> {
        self.features
            .iter()
            .filter(|(_, f)| f.drift)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// "down" | "wrong" | "slow" when the engine failed; else "drift" when a
    /// feature differs; else "ok".
    pub fn verdict(&self) -> &'static str {
        match self.status {
            Status::Down => "down",
            Status::Wrong => "wrong",
            Status::Slow => "slow",
            Status::Ok | Status::SkippedBusy => {
                if self.drifted().is_empty() {
                    "ok"
                } else {
                    "drift"
                }
            }
        }
    }

    /// The report record, with Python's field names and nesting. Keys are
    /// sorted because serde_json maps are BTreeMaps (preserve_order stays off).
    pub fn to_json(&self) -> Value {
        let features: Map<String, Value> = self
            .features
            .iter()
            .map(|(name, feature)| (name.clone(), feature.to_json()))
            .collect();
        json!({
            "engine": self.name,
            "url": self.url,
            "host": self.host,
            "kind": self.kind,
            "verdict": self.verdict(),
            "status": self.status.as_str(),
            "model": self.model,
            "version": self.version,
            "load": self.load,
            "sanity": self.sanity.to_json(),
            "features": features,
            "problems": self.problems,
            "notes": self.notes,
            "requests": self.requests,
        })
    }
}

/// Python's `exit_code`: 1 when an engine is down, wrong or slow; 2 when only
/// features drifted; 0 otherwise.
pub fn exit_code(results: &[CheckResult]) -> i32 {
    if results
        .iter()
        .any(|r| matches!(r.verdict(), "down" | "wrong" | "slow"))
    {
        return 1;
    }
    if results.iter().any(|r| r.verdict() == "drift") {
        return 2;
    }
    0
}

/// Python's `round(x, 3)`, used for `latency_s`.
pub fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}
