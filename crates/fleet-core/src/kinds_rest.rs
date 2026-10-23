//! The REST-only engine kinds: laya, lfm2d and jev-glm. Ports of `check_laya`,
//! `check_lfm2d`, `check_jev_glm` and the shared `_check_systemone` from
//! `check/fleet_check.py`.
//!
//! TensorFold's feature probes live in `probes`; this module only speaks HTTP.

use serde_json::{json, Value};

use crate::jsonpy;
use crate::opts::Opts;
use crate::registry::EngineConfig;
use crate::result::{round3, CheckResult, Status};
use crate::{health_up, py_repr, py_str, py_truthy, Client, Resp};

/// One engine-kind check. The registry entry travels beside the result because
/// Python's `Result.cfg` is consulted throughout the checks: expected model,
/// health path, lfm2d read mode and exporter URL.
pub type CheckFn = fn(&mut CheckResult, &EngineConfig, &mut Client, &Opts);

/// The check for a REST-only kind, or `None` for a kind another module owns.
pub fn rest_kind_check(kind: &str) -> Option<CheckFn> {
    match kind {
        "laya" => Some(check_laya),
        "lfm2d" => Some(check_lfm2d),
        "jev-glm" => Some(check_jev_glm),
        _ => None,
    }
}

/// The two health gates laya and jev-glm share: a non-ok answer is down, and a
/// body that is not healthy is down too. Returns false when the caller must
/// stop.
fn health_gate(res: &mut CheckResult, h: &Resp) -> bool {
    if !h.ok() {
        res.fail(Status::Down, format!("health: {}", h.why(110)));
        return false;
    }
    let up = h.json.as_ref().is_some_and(health_up);
    if !up {
        res.fail(
            Status::Down,
            format!("health says not ok: {}", h.message(80)),
        );
        return false;
    }
    true
}

/// The laya decision classifier: health, then one intent question that must
/// come back as "billing".
pub fn check_laya(res: &mut CheckResult, cfg: &EngineConfig, c: &mut Client, _opts: &Opts) {
    let h = c.get(cfg.health_path());
    if !health_gate(res, &h) {
        return;
    }
    let body = h.json.as_ref().expect("health gate requires a JSON body");

    let loaded = match body.get("loaded") {
        Some(value) if py_truthy(value) => join_loaded(value),
        _ => String::new(),
    };
    let loaded = if loaded.is_empty() {
        "nothing".to_string()
    } else {
        loaded
    };
    let device = match body.get("device") {
        Some(value) => py_str(value),
        None => "?".to_string(),
    };
    res.notes.push(format!("loaded {loaded} on {device}"));

    let body = json!({
        "state": {"text": "I was charged twice for my order, please refund me."},
        "questions": {
            "intent": {
                "type": "choice",
                "instructions": "What is this message about?",
                "criteria": {
                    "billing": "payment, a charge or a refund",
                    "shipping": "delivery of an order",
                    "other": "anything else",
                },
            },
        },
    });
    let r = c.post("/v1/systemone", &body);
    res.sanity.latency_s = Some(round3(r.elapsed));
    check_systemone(res, &r, &cfg.model, "intent", "billing");
}

/// Python's `_check_systemone`: one choice question out of a `/v1/systemone`
/// answer. A bad answer fails the sanity slot; a wrong model is a separate
/// problem on top of it.
fn check_systemone(res: &mut CheckResult, r: &Resp, model: &str, qid: &str, want: &str) {
    if !r.ok() {
        let why = r.why(110);
        res.sanity.result = "error".to_string();
        res.sanity.detail = why.clone();
        res.fail(
            if r.status == 0 {
                Status::Down
            } else {
                Status::Wrong
            },
            format!("sanity request: {why}"),
        );
        return;
    }

    let Some(json_body) = r.json.as_ref() else {
        return missing_answers(res);
    };
    let Some(answers) = json_body.get("answers").and_then(Value::as_object) else {
        return missing_answers(res);
    };
    let Some(answer) = answers.get(qid).and_then(Value::as_object) else {
        return missing_answers(res);
    };
    let Some(choice) = answer.get("choice") else {
        return missing_answers(res);
    };

    let got_model = json_body
        .get("model")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let probs = answer.get("probabilities").and_then(Value::as_object);
    res.model = jsonpy::number_text(&got_model);

    if choice.as_str() == Some(want) {
        let extra = match probs.and_then(|probs| probs.get(want)) {
            Some(Value::Number(p)) => p
                .as_f64()
                .map(|p| format!(" (p={p:.2})"))
                .unwrap_or_default(),
            Some(Value::Bool(p)) => format!(" (p={:.2})", if *p { 1.0 } else { 0.0 }),
            Some(p) if jsonpy::is_nonfinite(p) => {
                format!(" (p={})", jsonpy::number_text(p))
            }
            _ => String::new(),
        };
        res.sanity.result = "pass".to_string();
        res.sanity.detail = format!("chose {}{extra}", render_value(choice));
    } else {
        res.sanity.result = "fail".to_string();
        res.sanity.detail = format!(
            "chose {}, expected {}",
            render_value(choice),
            py_repr(&Value::String(want.to_string()))
        );
        res.fail(Status::Wrong, format!("sanity: {}", res.sanity.detail));
    }

    // Python's `if got_model and got_model != model`: truthiness on the raw
    // value, then a raw comparison against the registry's string.
    if py_truthy(&got_model) && got_model != Value::String(model.to_string()) {
        res.fail(
            Status::Wrong,
            format!(
                "model is {}, registry expects {model}",
                jsonpy::number_text(&got_model)
            ),
        );
    }
}

/// Python's `repr()` for a choice: a sentinel is the float it stands for, so
/// `repr(float("nan"))` renders bare (`nan`), not as a quoted string.
fn render_value(value: &Value) -> String {
    if jsonpy::is_nonfinite(value) {
        jsonpy::number_text(value)
    } else {
        py_repr(value)
    }
}

fn missing_answers(res: &mut CheckResult) {
    res.sanity.result = "fail".to_string();
    res.sanity.detail = "response has no answers/choice".to_string();
    res.fail(Status::Wrong, "sanity: response has no answers/choice");
}

/// The lfm2d opinion/span engine: the model list is the health check, then one
/// cheap read (`specs` or `spans`), then the optional exporter.
pub fn check_lfm2d(res: &mut CheckResult, cfg: &EngineConfig, c: &mut Client, _opts: &Opts) {
    let h = c.get(cfg.health_path());
    if !h.ok() {
        res.fail(Status::Down, format!("model list: {}", h.why(110)));
        return;
    }
    let Some(models) = h.json.as_ref().and_then(Value::as_array) else {
        res.fail(Status::Wrong, "model list is not a JSON list");
        return;
    };

    let ids: Vec<Option<String>> = models
        .iter()
        .filter_map(Value::as_object)
        .map(|model| model.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    if !ids
        .iter()
        .any(|id| id.as_deref() == Some(cfg.model.as_str()))
    {
        res.model = ids.first().cloned().flatten().unwrap_or_default();
        let rendered: Vec<String> = ids
            .iter()
            .map(|id| id.clone().unwrap_or_else(|| "None".to_string()))
            .collect();
        let list = if rendered.is_empty() {
            "none".to_string()
        } else {
            rendered.join(", ")
        };
        res.fail(
            Status::Wrong,
            format!("models are {list}, registry expects {}", cfg.model),
        );
        return;
    }
    res.model = cfg.model.clone();

    let (r, good, detail) = if cfg.read() == "specs" {
        let r = c.get("/v1/opinion/specs");
        let specs = r.json.as_ref().and_then(Value::as_array);
        let good = r.ok() && specs.is_some_and(|specs| !specs.is_empty());
        let detail = if good {
            format!("{} spec(s) listed", specs.map_or(0, Vec::len))
        } else {
            "expected at least one spec".to_string()
        };
        (r, good, detail)
    } else {
        let body = json!({"inputs": ["fleet-check ping"]});
        let r = c.post("/v1/spans/credentials", &body);
        let good = r.ok() && r.json.as_ref().is_some_and(Value::is_array);
        let detail = if good {
            "span scan answered".to_string()
        } else {
            "expected a list of span lists".to_string()
        };
        (r, good, detail)
    };

    res.sanity.latency_s = Some(round3(r.elapsed));
    if good {
        res.sanity.result = "pass".to_string();
        res.sanity.detail = detail;
    } else {
        let why = if r.ok() { detail } else { r.why(110) };
        res.sanity.result = if r.ok() {
            "fail".to_string()
        } else {
            "error".to_string()
        };
        res.sanity.detail = why.clone();
        res.fail(
            if r.status == 0 {
                Status::Down
            } else {
                Status::Wrong
            },
            format!("read: {why}"),
        );
    }

    if let Some(exporter) = cfg.exporter_url.as_deref().filter(|url| !url.is_empty()) {
        let e = c.get(&format!("{}/healthz", exporter.trim_end_matches('/')));
        if e.ok() && e.text.trim() == "ok" {
            res.notes.push("exporter /healthz ok".to_string());
        } else {
            res.fail(Status::Wrong, format!("exporter {exporter}: {}", e.why(60)));
        }
    }
}

/// jev-glm: health and `/ready` (a non-ok `/ready` is wrong, not down), then
/// one verdict question with a long timeout because the GLM behind it is slow.
pub fn check_jev_glm(res: &mut CheckResult, cfg: &EngineConfig, c: &mut Client, opts: &Opts) {
    let h = c.get("/health");
    if !health_gate(res, &h) {
        return;
    }
    let health_body = h.json.as_ref().expect("health gate requires a JSON body");
    res.model = health_body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let ready = c.get("/ready");
    if !ready.ok() {
        res.fail(
            Status::Wrong,
            format!(
                "/ready: the GLM behind it is not answering ({})",
                ready.why(70)
            ),
        );
        return;
    }

    let body = json!({
        "state": "ls -la",
        "model": cfg.model.clone(),
        "questions": {
            "verdict": {
                "type": "choice",
                "instructions": "What kind of shell command is this?",
                "criteria": {
                    "read": "only looks at things",
                    "destroy": "deletes data that cannot be brought back",
                },
            },
        },
    });
    let r = c.post_with_timeout("/v1/systemone", &body, opts.timeout.max(90.0));
    res.sanity.latency_s = Some(round3(r.elapsed));
    check_systemone(res, &r, &cfg.model, "verdict", "read");
}

/// Python's `','.join(loaded)`: strings join as-is, a bare string joins its
/// characters, and an object joins its keys.
fn join_loaded(value: &Value) -> String {
    match value {
        Value::Array(items) => items.iter().map(py_str).collect::<Vec<_>>().join(","),
        Value::String(value) => value
            .chars()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(map) => map.keys().cloned().collect::<Vec<_>>().join(","),
        other => py_str(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::CheckResult;

    fn resp(text: &str) -> Resp {
        Resp::new(200, text.to_string(), 0.0, "")
    }

    fn result() -> CheckResult {
        CheckResult::new("e", "http://x", "h", "laya")
    }

    #[test]
    fn systemone_nan_probability_and_model_render_like_python() {
        let mut res = result();
        check_systemone(
            &mut res,
            &resp(
                r#"{"answers": {"intent": {"choice": "billing",
                    "probabilities": {"billing": NaN}}}, "model": NaN}"#,
            ),
            "M-1",
            "intent",
            "billing",
        );
        assert_eq!(res.sanity.result, "pass");
        assert_eq!(res.sanity.detail, "chose 'billing' (p=nan)");
        assert_eq!(res.model, "nan");
        assert_eq!(res.status, Status::Wrong);
        assert_eq!(res.problems, vec!["model is nan, registry expects M-1"]);
    }

    #[test]
    fn systemone_infinite_probability_renders_with_its_sign() {
        for (token, rendered) in [("Infinity", "inf"), ("-Infinity", "-inf")] {
            let mut res = result();
            let body = r#"{"answers": {"intent": {"choice": "billing",
                "probabilities": {"billing": TOKEN}}}, "model": "M-1"}"#
                .replace("TOKEN", token);
            check_systemone(&mut res, &resp(&body), "M-1", "intent", "billing");
            assert_eq!(res.sanity.result, "pass", "{token}");
            assert_eq!(
                res.sanity.detail,
                format!("chose 'billing' (p={rendered})"),
                "{token}"
            );
            assert_eq!(res.status, Status::Ok, "{token}");
            assert!(res.problems.is_empty(), "{token}");
        }
    }

    #[test]
    fn systemone_nan_choice_renders_as_a_bare_float_repr() {
        let mut res = result();
        check_systemone(
            &mut res,
            &resp(r#"{"answers": {"intent": {"choice": NaN}}, "model": "M-1"}"#),
            "M-1",
            "intent",
            "billing",
        );
        assert_eq!(res.sanity.result, "fail");
        assert_eq!(res.sanity.detail, "chose nan, expected 'billing'");
        assert_eq!(res.status, Status::Wrong);
        assert_eq!(res.problems, vec!["sanity: chose nan, expected 'billing'"]);
    }
}
