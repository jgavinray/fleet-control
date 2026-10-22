//! The TensorFold engine check, ported 1:1 from `check_tensorfold` in
//! `check/fleet_check.py`.
//!
//! Order matters and is the Python order: health (up, not warming), load and
//! version, model ids (health `model_ids`, else `/v1/models` `data[].id`), the
//! shared-busy skip, the sanity question (background priority when the registry
//! expects it), the `--quick` return, then the background probe and the three
//! feature probes through the `BgSender` wrapper.

use serde_json::Value;

use crate::http::{Client, Resp};
use crate::opts::Opts;
use crate::probes::{self, chat_body, chat_text, record_feature, BgSender, SANITY_PROMPT};
use crate::registry::EngineConfig;
use crate::result::{round3, CheckResult, Sanity, Status};
use crate::{describe_load, health_up, sanity_answer_ok};

/// The four features a TensorFold engine is probed for, in Python's order.
pub const FEATURES: [&str; 4] = [
    "guided_choice",
    "background_priority",
    "logprobs",
    "prompt_logprobs",
];

/// Python's `check_tensorfold`. The caller sets `res.requests = c.count` after
/// the call, exactly as Python's `check_engine` does.
pub fn check_tensorfold(res: &mut CheckResult, cfg: &EngineConfig, c: &mut Client, opts: &Opts) {
    let model = cfg.model.as_str();

    let h = c.get(cfg.health_path());
    if !h.ok() {
        res.fail(Status::Down, format!("health: {}", h.why(110)));
        return;
    }
    let hj = h.json.clone().unwrap_or(Value::Null);
    if !health_up(&hj) {
        res.fail(
            Status::Down,
            format!("health says not ok: {}", h.message(80)),
        );
        return;
    }
    if probes::truthy(hj.get("warming").unwrap_or(&Value::Null)) {
        res.fail(Status::Down, "still warming up (health.warming is true)");
        return;
    }
    let (load, in_flight) = describe_load(&hj);
    let busy = in_flight > 0;
    res.load = load;
    res.version = version_string(hj.get("version"));

    let ids = match hj.get("model_ids") {
        Some(Value::Array(items)) if !items.is_empty() => {
            Some(items.iter().map(id_string).collect::<Vec<String>>())
        }
        _ => {
            let m = c.get("/v1/models");
            if !m.ok() {
                res.fail(Status::Down, format!("model list: {}", m.why(110)));
                return;
            }
            model_ids(&m)
        }
    };
    let Some(ids) = ids else {
        res.fail(Status::Wrong, "model list has no data[].id");
        return;
    };
    res.model = ids.first().cloned().unwrap_or_default();
    if !ids.iter().any(|id| id == model) {
        let joined = if ids.is_empty() {
            "none".to_string()
        } else {
            ids.join(", ")
        };
        res.fail(
            Status::Wrong,
            format!("model is {joined}, registry expects {model}"),
        );
        return;
    }

    if cfg.shared && busy && !opts.force_shared {
        res.status = Status::SkippedBusy;
        res.sanity = Sanity {
            result: "skipped".to_string(),
            detail: "shared engine is busy".to_string(),
            latency_s: None,
        };
        res.notes.push(
            "shared engine is busy (live.connections > 0): sanity question and probes skipped so the other user is not slowed. Re-run when idle, or use --force-shared."
                .to_string(),
        );
        if !opts.quick {
            for name in FEATURES {
                record_feature(res, cfg, name, "skipped", "shared engine busy");
            }
        }
        return;
    }

    // The sanity question carries priority:"background" when the registry says
    // the engine accepts it. A 4xx then means it refuses it, so retry plain.
    let want_bg = cfg.expected("background_priority") == Some("yes");
    let plain_body = chat_body(model, SANITY_PROMPT, 16, &[]);
    let mut r = if want_bg {
        let bg_body = chat_body(
            model,
            SANITY_PROMPT,
            16,
            &[("priority", serde_json::json!("background"))],
        );
        c.post_with_timeout("/v1/chat/completions", &bg_body, opts.bg_timeout)
    } else {
        c.post("/v1/chat/completions", &plain_body)
    };
    let mut bg_result: Option<(String, String)> = None;
    let mut bg_ok = want_bg;
    let mut spent = r.elapsed;
    if want_bg {
        if r.ok() {
            bg_result = Some((
                "yes".to_string(),
                "accepted with the sanity request (HTTP 200)".to_string(),
            ));
        } else if (400..500).contains(&r.status) {
            bg_result = Some(probes::classify_refusal(&r));
            bg_ok = false;
            r = c.post("/v1/chat/completions", &plain_body);
            spent += r.elapsed;
        } else if r.status == 0 && r.error.contains("timed out") && busy {
            // Accepted but starved: a saturated engine serves background last.
            bg_result = Some((
                "yes".to_string(),
                format!(
                    "accepted, but starved {}s behind other work; sanity re-sent at normal priority",
                    opts.bg_timeout
                ),
            ));
            bg_ok = false;
            res.notes.push(format!(
                "background-priority sanity request waited {}s behind other work ({}); re-sent once at normal priority",
                opts.bg_timeout, res.load
            ));
            r = c.post("/v1/chat/completions", &plain_body);
            spent += r.elapsed;
        }
    }
    res.sanity.latency_s = Some(round3(spent));
    if !r.ok() {
        res.sanity.result = "error".to_string();
        res.sanity.detail = r.why(110);
        if r.status == 0 && r.error.contains("timed out") && busy {
            // Background priority queues behind everyone; a busy engine is not
            // a dead one.
            res.fail(
                Status::Slow,
                format!(
                    "sanity request timed out after {}s while the engine is {}: not down, but it did not answer in time (re-run later, or raise --timeout)",
                    c.timeout, res.load
                ),
            );
        } else {
            let status = if r.status == 0 || r.status >= 500 {
                Status::Down
            } else {
                Status::Wrong
            };
            let detail = res.sanity.detail.clone();
            res.fail(status, format!("sanity request: {detail}"));
        }
        return;
    }
    let text = chat_text(&r);
    let shown = match &text {
        None => "null".to_string(),
        Some(text) => probes::py_repr(&text.trim().chars().take(30).collect::<String>()),
    };
    if text.as_deref().is_some_and(sanity_answer_ok) {
        res.sanity.result = "pass".to_string();
        res.sanity.detail = format!("answered {shown}");
    } else {
        res.sanity.result = "fail".to_string();
        res.sanity.detail = format!("answered {shown}, expected '42'");
        let detail = res.sanity.detail.clone();
        res.fail(Status::Wrong, format!("sanity: {detail}"));
        return;
    }

    if opts.quick {
        return;
    }

    let bg_result = bg_result.unwrap_or_else(|| probes::probe_background(c, model));
    record_feature(res, cfg, "background_priority", &bg_result.0, &bg_result.1);
    // Priority on the probes only where the engine accepts it and has not
    // starved us.
    let bg = bg_ok && bg_result.0 == "yes";
    let mut send = BgSender::new(c, res, bg, opts.bg_timeout);
    let (result, reason) = probes::probe_guided(&mut send, model);
    record_feature(send.result(), cfg, "guided_choice", &result, &reason);
    let (result, reason) = probes::probe_logprobs(&mut send, model);
    record_feature(send.result(), cfg, "logprobs", &result, &reason);
    let (result, reason) = probes::probe_prompt_logprobs(&mut send, model);
    record_feature(send.result(), cfg, "prompt_logprobs", &result, &reason);
}

/// `ids = [d["id"] for d in m.json["data"]]`, None for the shapes Python turns
/// into its "model list has no data[].id" failure.
fn model_ids(m: &Resp) -> Option<Vec<String>> {
    let data = m.json.as_ref()?.as_object()?.get("data")?.as_array()?;
    let mut ids = Vec::with_capacity(data.len());
    for item in data {
        ids.push(id_string(item.as_object()?.get("id")?));
    }
    Some(ids)
}

/// Python `str()` of a model id, for joining ids in the mismatch message.
fn id_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python's `str(hj.get("version") or "")`: empty for absent, null, false, 0
/// or an empty string.
fn version_string(v: Option<&Value>) -> String {
    let Some(v) = v.filter(|v| probes::truthy(v)) else {
        return String::new();
    };
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::thread;

    use serde_json::{json, Value};

    use super::*;
    use crate::result::{exit_code, Feature};

    const MODEL: &str = "M-1";

    type Handler = Box<dyn Fn(&Value) -> Reply + Send + Sync>;

    #[derive(Clone)]
    struct Reply {
        status: u16,
        body: String,
    }

    impl Reply {
        fn json(status: u16, body: Value) -> Self {
            Self {
                status,
                body: body.to_string(),
            }
        }
    }

    /// One mock engine, mirroring `crates/parity.py`'s scenario dict: absent
    /// routes answer 404, and one behaviour is overridden per test.
    struct Scenario {
        health: Reply,
        models: Option<Reply>,
        chat: Handler,
        completions: Option<Handler>,
        shared: bool,
        expected: BTreeMap<String, String>,
    }

    fn scenario() -> Scenario {
        Scenario {
            health: Reply::json(404, json!({"error": "no route"})),
            models: None,
            chat: Box::new(|_| Reply::json(404, json!({"error": "no route"}))),
            completions: None,
            shared: false,
            expected: BTreeMap::new(),
        }
    }

    impl Scenario {
        fn health(mut self, status: u16, body: Value) -> Self {
            self.health = Reply::json(status, body);
            self
        }

        fn models(mut self, status: u16, body: Value) -> Self {
            self.models = Some(Reply::json(status, body));
            self
        }

        fn chat(mut self, handler: impl Fn(&Value) -> Reply + Send + Sync + 'static) -> Self {
            self.chat = Box::new(handler);
            self
        }

        fn completions(
            mut self,
            handler: impl Fn(&Value) -> Reply + Send + Sync + 'static,
        ) -> Self {
            self.completions = Some(Box::new(handler));
            self
        }

        fn shared(mut self, shared: bool) -> Self {
            self.shared = shared;
            self
        }

        fn expected(mut self, pairs: &[(&str, &str)]) -> Self {
            self.expected = pairs
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect();
            self
        }
    }

    fn healthy_for(model: &str) -> Value {
        json!({
            "ok": true,
            "model_ids": [model],
            "version": "1.2.3",
            "requests_running": 0,
            "requests_waiting": 0,
        })
    }

    fn healthy() -> Value {
        healthy_for(MODEL)
    }

    fn busy() -> Value {
        json!({
            "ok": true,
            "model_ids": [MODEL],
            "version": "1.2.3",
            "requests_running": 2,
            "requests_waiting": 1,
        })
    }

    fn chat(content: &str) -> Value {
        json!({"choices": [{"message": {"content": content}}]})
    }

    fn ok_chat() -> Reply {
        Reply::json(200, chat("42"))
    }

    fn ideal_chat(body: &Value) -> Reply {
        let content = body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.first())
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str);
        if body.get("guided_choice").is_some() {
            return Reply::json(200, chat("apple"));
        }
        if probes::truthy(body.get("logprobs").unwrap_or(&Value::Null)) {
            return Reply::json(
                200,
                json!({
                    "choices": [{
                        "message": {"content": "ok"},
                        "logprobs": {"content": [{"token": "ok", "logprob": -0.01}]},
                    }],
                }),
            );
        }
        if content == Some(SANITY_PROMPT) {
            return ok_chat();
        }
        if body.get("priority").and_then(Value::as_str) == Some("background") {
            return Reply::json(200, chat("ok"));
        }
        ok_chat()
    }

    fn ideal_plp(_body: &Value) -> Reply {
        Reply::json(
            200,
            json!({"prompt_logprobs": [{"token": " The", "logprob": -0.6}]}),
        )
    }

    fn refuse_bg(body: &Value) -> Reply {
        if body.get("priority").is_some() {
            Reply::json(400, json!({"error": "priority not supported"}))
        } else {
            ideal_chat(body)
        }
    }

    fn sleep(secs: f64) {
        thread::sleep(std::time::Duration::from_secs_f64(secs));
    }

    /// Start the mock engine on a loopback port; one thread per connection so
    /// a deliberately slow answer cannot block the retry that follows it.
    fn serve(sc: Scenario) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
        let addr = listener.local_addr().expect("local addr");
        let sc = Arc::new(sc);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let sc = Arc::clone(&sc);
                thread::spawn(move || handle(stream, &sc));
            }
        });
        format!("http://{addr}")
    }

    fn handle(stream: TcpStream, sc: &Scenario) {
        let Ok(socket) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(socket);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => return,
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut raw = vec![0u8; content_length];
        if content_length > 0 && reader.read_exact(&mut raw).is_err() {
            return;
        }
        let body: Option<Value> = if raw.is_empty() {
            None
        } else {
            serde_json::from_slice(&raw).ok()
        };
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        let reply = match (method, path) {
            ("GET", "/health") => sc.health.clone(),
            ("GET", "/v1/models") => sc
                .models
                .clone()
                .unwrap_or_else(|| Reply::json(404, json!({"error": "none"}))),
            ("POST", "/v1/chat/completions") => (sc.chat)(body.as_ref().unwrap_or(&Value::Null)),
            ("POST", "/v1/completions") => match &sc.completions {
                Some(handler) => handler(body.as_ref().unwrap_or(&Value::Null)),
                None => Reply::json(404, json!({"error": "no route"})),
            },
            _ => Reply::json(404, json!({"error": "no route"})),
        };
        let payload = reply.body.as_bytes();
        let head = format!(
            "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            reply.status,
            payload.len()
        );
        let mut stream = stream;
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(payload);
        let _ = stream.flush();
    }

    fn opts(quick: bool) -> Opts {
        Opts {
            quick,
            json: false,
            registry: None,
            timeout: 5.0,
            bg_timeout: 5.0,
            force_shared: false,
            engines: Vec::new(),
        }
    }

    /// Run one scenario through the real `Client` and `check_tensorfold`,
    /// setting `requests` the way Python's `check_engine` does.
    fn run(sc: Scenario, opts: &Opts) -> (CheckResult, u32) {
        let shared = sc.shared;
        let expected = sc.expected.clone();
        let url = serve(sc);
        let cfg = EngineConfig {
            name: "e".to_string(),
            url: url.clone(),
            model: MODEL.to_string(),
            kind: "tensorfold".to_string(),
            host: "h".to_string(),
            shared,
            expected_features: expected,
            ..EngineConfig::default()
        };

        let mut client = Client::new(&url, opts.timeout);
        let mut res = CheckResult::for_engine(&cfg);
        check_tensorfold(&mut res, &cfg, &mut client, opts);
        res.requests = client.count;
        (res, client.count)
    }

    fn feature<'a>(res: &'a CheckResult, name: &str) -> &'a Feature {
        res.features
            .get(name)
            .unwrap_or_else(|| panic!("feature {name} was recorded"))
    }

    #[test]
    fn ok_ids_in_health_quick() {
        let sc = scenario().health(200, healthy()).chat(|_| ok_chat());
        let (res, requests) = run(sc, &opts(true));

        assert_eq!(res.status, Status::Ok);
        assert_eq!(res.verdict(), "ok");
        assert_eq!(res.model, MODEL);
        assert_eq!(res.version, "1.2.3");
        assert_eq!(res.load, "idle");
        assert_eq!(res.sanity.result, "pass");
        assert_eq!(res.sanity.detail, "answered '42'");
        assert!(res.sanity.latency_s.is_some());
        assert!(res.features.is_empty(), "quick mode records no features");
        assert_eq!(res.problems.len(), 0);
        assert_eq!(res.notes.len(), 0);
        assert_eq!(requests, 2, "health + sanity");
    }

    #[test]
    fn ok_status_shape_falls_back_to_models() {
        let sc = scenario()
            .health(200, json!({"status": "ok"}))
            .models(200, json!({"data": [{"id": MODEL}]}))
            .chat(|_| Reply::json(200, chat("The answer is 42.")));
        let (res, requests) = run(sc, &opts(true));

        assert_eq!(res.status, Status::Wrong);
        assert_eq!(res.verdict(), "wrong");
        assert_eq!(res.model, MODEL);
        assert_eq!(res.version, "");
        assert_eq!(res.load, "");
        assert_eq!(res.sanity.result, "fail");
        assert_eq!(
            res.sanity.detail,
            "answered 'The answer is 42.', expected '42'"
        );
        assert_eq!(res.problems.len(), 1);
        assert_eq!(requests, 3, "health + models + sanity");
    }

    #[test]
    fn load_shapes_quick() {
        let busy_sc = scenario().health(200, busy()).chat(|_| ok_chat());
        let (res, requests) = run(busy_sc, &opts(true));
        assert_eq!(res.status, Status::Ok);
        assert_eq!(res.load, "busy (2 running, 1 waiting)");
        assert_eq!(requests, 2);

        let live = json!({
            "ok": true,
            "model_ids": [MODEL],
            "live": {"connections": 3, "waiting": 1},
        });
        let live_sc = scenario().health(200, live).chat(|_| ok_chat());
        let (res, requests) = run(live_sc, &opts(true));
        assert_eq!(res.load, "busy (3 running, 1 waiting)");
        assert_eq!(requests, 2);
    }

    #[test]
    fn wrong_model_stops_before_sanity() {
        let sc = scenario()
            .health(200, healthy_for("Other"))
            .chat(|_| ok_chat());
        let (res, requests) = run(sc, &opts(true));

        assert_eq!(res.status, Status::Wrong);
        assert_eq!(res.verdict(), "wrong");
        assert_eq!(res.model, "Other");
        assert_eq!(res.sanity.result, "n/a");
        assert_eq!(res.problems, vec!["model is Other, registry expects M-1"]);
        assert_eq!(requests, 1, "health only");
    }

    #[test]
    fn health_failures_quick() {
        let not_ok = scenario()
            .health(200, json!({"ok": false}))
            .chat(|_| ok_chat());
        let (res, requests) = run(not_ok, &opts(true));
        assert_eq!(res.status, Status::Down);
        assert_eq!(res.verdict(), "down");
        assert_eq!(res.problems.len(), 1);
        assert!(res.problems[0].starts_with("health says not ok:"));
        assert_eq!(res.model, "");
        assert_eq!(res.load, "");
        assert_eq!(requests, 1);

        let warming = scenario()
            .health(
                200,
                json!({"ok": true, "warming": true, "model_ids": [MODEL]}),
            )
            .chat(|_| ok_chat());
        let (res, requests) = run(warming, &opts(true));
        assert_eq!(res.status, Status::Down);
        assert_eq!(
            res.problems,
            vec!["still warming up (health.warming is true)"]
        );
        assert_eq!(requests, 1);

        let health_500 = scenario()
            .health(500, json!({"error": "boom"}))
            .chat(|_| ok_chat());
        let (res, requests) = run(health_500, &opts(true));
        assert_eq!(res.status, Status::Down);
        assert_eq!(res.problems, vec!["health: HTTP 500: boom"]);
        assert_eq!(requests, 1);

        let models_404 = scenario()
            .health(200, json!({"ok": true}))
            .models(404, json!({"error": "no"}))
            .chat(|_| ok_chat());
        let (res, requests) = run(models_404, &opts(true));
        assert_eq!(res.status, Status::Down);
        assert_eq!(res.problems, vec!["model list: HTTP 404: no"]);
        assert_eq!(requests, 2);

        let models_no_ids = scenario()
            .health(200, json!({"ok": true}))
            .models(200, json!({"x": 1}))
            .chat(|_| ok_chat());
        let (res, requests) = run(models_no_ids, &opts(true));
        assert_eq!(res.status, Status::Wrong);
        assert_eq!(res.problems, vec!["model list has no data[].id"]);
        assert_eq!(res.model, "");
        assert_eq!(requests, 2);
    }

    #[test]
    fn sanity_failures_quick() {
        let wrong = scenario()
            .health(200, healthy())
            .chat(|_| Reply::json(200, chat("41")));
        let (res, requests) = run(wrong, &opts(true));
        assert_eq!(res.status, Status::Wrong);
        assert_eq!(res.sanity.result, "fail");
        assert_eq!(res.sanity.detail, "answered '41', expected '42'");
        assert_eq!(res.problems.len(), 1);
        assert_eq!(requests, 2);

        let null = scenario()
            .health(200, healthy())
            .chat(|_| Reply::json(200, json!({"choices": [{"message": {"content": null}}]})));
        let (res, requests) = run(null, &opts(true));
        assert_eq!(res.status, Status::Wrong);
        assert_eq!(res.sanity.detail, "answered null, expected '42'");
        assert_eq!(requests, 2);

        let err_500 = scenario()
            .health(200, healthy())
            .chat(|_| Reply::json(500, json!({"error": "x"})));
        let (res, requests) = run(err_500, &opts(true));
        assert_eq!(res.status, Status::Down);
        assert_eq!(res.sanity.result, "error");
        assert_eq!(res.sanity.detail, "HTTP 500: x");
        assert_eq!(res.problems, vec!["sanity request: HTTP 500: x"]);
        assert_eq!(requests, 2);

        let err_404 = scenario()
            .health(200, healthy())
            .chat(|_| Reply::json(404, json!({"error": "x"})));
        let (res, requests) = run(err_404, &opts(true));
        assert_eq!(res.status, Status::Wrong);
        assert_eq!(res.sanity.result, "error");
        assert_eq!(res.sanity.detail, "HTTP 404: x");
        assert_eq!(res.problems, vec!["sanity request: HTTP 404: x"]);
        assert_eq!(requests, 2);
    }

    #[test]
    fn shared_busy_skips_in_quick_and_full() {
        let quick_sc = scenario()
            .health(200, busy())
            .chat(|_| ok_chat())
            .shared(true);
        let (res, requests) = run(quick_sc, &opts(true));
        assert_eq!(res.status, Status::SkippedBusy);
        assert_eq!(res.verdict(), "ok");
        assert_eq!(res.sanity.result, "skipped");
        assert_eq!(res.sanity.detail, "shared engine is busy");
        assert_eq!(res.sanity.latency_s, None);
        assert!(res.features.is_empty(), "quick mode leaves features empty");
        assert_eq!(res.notes.len(), 1);
        assert_eq!(res.problems.len(), 0);
        assert_eq!(requests, 1, "health only");

        let full_sc = scenario()
            .health(200, busy())
            .chat(|_| ok_chat())
            .shared(true);
        let (res, requests) = run(full_sc, &opts(false));
        assert_eq!(res.status, Status::SkippedBusy);
        assert_eq!(res.features.len(), 4);
        for name in FEATURES {
            let f = feature(&res, name);
            assert_eq!(f.result, "skipped");
            assert_eq!(f.reason, "shared engine busy");
            assert_eq!(f.expected, None);
            assert!(!f.drift);
        }
        assert_eq!(res.notes.len(), 1);
        assert_eq!(requests, 1, "no probes while busy");
    }

    #[test]
    fn shared_idle_full_runs_every_probe() {
        let sc = scenario()
            .health(200, healthy())
            .chat(ideal_chat)
            .shared(true);
        let (res, requests) = run(sc, &opts(false));

        assert_eq!(res.status, Status::Ok);
        assert_eq!(res.verdict(), "ok");
        assert_eq!(res.sanity.result, "pass");
        assert_eq!(res.sanity.detail, "answered '42'");
        assert_eq!(feature(&res, "background_priority").result, "yes");
        assert_eq!(
            feature(&res, "background_priority").reason,
            "accepted (HTTP 200)"
        );
        assert_eq!(feature(&res, "guided_choice").result, "yes");
        assert_eq!(feature(&res, "guided_choice").reason, "answered 'apple'");
        assert_eq!(feature(&res, "logprobs").result, "yes");
        assert_eq!(feature(&res, "logprobs").reason, "logprobs returned");
        assert_eq!(feature(&res, "prompt_logprobs").result, "no");
        assert_eq!(
            feature(&res, "prompt_logprobs").reason,
            "HTTP 404: no route"
        );
        assert_eq!(res.features.len(), 4);
        assert_eq!(res.problems.len(), 0);
        assert_eq!(res.notes.len(), 0);
        assert_eq!(requests, 6, "health + sanity + bg + 3 probes");
    }

    #[test]
    fn bg_accepted_full_keeps_background_priority_on_probes() {
        let sc = scenario()
            .health(200, healthy())
            .chat(|_| ok_chat())
            .expected(&[("background_priority", "yes")]);
        let (res, requests) = run(sc, &opts(false));

        assert_eq!(res.status, Status::Ok);
        assert_eq!(
            feature(&res, "background_priority").reason,
            "accepted with the sanity request (HTTP 200)"
        );
        assert_eq!(feature(&res, "guided_choice").result, "no");
        assert_eq!(
            feature(&res, "guided_choice").reason,
            "HTTP 200 but answered '42', not one of the choices (ignored)"
        );
        assert_eq!(
            feature(&res, "logprobs").reason,
            "HTTP 200 but no logprobs in the answer (silently ignored)"
        );
        assert_eq!(feature(&res, "prompt_logprobs").result, "no");
        assert_eq!(res.notes.len(), 0, "no starvation and no probe timeout");
        assert_eq!(
            requests, 5,
            "sanity answered the bg probe; no separate bg probe"
        );
    }

    #[test]
    fn bg_refused_full_resends_plain_and_reports_drift() {
        let sc = scenario()
            .health(200, healthy())
            .chat(refuse_bg)
            .expected(&[("background_priority", "yes")]);
        let (res, requests) = run(sc, &opts(false));

        assert_eq!(res.status, Status::Ok);
        assert_eq!(res.verdict(), "drift");
        assert_eq!(res.sanity.result, "pass", "sanity was re-sent plain");
        let bg = feature(&res, "background_priority");
        assert_eq!(bg.result, "no");
        assert_eq!(bg.reason, "HTTP 400: priority not supported");
        assert_eq!(bg.expected.as_deref(), Some("yes"));
        assert!(bg.drift);
        assert_eq!(feature(&res, "guided_choice").result, "yes");
        assert_eq!(feature(&res, "logprobs").result, "yes");
        assert_eq!(feature(&res, "prompt_logprobs").result, "no");
        assert_eq!(res.problems.len(), 0);
        assert_eq!(res.notes.len(), 0);
        assert_eq!(requests, 6, "health + bg sanity + plain sanity + 3 probes");
        assert_eq!(exit_code(std::slice::from_ref(&res)), 2);
    }

    #[test]
    fn guided_outcomes_full() {
        let yes = scenario().health(200, healthy()).chat(|body| {
            if body.get("guided_choice").is_some() {
                Reply::json(200, chat("apple"))
            } else {
                ideal_chat(body)
            }
        });
        let (res, requests) = run(yes, &opts(false));
        assert_eq!(feature(&res, "guided_choice").result, "yes");
        assert_eq!(feature(&res, "guided_choice").reason, "answered 'apple'");
        assert_eq!(requests, 6);

        let refused = scenario().health(200, healthy()).chat(|body| {
            if body.get("guided_choice").is_some() {
                Reply::json(400, json!({"error": "guided_choice not supported"}))
            } else {
                ideal_chat(body)
            }
        });
        let (res, requests) = run(refused, &opts(false));
        assert_eq!(feature(&res, "guided_choice").result, "no");
        assert_eq!(
            feature(&res, "guided_choice").reason,
            "HTTP 400: guided_choice not supported"
        );
        assert_eq!(requests, 6);

        let null = scenario().health(200, healthy()).chat(|body| {
            if body.get("guided_choice").is_some() {
                Reply::json(200, json!({"choices": [{"message": {"content": null}}]}))
            } else {
                ideal_chat(body)
            }
        });
        let (res, requests) = run(null, &opts(false));
        assert_eq!(feature(&res, "guided_choice").result, "no");
        assert_eq!(
            feature(&res, "guided_choice").reason,
            "HTTP 200 but content was null (guidance ignored, or token budget spent thinking)"
        );
        assert_eq!(requests, 6);

        let offlist = scenario().health(200, healthy()).chat(|body| {
            if body.get("guided_choice").is_some() {
                Reply::json(200, chat("granite and apple"))
            } else {
                ideal_chat(body)
            }
        });
        let (res, requests) = run(offlist, &opts(false));
        assert_eq!(feature(&res, "guided_choice").result, "no");
        assert_eq!(
            feature(&res, "guided_choice").reason,
            "HTTP 200 but answered 'granite and apple', not one of the choices (ignored)"
        );
        assert_eq!(requests, 6);
    }

    #[test]
    fn logprobs_present_and_absent_full() {
        let present = scenario().health(200, healthy()).chat(ideal_chat);
        let (res, requests) = run(present, &opts(false));
        assert_eq!(feature(&res, "logprobs").result, "yes");
        assert_eq!(feature(&res, "logprobs").reason, "logprobs returned");
        assert_eq!(requests, 6);

        let absent = scenario().health(200, healthy()).chat(|body| {
            if probes::truthy(body.get("logprobs").unwrap_or(&Value::Null)) {
                Reply::json(200, chat("ok"))
            } else {
                ideal_chat(body)
            }
        });
        let (res, requests) = run(absent, &opts(false));
        assert_eq!(feature(&res, "logprobs").result, "no");
        assert_eq!(
            feature(&res, "logprobs").reason,
            "HTTP 200 but no logprobs in the answer (silently ignored)"
        );
        assert_eq!(requests, 6);
    }

    #[test]
    fn prompt_logprobs_present_and_absent_full() {
        let present = scenario()
            .health(200, healthy())
            .chat(ideal_chat)
            .completions(ideal_plp);
        let (res, requests) = run(present, &opts(false));
        assert_eq!(feature(&res, "prompt_logprobs").result, "yes");
        assert_eq!(
            feature(&res, "prompt_logprobs").reason,
            "prompt_logprobs returned"
        );
        assert_eq!(requests, 6);

        let absent = scenario()
            .health(200, healthy())
            .chat(ideal_chat)
            .completions(|_| Reply::json(200, json!({"choices": [], "id": "cmpl"})));
        let (res, requests) = run(absent, &opts(false));
        assert_eq!(feature(&res, "prompt_logprobs").result, "no");
        assert_eq!(
            feature(&res, "prompt_logprobs").reason,
            "HTTP 200 but no prompt_logprobs field (silently ignored)"
        );
        assert_eq!(requests, 6);
    }

    #[test]
    fn bg_refused_sanity_yes_and_no() {
        let yes = scenario()
            .health(200, healthy())
            .chat(refuse_bg)
            .completions(ideal_plp)
            .expected(&[("background_priority", "yes")]);
        let (res, requests) = run(yes, &opts(false));
        assert_eq!(res.sanity.result, "pass");
        let bg = feature(&res, "background_priority");
        assert_eq!(bg.result, "no");
        assert_eq!(bg.reason, "HTTP 400: priority not supported");
        assert!(bg.drift);
        assert_eq!(feature(&res, "guided_choice").result, "yes");
        assert_eq!(feature(&res, "logprobs").result, "yes");
        assert_eq!(feature(&res, "prompt_logprobs").result, "yes");
        assert_eq!(res.verdict(), "drift");
        assert_eq!(requests, 6);

        let no = scenario()
            .health(200, healthy())
            .chat(refuse_bg)
            .completions(ideal_plp)
            .expected(&[("background_priority", "no")]);
        let (res, requests) = run(no, &opts(false));
        assert_eq!(res.sanity.result, "pass", "no background priority was sent");
        let bg = feature(&res, "background_priority");
        assert_eq!(bg.result, "no");
        assert!(!bg.drift, "matches the registry expectation");
        assert_eq!(feature(&res, "guided_choice").result, "yes");
        assert_eq!(feature(&res, "logprobs").result, "yes");
        assert_eq!(feature(&res, "prompt_logprobs").result, "yes");
        assert_eq!(res.verdict(), "ok");
        assert_eq!(requests, 6);
    }

    #[test]
    fn logprobs_drift_sets_verdict_drift() {
        let sc = scenario()
            .health(200, healthy())
            .chat(|body| {
                if probes::truthy(body.get("logprobs").unwrap_or(&Value::Null)) {
                    Reply::json(200, chat("ok"))
                } else {
                    ideal_chat(body)
                }
            })
            .completions(ideal_plp)
            .expected(&[("logprobs", "yes")]);
        let (res, requests) = run(sc, &opts(false));

        assert_eq!(res.status, Status::Ok);
        assert_eq!(res.verdict(), "drift");
        let lp = feature(&res, "logprobs");
        assert_eq!(lp.result, "no");
        assert_eq!(lp.expected.as_deref(), Some("yes"));
        assert!(lp.drift);
        assert_eq!(res.problems.len(), 0);
        assert_eq!(requests, 6);
        assert_eq!(exit_code(std::slice::from_ref(&res)), 2);
    }

    #[test]
    fn down_plus_drift_exits_one() {
        let down_sc = scenario()
            .health(200, json!({"ok": false}))
            .chat(|_| ok_chat());
        let (down, _) = run(down_sc, &opts(false));

        let drift_sc = scenario()
            .health(200, healthy())
            .chat(|body| {
                if probes::truthy(body.get("logprobs").unwrap_or(&Value::Null)) {
                    Reply::json(200, chat("ok"))
                } else {
                    ideal_chat(body)
                }
            })
            .completions(ideal_plp)
            .expected(&[("logprobs", "yes")]);
        let (drift, _) = run(drift_sc, &opts(false));

        assert_eq!(down.verdict(), "down");
        assert_eq!(drift.verdict(), "drift");
        assert_eq!(exit_code(&[down, drift]), 1);
    }

    #[test]
    fn bg_sanity_starvation_while_busy_resends_plain() {
        let sc = scenario()
            .health(200, busy())
            .chat(|body| {
                if body.get("priority").is_some() {
                    sleep(1.0);
                    ok_chat()
                } else {
                    ideal_chat(body)
                }
            })
            .expected(&[("background_priority", "yes")]);
        let mut o = opts(false);
        o.bg_timeout = 0.3;
        let (res, requests) = run(sc, &o);

        assert_eq!(res.status, Status::Ok);
        assert_eq!(res.sanity.result, "pass", "the plain resend answered");
        assert_eq!(res.load, "busy (2 running, 1 waiting)");
        let bg = feature(&res, "background_priority");
        assert_eq!(bg.result, "yes");
        assert_eq!(
            bg.reason,
            "accepted, but starved 0.3s behind other work; sanity re-sent at normal priority"
        );
        assert_eq!(bg.expected.as_deref(), Some("yes"));
        assert!(!bg.drift);
        assert_eq!(res.notes.len(), 1);
        assert!(res.notes[0].contains("re-sent once at normal priority"));
        assert_eq!(feature(&res, "guided_choice").result, "yes");
        assert_eq!(feature(&res, "logprobs").result, "yes");
        assert_eq!(feature(&res, "prompt_logprobs").result, "no");
        assert_eq!(requests, 6, "health + bg sanity + plain sanity + 3 probes");
        assert_eq!(exit_code(std::slice::from_ref(&res)), 0);
    }

    #[test]
    fn bg_probe_timeout_resends_plain_and_disables_background() {
        let sc = scenario()
            .health(200, healthy())
            .chat(|body| {
                let guided = body.get("guided_choice").is_some();
                let bg = body.get("priority").is_some();
                if guided && bg {
                    sleep(1.0);
                    Reply::json(200, chat("apple"))
                } else {
                    ideal_chat(body)
                }
            })
            .completions(ideal_plp)
            .expected(&[("background_priority", "yes")]);
        let mut o = opts(false);
        o.bg_timeout = 0.3;
        let (res, requests) = run(sc, &o);

        assert_eq!(res.status, Status::Ok);
        assert_eq!(res.verdict(), "ok");
        assert_eq!(feature(&res, "background_priority").result, "yes");
        assert_eq!(feature(&res, "guided_choice").result, "yes");
        assert_eq!(feature(&res, "guided_choice").reason, "answered 'apple'");
        assert_eq!(feature(&res, "logprobs").result, "yes");
        assert_eq!(feature(&res, "prompt_logprobs").result, "yes");
        assert_eq!(res.notes.len(), 1);
        assert!(res.notes[0].contains("the remaining probes were re-sent"));
        assert_eq!(
            requests, 6,
            "health + sanity + timed-out guided + plain guided + logprobs + prompt_logprobs"
        );
    }

    #[test]
    fn sanity_timeout_while_idle_is_down() {
        let sc = scenario().health(200, healthy()).chat(|_| {
            sleep(1.0);
            ok_chat()
        });
        let mut o = opts(true);
        o.timeout = 0.3;
        let (res, requests) = run(sc, &o);

        assert_eq!(res.status, Status::Down);
        assert_eq!(res.verdict(), "down");
        assert_eq!(res.sanity.result, "error");
        assert_eq!(res.sanity.detail, "URLError: timed out");
        assert!(res.sanity.latency_s.is_some());
        assert_eq!(res.problems, vec!["sanity request: URLError: timed out"]);
        assert_eq!(requests, 2, "health + timed-out sanity");
        assert!(res.features.is_empty());
    }

    #[test]
    fn sanity_resend_timeout_while_busy_is_slow() {
        let sc = scenario()
            .health(200, busy())
            .chat(|_| {
                sleep(1.0);
                ok_chat()
            })
            .expected(&[("background_priority", "yes")]);
        let mut o = opts(false);
        o.timeout = 0.3;
        o.bg_timeout = 0.3;
        let (res, requests) = run(sc, &o);

        assert_eq!(res.status, Status::Slow);
        assert_eq!(res.verdict(), "slow");
        assert_eq!(res.sanity.result, "error");
        assert_eq!(res.sanity.detail, "URLError: timed out");
        assert_eq!(res.problems.len(), 1);
        assert!(res.problems[0].starts_with("sanity request timed out after 0.3s"));
        assert!(res.problems[0].contains("busy (2 running, 1 waiting)"));
        assert_eq!(res.notes.len(), 1);
        assert!(res.notes[0].contains("re-sent once at normal priority"));
        assert_eq!(requests, 3, "health + bg sanity + plain resend");
        assert_eq!(exit_code(std::slice::from_ref(&res)), 1);
    }
}
