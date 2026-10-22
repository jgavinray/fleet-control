//! Parity tests for the pieces `check/fleet_check.py` defines: registry
//! loading (including the real `fleet.toml`), the local LAN override, health
//! and load helpers, the sanity answer rule, the result record, and the
//! counting client.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use serde_json::json;

use fleet_core::{
    apply_local_address, describe_load, exit_code, health_up, load_local_addresses, parse_registry,
    sanity_answer_ok, CheckResult, Client, Feature, Resp, Sanity, Status,
};

fn addresses(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, address)| (name.to_string(), address.to_string()))
        .collect()
}

fn temp_file(name: &str, contents: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("fleet-core-{}-{name}", std::process::id()));
    std::fs::write(&path, contents).expect("write temp file");
    path
}

#[test]
fn real_registry_parses() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fleet.toml");
    let text = std::fs::read_to_string(&path).expect("read the repo fleet.toml");
    let registry = parse_registry(&text).expect("parse the repo fleet.toml");

    assert_eq!(registry.engines.len(), 7, "engines in fleet.toml");
    assert_eq!(registry.hosts.len(), 10, "hosts in fleet.toml");
    assert!(registry.hosts.contains("hyper01"));
    assert!(registry.hosts.contains("spark"));

    let deepseek = registry.engine("deepseek").expect("deepseek engine");
    assert_eq!(deepseek.name, "deepseek");
    assert_eq!(deepseek.kind, "tensorfold");
    assert_eq!(deepseek.model, "DeepSeek-V4.1-Flash-1017");
    assert!(deepseek.url.ends_with(":8888"), "url {}", deepseek.url);
    assert_eq!(deepseek.host, "spark");
    assert!(!deepseek.shared);
    assert_eq!(deepseek.health_path(), "/health");
    assert_eq!(deepseek.exporter_url, None);
    assert_eq!(deepseek.expected("guided_choice"), Some("no"));
    assert_eq!(deepseek.expected("background_priority"), Some("yes"));
    assert_eq!(deepseek.expected("logprobs"), Some("no"));
    assert_eq!(deepseek.expected("prompt_logprobs"), Some("no"));

    let lfm2d = registry.engine("lfm2d-8b").expect("lfm2d-8b engine");
    assert_eq!(lfm2d.health_path(), "/v1/models");
    assert_eq!(
        lfm2d.exporter_url.as_deref(),
        Some("http://hyper01.puma-wyvern.ts.net:9170")
    );
    assert!(lfm2d.expected_features.is_empty());

    assert!(registry.engine("qwen").expect("qwen engine").shared);
    assert_eq!(
        registry.names(),
        registry
            .engines
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
}

#[test]
fn local_override_file_semantics() {
    let ok = temp_file(
        "local-ok.toml",
        "[hosts.spark]\naddress = \"192.0.2.10\"\n\n[hosts.spark2]\naddress = \"\"\n\n[hosts.hyper01]\nnotes = \"no address here\"\n",
    );
    let found = load_local_addresses(Some(&ok));
    assert_eq!(found.get("spark").map(String::as_str), Some("192.0.2.10"));
    assert!(!found.contains_key("spark2"), "empty address is ignored");
    assert!(!found.contains_key("hyper01"), "missing address is ignored");
    std::fs::remove_file(&ok).ok();

    let missing = std::env::temp_dir().join(format!(
        "fleet-core-{}-no-such-file.toml",
        std::process::id()
    ));
    assert!(load_local_addresses(Some(&missing)).is_empty());

    let bad = temp_file("local-bad.toml", "this is not [ valid toml");
    assert!(
        load_local_addresses(Some(&bad)).is_empty(),
        "invalid TOML is ignored"
    );
    std::fs::remove_file(&bad).ok();
}

#[test]
fn local_override_rewrites_bare_name_and_keeps_port() {
    let addrs = addresses(&[("spark", "192.0.2.10")]);
    assert_eq!(
        apply_local_address("http://spark:8888/v1/models", &addrs),
        "http://192.0.2.10:8888/v1/models"
    );
    assert_eq!(
        apply_local_address("http://spark", &addrs),
        "http://192.0.2.10"
    );
    assert_eq!(
        apply_local_address("http://spark:8888?x=1", &addrs),
        "http://192.0.2.10:8888?x=1"
    );
}

#[test]
fn local_override_rewrites_ts_net_name_and_exporter() {
    let addrs = addresses(&[("spark", "192.0.2.10")]);
    assert_eq!(
        apply_local_address("http://spark.puma-wyvern.ts.net:8888", &addrs),
        "http://192.0.2.10:8888"
    );

    let mut registry = parse_registry(
        "[engines.e]\nurl = \"http://spark.puma-wyvern.ts.net:8888\"\nexporter_url = \"http://spark.puma-wyvern.ts.net:9170\"\n\n[hosts.spark]\nfqdn = \"spark.puma-wyvern.ts.net\"\n",
    )
    .expect("parse inline registry");
    registry.apply_local(&addrs);
    let engine = registry.engine("e").expect("engine e");
    assert_eq!(engine.url, "http://192.0.2.10:8888");
    assert_eq!(
        engine.exporter_url.as_deref(),
        Some("http://192.0.2.10:9170")
    );
}

#[test]
fn local_override_leaves_other_hosts_alone() {
    let addrs = addresses(&[("spark", "192.0.2.10")]);
    assert_eq!(
        apply_local_address("http://hyper01.puma-wyvern.ts.net:8001", &addrs),
        "http://hyper01.puma-wyvern.ts.net:8001"
    );
    assert_eq!(
        apply_local_address("http://spark.example.com:8888", &addrs),
        "http://spark.example.com:8888"
    );
    assert_eq!(
        apply_local_address("http://spark.puma-wyvern.ts.net.evil:8888", &addrs),
        "http://spark.puma-wyvern.ts.net.evil:8888"
    );
    assert_eq!(
        apply_local_address("http://spark:8888", &addresses(&[])),
        "http://spark:8888"
    );
}

#[test]
fn local_override_brackets_ipv6() {
    let addrs = addresses(&[("spark", "fd00::5")]);
    assert_eq!(
        apply_local_address("http://spark.puma-wyvern.ts.net:8888", &addrs),
        "http://[fd00::5]:8888"
    );
    assert_eq!(
        apply_local_address("http://spark", &addrs),
        "http://[fd00::5]"
    );
    assert_eq!(
        apply_local_address("http://spark:8888/health", &addrs),
        "http://[fd00::5]:8888/health"
    );
}

#[test]
fn health_up_shapes() {
    assert!(health_up(&json!({"ok": true})));
    assert!(health_up(&json!({"status": "ok"})));
    assert!(health_up(&json!({"status": "ok", "version": "0.6.0"})));
    assert!(!health_up(&json!({"ok": false})));
    assert!(!health_up(&json!({"ok": false, "status": "ok"})));
    assert!(!health_up(&json!({"ok": "true"})));
    assert!(!health_up(&json!({"status": "starting"})));
    assert!(!health_up(&json!([{"ok": true}])));
    assert!(!health_up(&json!("ok")));
}

#[test]
fn describe_load_strings() {
    assert_eq!(
        describe_load(&json!({"requests_running": 4, "requests_waiting": 1})),
        ("busy (4 running, 1 waiting)".to_string(), 5)
    );
    assert_eq!(
        describe_load(&json!({"requests_running": 0, "requests_waiting": 0})),
        ("idle".to_string(), 0)
    );
    assert_eq!(
        describe_load(&json!({"live": {"connections": 3, "waiting": 2}})),
        ("busy (3 running, 2 waiting)".to_string(), 5)
    );
    assert_eq!(
        describe_load(&json!({"requests_running": 2, "live": {"waiting": 3}})),
        ("busy (2 running, 3 waiting)".to_string(), 5)
    );
    assert_eq!(
        describe_load(&json!({"requests_running": 0})),
        ("idle".to_string(), 0)
    );
    assert_eq!(describe_load(&json!({})), (String::new(), 0));
    assert_eq!(describe_load(&json!([])), (String::new(), 0));
}

#[test]
fn message_extraction() {
    let resp = Resp::new(
        500,
        r#"{"error": {"message": "bad  thing\n here"}}"#.to_string(),
        0.01,
        "",
    );
    assert_eq!(resp.message(110), "bad thing here");
    assert_eq!(resp.why(110), "HTTP 500: bad thing here");

    let resp = Resp::new(400, r#"{"detail": "no such model"}"#.to_string(), 0.0, "");
    assert_eq!(resp.message(110), "no such model");

    let resp = Resp::new(200, "  plain   text \n".to_string(), 0.0, "");
    assert_eq!(resp.message(110), "plain text");

    let resp = Resp::new(
        400,
        r#"{"detail": {"message": "inner", "code": 7}}"#.to_string(),
        0.0,
        "",
    );
    assert_eq!(resp.message(110), "inner");

    let resp = Resp::new(400, r#"{"detail": {"code": 7}}"#.to_string(), 0.0, "");
    assert_eq!(resp.message(110), r#"{"code": 7}"#);

    let resp = Resp::new(400, r#"{"error": "flat error"}"#.to_string(), 0.0, "");
    assert_eq!(resp.message(110), "flat error");

    let resp = Resp::new(400, r#"{"message": "top level"}"#.to_string(), 0.0, "");
    assert_eq!(resp.message(110), "top level");

    let resp = Resp::new(200, "abcdefgh".to_string(), 0.0, "");
    assert_eq!(resp.message(5), "abcde");
    assert!(resp.ok());

    let resp = Resp::new(
        0,
        String::new(),
        0.0,
        "URLError: [Errno 61] Connection refused",
    );
    assert!(!resp.ok());
    assert_eq!(resp.why(110), "URLError: [Errno 61] Connection refused");
    assert_eq!(resp.why(5), "URLEr");
}

#[test]
fn sanity_answer_regex() {
    for good in ["42", " 42 ", "-42.", "**42**", " (42) ", "\n42\t", "42!"] {
        assert!(sanity_answer_ok(good), "{good:?} should pass");
    }
    for bad in [
        "",
        "x42",
        "42x",
        "The answer is 42",
        "4 2",
        "421",
        "42.0",
        "42 and 42",
        "_42",
        "forty-two",
    ] {
        assert!(!sanity_answer_ok(bad), "{bad:?} should fail");
    }
}

#[test]
fn status_worst_wins() {
    assert_eq!(Status::SkippedBusy.rank(), 0);
    assert_eq!(Status::Ok.rank(), 0);
    assert_eq!(Status::Slow.rank(), 1);
    assert_eq!(Status::Wrong.rank(), 2);
    assert_eq!(Status::Down.rank(), 3);

    let mut result = CheckResult::new("e", "http://x", "h", "tensorfold");
    assert_eq!(result.status, Status::Ok);
    result.fail(Status::Slow, "slow one");
    assert_eq!(result.status, Status::Slow);
    result.fail(Status::Wrong, "wrong one");
    assert_eq!(result.status, Status::Wrong);
    result.fail(Status::Slow, "slow again");
    assert_eq!(result.status, Status::Wrong);
    result.fail(Status::Down, "down one");
    assert_eq!(result.status, Status::Down);
    result.fail(Status::Wrong, "wrong again");
    assert_eq!(result.status, Status::Down);
    assert_eq!(result.problems.len(), 5);

    let mut skipped = CheckResult::new("s", "http://x", "h", "tensorfold");
    skipped.status = Status::SkippedBusy;
    skipped.fail(Status::Ok, "not a failure");
    assert_eq!(skipped.status, Status::SkippedBusy);
}

#[test]
fn verdict_reports_drift() {
    let mut result = CheckResult::new("e", "http://x", "h", "tensorfold");
    assert_eq!(result.verdict(), "ok");
    assert!(result.drifted().is_empty());

    result.features.insert(
        "guided_choice".to_string(),
        Feature::record("yes", "answered 'apple'", Some("no")),
    );
    assert!(result.features["guided_choice"].drift);
    assert_eq!(result.drifted(), vec!["guided_choice"]);
    assert_eq!(result.verdict(), "drift");

    result.features.insert(
        "logprobs".to_string(),
        Feature::record("skipped", "shared engine busy", Some("no")),
    );
    assert!(!result.features["logprobs"].drift);
    assert_eq!(result.drifted(), vec!["guided_choice"]);

    result.features.insert(
        "prompt_logprobs".to_string(),
        Feature::record("yes", "returned", None),
    );
    assert!(
        !result.features["prompt_logprobs"].drift,
        "unjudged feature never drifts"
    );

    result.fail(Status::Slow, "slow");
    assert_eq!(result.verdict(), "slow");
    result.fail(Status::Down, "down");
    assert_eq!(result.verdict(), "down");
}

#[test]
fn exit_codes_follow_verdicts() {
    let ok = CheckResult::new("a", "u", "h", "tensorfold");
    assert_eq!(exit_code(std::slice::from_ref(&ok)), 0);

    let mut drift = CheckResult::new("b", "u", "h", "tensorfold");
    drift.features.insert(
        "logprobs".to_string(),
        Feature::record("yes", "returned", Some("no")),
    );
    assert_eq!(exit_code(std::slice::from_ref(&drift)), 2);
    assert_eq!(exit_code(&[ok.clone(), drift.clone()]), 2);

    let mut down = CheckResult::new("c", "u", "h", "tensorfold");
    down.fail(Status::Down, "health: refused");
    assert_eq!(exit_code(std::slice::from_ref(&down)), 1);
    assert_eq!(exit_code(&[ok, drift, down]), 1);
}

#[test]
fn to_json_has_python_fields_in_sorted_order() {
    let mut result = CheckResult::new("deepseek", "http://spark:8888", "spark", "tensorfold");
    result.model = "DeepSeek-V4.1-Flash-1017".to_string();
    result.version = "0.6.0".to_string();
    result.load = "idle".to_string();
    result.sanity = Sanity {
        result: "pass".to_string(),
        detail: "answered '42'".to_string(),
        latency_s: Some(0.125),
    };
    result.features.insert(
        "guided_choice".to_string(),
        Feature::record("no", "refused", Some("no")),
    );
    result.notes.push("loaded on cuda:0".to_string());
    result.requests = 3;
    result.fail(Status::Wrong, "model is x, registry expects m");

    let value = result.to_json();
    let obj = value.as_object().expect("object");

    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "engine", "features", "host", "kind", "load", "model", "notes", "problems", "requests",
            "sanity", "status", "url", "verdict", "version",
        ]
    );

    assert_eq!(obj["engine"], json!("deepseek"));
    assert_eq!(obj["url"], json!("http://spark:8888"));
    assert_eq!(obj["host"], json!("spark"));
    assert_eq!(obj["kind"], json!("tensorfold"));
    assert_eq!(obj["verdict"], json!("wrong"));
    assert_eq!(obj["status"], json!("wrong"));
    assert_eq!(obj["model"], json!("DeepSeek-V4.1-Flash-1017"));
    assert_eq!(obj["version"], json!("0.6.0"));
    assert_eq!(obj["load"], json!("idle"));
    assert_eq!(obj["requests"], json!(3));
    assert_eq!(obj["problems"], json!(["model is x, registry expects m"]));
    assert_eq!(obj["notes"], json!(["loaded on cuda:0"]));

    let sanity = obj["sanity"].as_object().expect("sanity object");
    assert_eq!(
        sanity.keys().map(String::as_str).collect::<Vec<_>>(),
        ["detail", "latency_s", "result"]
    );
    assert_eq!(sanity["result"], json!("pass"));
    assert_eq!(sanity["detail"], json!("answered '42'"));
    assert_eq!(sanity["latency_s"], json!(0.125));

    let feature = obj["features"]["guided_choice"]
        .as_object()
        .expect("feature object");
    assert_eq!(
        feature.keys().map(String::as_str).collect::<Vec<_>>(),
        ["drift", "expected", "reason", "result"]
    );
    assert_eq!(feature["result"], json!("no"));
    assert_eq!(feature["expected"], json!("no"));
    assert_eq!(feature["drift"], json!(false));

    let serialized = serde_json::to_string(&value).expect("serialize");
    assert!(serialized.starts_with(r#"{"engine":"#), "{serialized}");
}

#[test]
fn request_count_increments_on_refused_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let address = listener.local_addr().expect("local addr");
    drop(listener);

    let mut client = Client::new(&format!("http://{address}"), 2.0);
    assert_eq!(client.count, 0);

    let resp = client.get("/health");
    assert_eq!(resp.status, 0, "nothing is listening");
    assert_eq!(client.count, 1);
    assert!(!resp.error.is_empty());
    assert_eq!(resp.text, "");
    assert!(resp.json.is_none());

    let resp = client.post("/v1/chat/completions", &json!({"model": "m"}));
    assert_eq!(resp.status, 0);
    assert_eq!(client.count, 2);
}
