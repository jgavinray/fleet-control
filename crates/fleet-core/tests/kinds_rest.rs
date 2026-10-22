//! Scenario tests for the REST-only kinds (laya, lfm2d, jev-glm), mirroring
//! the mock-engine scenarios in `crates/parity.py`. Every test runs a real
//! `Client` against a `std::net::TcpListener` mock on loopback.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

use fleet_core::http::py_dumps;
use fleet_core::kinds_rest::rest_kind_check;
use fleet_core::opts::Opts;
use fleet_core::py_str;
use fleet_core::registry::EngineConfig;
use fleet_core::{CheckResult, Client, Status};

const MODEL: &str = "test-model";

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    body: String,
}

/// One mock engine on a loopback port. Routes are keyed by "METHOD PATH" and
/// carry the status plus the already-serialized body; anything else is 404.
struct Mock {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Mock {
    fn new(routes: Vec<(&str, u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock engine");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("mock engine address")
        );
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let routes: Vec<(String, u16, String)> = routes
            .into_iter()
            .map(|(key, status, body)| (key.to_string(), status, body))
            .collect();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let seen_thread = Arc::clone(&seen);
        let stop_thread = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        serve(&mut stream, &routes, &seen_thread);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base,
            seen,
            stop,
            handle: Some(handle),
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("mock request log").clone()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
    }
}

fn serve(stream: &mut TcpStream, routes: &[(String, u16, String)], seen: &Mutex<Vec<Seen>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone mock stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok();
    }
    let body = String::from_utf8_lossy(&body).to_string();
    seen.lock().expect("mock request log").push(Seen {
        method: method.clone(),
        path: path.clone(),
        body,
    });

    let key = format!("{method} {path}");
    let (status, payload) = routes
        .iter()
        .find(|(route, _, _)| *route == key)
        .map(|(_, status, body)| (*status, body.clone()))
        .unwrap_or((404, py_dumps(&json!({"error": "no route"}))));
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(response.as_bytes()).ok();
    stream.flush().ok();
}

/// Attach a route key to an already-serialized body.
fn route(key: &'static str, status: u16, body: Value) -> (&'static str, u16, String) {
    (key, status, py_dumps(&body))
}

fn text_route(key: &'static str, status: u16, text: &str) -> (&'static str, u16, String) {
    (key, status, text.to_string())
}

fn opts() -> Opts {
    Opts {
        quick: false,
        json: true,
        registry: None,
        timeout: 60.0,
        bg_timeout: 20.0,
        force_shared: false,
        engines: Vec::new(),
    }
}

fn engine(kind: &str, base: &str, model: &str, read: &str, exporter: Option<&str>) -> EngineConfig {
    EngineConfig {
        name: format!("{kind}-engine"),
        url: base.to_string(),
        model: model.to_string(),
        kind: kind.to_string(),
        host: "mock".to_string(),
        health_path: None,
        exporter_url: exporter.map(str::to_string),
        read: Some(read.to_string()),
        shared: false,
        expected_features: BTreeMap::new(),
    }
}

/// Run one engine check the way `check_engine` does, including the request
/// count copy.
fn run(cfg: &EngineConfig) -> (CheckResult, u32) {
    let opts = opts();
    let mut res = CheckResult::for_engine(cfg);
    let mut client = Client::new(cfg.base_url(), opts.timeout);
    let check = rest_kind_check(&cfg.kind).expect("kind has a REST check");
    check(&mut res, cfg, &mut client, &opts);
    res.requests = client.count;
    (res, client.count)
}

fn paths(seen: &[Seen]) -> Vec<String> {
    seen.iter()
        .map(|seen| format!("{} {}", seen.method, seen.path))
        .collect()
}

fn body_of(seen: &[Seen], index: usize) -> Value {
    serde_json::from_str(&seen[index].body).expect("JSON request body")
}

fn laya_body() -> Value {
    json!({
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
    })
}

fn jev_body() -> Value {
    json!({
        "state": "ls -la",
        "model": MODEL,
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
    })
}

// --- laya ------------------------------------------------------------------

#[test]
fn laya_ok() {
    let mock = Mock::new(vec![
        route(
            "GET /health",
            200,
            json!({"ok": true, "loaded": ["en"], "device": "cuda:0"}),
        ),
        route(
            "POST /v1/systemone",
            200,
            json!({
                "answers": {"intent": {"choice": "billing", "probabilities": {"billing": 0.87}}},
                "model": MODEL,
            }),
        ),
    ]);
    let cfg = engine("laya", &mock.base, MODEL, "specs", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Ok);
    assert_eq!(res.sanity.result, "pass");
    assert_eq!(res.sanity.detail, "chose 'billing' (p=0.87)");
    assert_eq!(res.model, MODEL);
    assert!(res.problems.is_empty(), "{:?}", res.problems);
    assert_eq!(res.notes, vec!["loaded en on cuda:0"]);
    assert_eq!(count, 2);
    assert_eq!(res.requests, 2);
    assert_eq!(
        paths(&mock.seen()),
        vec!["GET /health", "POST /v1/systemone"]
    );
    assert_eq!(body_of(&mock.seen(), 1), laya_body());
}

#[test]
fn laya_wrong_choice() {
    let mock = Mock::new(vec![
        route(
            "GET /health",
            200,
            json!({"ok": true, "loaded": [], "device": "cpu"}),
        ),
        route(
            "POST /v1/systemone",
            200,
            json!({
                "answers": {"intent": {"choice": "shipping"}},
                "model": MODEL,
            }),
        ),
    ]);
    let cfg = engine("laya", &mock.base, MODEL, "specs", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Wrong);
    assert_eq!(res.sanity.result, "fail");
    assert_eq!(res.sanity.detail, "chose 'shipping', expected 'billing'");
    assert_eq!(
        res.problems,
        vec!["sanity: chose 'shipping', expected 'billing'"]
    );
    assert_eq!(res.notes, vec!["loaded nothing on cpu"]);
    assert_eq!(count, 2);
    assert_eq!(res.model, MODEL);
}

#[test]
fn laya_model_mismatch() {
    let mock = Mock::new(vec![
        route(
            "GET /health",
            200,
            json!({"ok": true, "loaded": ["en"], "device": "cuda:0"}),
        ),
        route(
            "POST /v1/systemone",
            200,
            json!({
                "answers": {"intent": {"choice": "billing"}},
                "model": "Other",
            }),
        ),
    ]);
    let cfg = engine("laya", &mock.base, MODEL, "specs", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Wrong);
    assert_eq!(res.sanity.result, "pass");
    assert_eq!(res.sanity.detail, "chose 'billing'", "no probability given");
    assert_eq!(res.model, "Other");
    assert_eq!(
        res.problems,
        vec![format!("model is Other, registry expects {MODEL}")]
    );
    assert_eq!(count, 2);
}

#[test]
fn laya_health_bad() {
    let mock = Mock::new(vec![route("GET /health", 200, json!({"ok": false}))]);
    let cfg = engine("laya", &mock.base, MODEL, "specs", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Down);
    assert_eq!(res.sanity.result, "n/a");
    assert_eq!(res.sanity.latency_s, None);
    assert_eq!(res.problems, vec!["health says not ok: {\"ok\": false}"]);
    assert!(res.notes.is_empty());
    assert_eq!(count, 1);
}

// --- lfm2d -----------------------------------------------------------------

#[test]
fn lfm2d_specs_ok() {
    let mock = Mock::new(vec![
        route("GET /v1/models", 200, json!([{"id": MODEL}])),
        route(
            "GET /v1/opinion/specs",
            200,
            json!([{"id": "s1", "name": "bash-risk"}]),
        ),
    ]);
    let cfg = engine("lfm2d", &mock.base, MODEL, "specs", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Ok);
    assert_eq!(res.model, MODEL);
    assert_eq!(res.sanity.result, "pass");
    assert_eq!(res.sanity.detail, "1 spec(s) listed");
    assert!(res.problems.is_empty(), "{:?}", res.problems);
    assert!(res.notes.is_empty());
    assert_eq!(count, 2);
    assert_eq!(
        paths(&mock.seen()),
        vec!["GET /v1/models", "GET /v1/opinion/specs"]
    );
}

#[test]
fn lfm2d_wrong_model() {
    let mock = Mock::new(vec![route("GET /v1/models", 200, json!([{"id": "Other"}]))]);
    let cfg = engine("lfm2d", &mock.base, MODEL, "specs", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Wrong);
    assert_eq!(res.model, "Other");
    assert_eq!(res.sanity.result, "n/a");
    assert_eq!(
        res.problems,
        vec![format!("models are Other, registry expects {MODEL}")]
    );
    assert_eq!(count, 1, "no read request after a bad model list");
}

#[test]
fn lfm2d_specs_empty() {
    let mock = Mock::new(vec![
        route("GET /v1/models", 200, json!([{"id": MODEL}])),
        route("GET /v1/opinion/specs", 200, json!([])),
    ]);
    let cfg = engine("lfm2d", &mock.base, MODEL, "specs", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Wrong);
    assert_eq!(res.model, MODEL);
    assert_eq!(res.sanity.result, "fail");
    assert_eq!(res.sanity.detail, "expected at least one spec");
    assert_eq!(res.problems, vec!["read: expected at least one spec"]);
    assert_eq!(count, 2);
}

#[test]
fn lfm2d_spans_ok() {
    let mock = Mock::new(vec![
        route("GET /v1/models", 200, json!([{"id": MODEL}])),
        route(
            "POST /v1/spans/credentials",
            200,
            json!([[{"text": "sk-1", "start": 0, "end": 5}]]),
        ),
    ]);
    let cfg = engine("lfm2d", &mock.base, MODEL, "spans", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Ok);
    assert_eq!(res.model, MODEL);
    assert_eq!(res.sanity.result, "pass");
    assert_eq!(res.sanity.detail, "span scan answered");
    assert!(res.problems.is_empty(), "{:?}", res.problems);
    assert_eq!(count, 2);
    assert_eq!(
        paths(&mock.seen()),
        vec!["GET /v1/models", "POST /v1/spans/credentials"]
    );
    assert_eq!(
        body_of(&mock.seen(), 1),
        json!({"inputs": ["fleet-check ping"]})
    );
}

#[test]
fn lfm2d_exporter_ok() {
    let mock = Mock::new(vec![
        route("GET /v1/models", 200, json!([{"id": MODEL}])),
        route("GET /v1/opinion/specs", 200, json!([{"id": "s1"}])),
        text_route("GET /healthz", 200, "ok"),
    ]);
    let base = mock.base.clone();
    let cfg = engine("lfm2d", &base, MODEL, "specs", Some(&base));
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Ok);
    assert_eq!(res.sanity.result, "pass");
    assert_eq!(res.notes, vec!["exporter /healthz ok"]);
    assert!(res.problems.is_empty(), "{:?}", res.problems);
    assert_eq!(count, 3);
    assert_eq!(
        paths(&mock.seen()),
        vec!["GET /v1/models", "GET /v1/opinion/specs", "GET /healthz"]
    );
}

#[test]
fn lfm2d_exporter_bad() {
    let mock = Mock::new(vec![
        route("GET /v1/models", 200, json!([{"id": MODEL}])),
        route("GET /v1/opinion/specs", 200, json!([{"id": "s1"}])),
        text_route("GET /healthz", 200, "starting"),
    ]);
    let base = mock.base.clone();
    let cfg = engine("lfm2d", &base, MODEL, "specs", Some(&base));
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Wrong);
    assert_eq!(res.sanity.result, "pass");
    assert_eq!(
        res.problems,
        vec![format!("exporter {base}: HTTP 200: starting")]
    );
    assert!(res.notes.is_empty());
    assert_eq!(count, 3);
}

// --- jev-glm ---------------------------------------------------------------

#[test]
fn jev_ok() {
    let mock = Mock::new(vec![
        route("GET /health", 200, json!({"ok": true, "model": MODEL})),
        route("GET /ready", 200, json!({"status": "ok"})),
        route(
            "POST /v1/systemone",
            200,
            json!({
                "answers": {"verdict": {"choice": "read", "probabilities": {"read": 0.9}}},
                "model": MODEL,
            }),
        ),
    ]);
    let cfg = engine("jev-glm", &mock.base, MODEL, "specs", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Ok);
    assert_eq!(res.sanity.result, "pass");
    assert_eq!(res.sanity.detail, "chose 'read' (p=0.90)");
    assert_eq!(res.model, MODEL);
    assert!(res.problems.is_empty(), "{:?}", res.problems);
    assert!(res.notes.is_empty());
    assert_eq!(count, 3);
    assert_eq!(
        paths(&mock.seen()),
        vec!["GET /health", "GET /ready", "POST /v1/systemone"]
    );
    assert_eq!(body_of(&mock.seen(), 2), jev_body());
}

#[test]
fn jev_ready_fail() {
    let mock = Mock::new(vec![
        route("GET /health", 200, json!({"ok": true, "model": MODEL})),
        route("GET /ready", 500, json!({"error": "glm is down"})),
    ]);
    let cfg = engine("jev-glm", &mock.base, MODEL, "specs", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Wrong, "not down: the engine answered");
    assert_eq!(
        res.problems,
        vec!["/ready: the GLM behind it is not answering (HTTP 500: glm is down)"]
    );
    assert_eq!(res.model, MODEL, "model comes from the health body");
    assert_eq!(res.sanity.result, "n/a");
    assert_eq!(count, 2, "no systemone request after a failed /ready");
}

#[test]
fn jev_wrong_choice() {
    let mock = Mock::new(vec![
        route("GET /health", 200, json!({"ok": true, "model": MODEL})),
        route("GET /ready", 200, json!({"status": "ok"})),
        route(
            "POST /v1/systemone",
            200,
            json!({
                "answers": {"verdict": {"choice": "destroy"}},
                "model": MODEL,
            }),
        ),
    ]);
    let cfg = engine("jev-glm", &mock.base, MODEL, "specs", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Wrong);
    assert_eq!(res.sanity.result, "fail");
    assert_eq!(res.sanity.detail, "chose 'destroy', expected 'read'");
    assert_eq!(
        res.problems,
        vec!["sanity: chose 'destroy', expected 'read'"]
    );
    assert_eq!(count, 3);
}

#[test]
fn jev_model_mismatch() {
    let mock = Mock::new(vec![
        route("GET /health", 200, json!({"ok": true, "model": MODEL})),
        route("GET /ready", 200, json!({"status": "ok"})),
        route(
            "POST /v1/systemone",
            200,
            json!({
                "answers": {"verdict": {"choice": "read"}},
                "model": "Other",
            }),
        ),
    ]);
    let cfg = engine("jev-glm", &mock.base, MODEL, "specs", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Wrong);
    assert_eq!(res.sanity.result, "pass");
    assert_eq!(res.sanity.detail, "chose 'read'");
    assert_eq!(res.model, "Other");
    assert_eq!(
        res.problems,
        vec![format!("model is Other, registry expects {MODEL}")]
    );
    assert_eq!(count, 3);
}

// --- dispatch --------------------------------------------------------------

#[test]
fn rest_kind_dispatch_is_limited_to_rest_kinds() {
    assert!(rest_kind_check("laya").is_some());
    assert!(rest_kind_check("lfm2d").is_some());
    assert!(rest_kind_check("jev-glm").is_some());
    assert!(rest_kind_check("tensorfold").is_none());
    assert!(rest_kind_check("mystery").is_none());
}

#[test]
fn laya_health_path_empty_requests_the_base_url() {
    let mock = Mock::new(vec![
        route(
            "GET /",
            200,
            json!({"ok": true, "loaded": [], "device": "cpu"}),
        ),
        route(
            "POST /v1/systemone",
            200,
            json!({
                "answers": {"intent": {"choice": "billing"}},
                "model": MODEL,
            }),
        ),
    ]);
    let mut cfg = engine("laya", &mock.base, MODEL, "specs", None);
    cfg.health_path = Some(String::new());
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Ok);
    assert_eq!(res.sanity.result, "pass");
    assert_eq!(
        paths(&mock.seen()),
        vec!["GET /", "POST /v1/systemone"],
        "a present-but-empty health_path requests the base URL"
    );
    assert_eq!(count, 2);
}

#[test]
fn lfm2d_read_empty_takes_the_spans_path() {
    let mock = Mock::new(vec![
        route("GET /v1/models", 200, json!([{"id": MODEL}])),
        route(
            "POST /v1/spans/credentials",
            200,
            json!([[{"text": "sk-1"}]]),
        ),
    ]);
    let cfg = engine("lfm2d", &mock.base, MODEL, "", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Ok);
    assert_eq!(res.sanity.result, "pass");
    assert_eq!(res.sanity.detail, "span scan answered");
    assert_eq!(
        paths(&mock.seen()),
        vec!["GET /v1/models", "POST /v1/spans/credentials"],
        "a present-but-empty read is not the specs default"
    );
    assert_eq!(count, 2);
}

#[test]
fn laya_reply_model_number_is_rendered_and_mismatches() {
    let mock = Mock::new(vec![
        route(
            "GET /health",
            200,
            json!({"ok": true, "loaded": [], "device": "cpu"}),
        ),
        route(
            "POST /v1/systemone",
            200,
            json!({
                "answers": {"intent": {"choice": "billing"}},
                "model": 5,
            }),
        ),
    ]);
    let cfg = engine("laya", &mock.base, MODEL, "specs", None);
    let (res, count) = run(&cfg);

    assert_eq!(res.status, Status::Wrong);
    assert_eq!(res.model, "5", "a non-string model renders via str()");
    assert_eq!(res.sanity.result, "pass");
    assert_eq!(
        res.problems,
        vec![format!("model is 5, registry expects {MODEL}")]
    );
    assert_eq!(count, 2);
}

#[test]
fn laya_reply_model_empty_or_null_skips_the_mismatch() {
    for model in [json!(""), json!(null)] {
        let mock = Mock::new(vec![
            route(
                "GET /health",
                200,
                json!({"ok": true, "loaded": [], "device": "cpu"}),
            ),
            route(
                "POST /v1/systemone",
                200,
                json!({
                    "answers": {"intent": {"choice": "billing"}},
                    "model": model,
                }),
            ),
        ]);
        let cfg = engine("laya", &mock.base, MODEL, "specs", None);
        let (res, count) = run(&cfg);

        assert_eq!(res.status, Status::Ok, "{model}");
        assert!(res.problems.is_empty(), "{model}");
        assert_eq!(res.model, py_str(&model));
        assert_eq!(count, 2);
    }
}

#[test]
fn laya_choice_with_quotes_is_repr_escaped() {
    let mock = Mock::new(vec![
        route(
            "GET /health",
            200,
            json!({"ok": true, "loaded": [], "device": "cpu"}),
        ),
        route(
            "POST /v1/systemone",
            200,
            json!({
                "answers": {"intent": {"choice": "a'b\"c"}},
                "model": MODEL,
            }),
        ),
    ]);
    let cfg = engine("laya", &mock.base, MODEL, "specs", None);
    let (res, _) = run(&cfg);

    assert_eq!(res.status, Status::Wrong);
    assert_eq!(res.sanity.result, "fail");
    assert_eq!(
        res.sanity.detail, "chose 'a\\'b\"c', expected 'billing'",
        "Python repr escapes the quote it chose"
    );
    assert_eq!(
        res.problems,
        vec!["sanity: chose 'a\\'b\"c', expected 'billing'"]
    );
}
