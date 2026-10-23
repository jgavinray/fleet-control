//! parity.py's comparison, replayed against the goldens.
//!
//! The projection is `comparable()` verbatim (parity.py:389-394): fifteen
//! fields, missing ones becoming null, `model` rendered the way both tools
//! print a non-finite float (parity.py:379-387), and problem/note counts
//! instead of message text. The three comparison paths are parity.py's:
//! `compare()` (parity.py:401-437), its usage-style branch (parity.py:414-417),
//! and `compare_help()` (parity.py:440-453).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::golden::GoldenRun;

const KEYS: [&str; 15] = [
    "engine",
    "url",
    "host",
    "kind",
    "verdict",
    "status",
    "model",
    "version",
    "load",
    "sanity_result",
    "sanity_latency_is_null",
    "features",
    "problem_count",
    "note_count",
    "requests",
];

const RUN_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Runner<'a> {
    pub bin: &'a Path,
    pub registry: &'a Path,
    pub dir: &'a Path,
    /// Engine name -> the URL this run's registry gave it, so the reported
    /// `url` is checked against the scenario, not just against the golden's
    /// port marker.
    pub urls: &'a BTreeMap<String, String>,
}

struct Outcome {
    exit: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

impl Runner<'_> {
    fn expand(&self, text: &str) -> String {
        text.replace("{registry}", &self.registry.display().to_string())
            .replace("{registry_dir}", &self.dir.display().to_string())
    }

    fn run(&self, args: &[String], cwd: Option<&Path>, env_reg: bool) -> Outcome {
        let mut command = Command::new(self.bin);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("FLEET_REGISTRY")
            // The goldens were recorded with the local LAN override disabled;
            // the override file must not rewrite the fixture's hosts.
            .env("FLEET_LOCAL", "");
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        if env_reg {
            command.env("FLEET_REGISTRY", self.registry);
        }
        let mut child = command.spawn().expect("spawn fleet-check-rs");
        let mut stdout = child.stdout.take().expect("child stdout");
        let mut stderr = child.stderr.take().expect("child stderr");
        let out_reader = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            stdout.read_to_end(&mut buffer).ok();
            buffer
        });
        let err_reader = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            stderr.read_to_end(&mut buffer).ok();
            buffer
        });

        let start = Instant::now();
        let mut timed_out = false;
        let status = loop {
            match child.try_wait().expect("wait for fleet-check-rs") {
                Some(status) => break Some(status),
                None if start.elapsed() >= RUN_TIMEOUT => {
                    child.kill().ok();
                    timed_out = true;
                    break child.wait().ok();
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        Outcome {
            exit: status.and_then(|status| status.code()),
            stdout: out_reader.join().unwrap_or_default(),
            stderr: err_reader.join().unwrap_or_default(),
            timed_out,
        }
    }
}

/// One comparison, exactly parity.py's `n_compared += 1` unit.
pub fn compare(run: &GoldenRun, runner: &Runner<'_>) -> Vec<String> {
    let label = &run.label;
    let args: Vec<String> = run.args.iter().map(|arg| runner.expand(arg)).collect();
    let cwd: Option<PathBuf> = run
        .cwd
        .as_ref()
        .map(|cwd| PathBuf::from(runner.expand(cwd)));
    let outcome = runner.run(&args, cwd.as_deref(), run.env_reg);

    let mut problems = Vec::new();
    if outcome.timed_out {
        problems.push(format!("{label}: timed out after {RUN_TIMEOUT:?}"));
        return problems;
    }
    let exit = outcome.exit.unwrap_or(-1);
    if exit != run.exit_code {
        problems.push(format!(
            "{label}: exit code golden={} rust={exit} (rust stderr: {})",
            run.exit_code,
            trimmed(&outcome.stderr, 200)
        ));
        return problems;
    }
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    let stderr = String::from_utf8_lossy(&outcome.stderr);

    match run.style.as_str() {
        // parity.py:414-417: exit 1 with no stdout is a usage-style failure;
        // only the exit code and a non-empty stderr are compared.
        "usage" => {
            if !(run.exit_code == 1 && run.stdout_empty) {
                problems.push(format!(
                    "{label}: golden is not a usage-style failure (exit {}, stdout_empty {})",
                    run.exit_code, run.stdout_empty
                ));
            }
            if !stdout.trim().is_empty() {
                problems.push(format!(
                    "{label}: rust printed stdout on a usage-style failure"
                ));
            }
            if stderr.trim().is_empty() {
                problems.push(format!(
                    "{label}: rust failed with exit 1 but printed nothing on stderr"
                ));
            }
        }
        // parity.py:440-453: the help text is compared byte for byte.
        "help" => {
            let expected = run.help_text.as_deref().unwrap_or_default();
            if stdout != expected {
                problems.push(format!(
                    "{label}: help text differs (golden {} bytes, rust {} bytes)",
                    expected.len(),
                    stdout.len()
                ));
            }
            if !stderr.trim().is_empty() || !run.stderr_empty {
                problems.push(format!(
                    "{label}: help run wrote to stderr (golden stderr_empty {}, rust {:?})",
                    run.stderr_empty,
                    trimmed(&outcome.stderr, 120)
                ));
            }
        }
        _ => {
            // parity.py:418-419: any stderr on a normal run fails.
            if !run.stderr_empty {
                problems.push(format!(
                    "{label}: golden recorded python stderr on a normal run: {:?}",
                    run.python_stderr_head
                ));
            }
            if !stderr.trim().is_empty() {
                problems.push(format!(
                    "{label}: rust wrote to stderr on a normal run: {:?}",
                    trimmed(&outcome.stderr, 200)
                ));
            }
            let document: Value = match serde_json::from_str(&stdout) {
                Ok(value) => value,
                Err(error) => {
                    problems.push(format!(
                        "{label}: output is not JSON ({error}); rust stdout starts {:?}",
                        first_bytes(&outcome.stdout, 120)
                    ));
                    return problems;
                }
            };
            // parity.py:427-428
            let json_exit = document.get("exit_code").and_then(Value::as_i64);
            if json_exit != run.json_exit_code {
                problems.push(format!(
                    "{label}: JSON exit_code golden={:?} rust={:?}",
                    run.json_exit_code, json_exit
                ));
            }
            let empty = Vec::new();
            let rust_engines = document
                .get("engines")
                .and_then(Value::as_array)
                .unwrap_or(&empty);
            // parity.py:429-432: the engine order must match; a mismatch stops
            // the per-engine comparison.
            let rust_order: Vec<&str> = rust_engines
                .iter()
                .map(|engine| engine.get("engine").and_then(Value::as_str).unwrap_or(""))
                .collect();
            let golden_order: Vec<&str> = run.engine_order.iter().map(String::as_str).collect();
            if golden_order != rust_order {
                problems.push(format!(
                    "{label}: engine order golden={golden_order:?} rust={rust_order:?}"
                ));
                return problems;
            }
            // parity.py:433-437: compare each engine's comparable projection.
            for (golden_engine, rust_engine) in run.engines.iter().zip(rust_engines) {
                let name = rust_engine
                    .get("engine")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(expected) = runner.urls.get(&name) {
                    let actual = rust_engine.get("url").and_then(Value::as_str);
                    if actual != Some(expected.as_str()) {
                        problems.push(format!(
                            "{label}: engine {name} url {actual:?} is not the scenario url {expected:?}"
                        ));
                    }
                }
                let mut rust_projection = projection(rust_engine);
                rust_projection["url"] = normalize_url(&rust_projection["url"]);
                let mut diff: Vec<(String, Value, Value)> = Vec::new();
                for key in KEYS {
                    let golden = golden_engine.get(key).cloned().unwrap_or(Value::Null);
                    let rust = rust_projection.get(key).cloned().unwrap_or(Value::Null);
                    if golden != rust {
                        diff.push((key.to_string(), golden, rust));
                    }
                }
                if !diff.is_empty() {
                    let shown: Vec<String> = diff
                        .iter()
                        .map(|(key, golden, rust)| format!("{key}: ({golden}, {rust})"))
                        .collect();
                    problems.push(format!(
                        "{label}: engine {name} differs (golden, rust): {{{}}}",
                        shown.join(", ")
                    ));
                }
            }
        }
    }
    problems
}

/// parity.py:389-394, `comparable()`.
fn projection(engine: &Value) -> Value {
    let sanity = get(engine, "sanity");
    let mut map = serde_json::Map::new();
    map.insert("engine".to_string(), get(engine, "engine"));
    map.insert("url".to_string(), get(engine, "url"));
    map.insert("host".to_string(), get(engine, "host"));
    map.insert("kind".to_string(), get(engine, "kind"));
    map.insert("verdict".to_string(), get(engine, "verdict"));
    map.insert("status".to_string(), get(engine, "status"));
    // parity.py:379-387, `rendered_model`: a non-finite float is carried as
    // the text both tables print. The Rust document already carries that
    // text, so the value is used as-is.
    map.insert("model".to_string(), get(engine, "model"));
    map.insert("version".to_string(), get(engine, "version"));
    map.insert("load".to_string(), get(engine, "load"));
    map.insert("sanity_result".to_string(), get(&sanity, "result"));
    map.insert(
        "sanity_latency_is_null".to_string(),
        Value::Bool(get(&sanity, "latency_s").is_null()),
    );
    map.insert("features".to_string(), get(engine, "features"));
    map.insert(
        "problem_count".to_string(),
        Value::from(get(engine, "problems").as_array().map_or(0, Vec::len)),
    );
    map.insert(
        "note_count".to_string(),
        Value::from(get(engine, "notes").as_array().map_or(0, Vec::len)),
    );
    map.insert("requests".to_string(), get(engine, "requests"));
    Value::Object(map)
}

/// `e.get(key)`, with a missing key becoming null like Python's `.get()`.
fn get(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or(Value::Null)
}

/// Mock ports are ephemeral, so the recorded url carries `{port}` where the
/// port was. This is the one projection field that cannot be compared
/// literally; the runner separately pins the rust url to the scenario's url.
fn normalize_url(value: &Value) -> Value {
    let Some(text) = value.as_str() else {
        return value.clone();
    };
    let Some(rest) = text.strip_prefix("http://127.0.0.1:") else {
        return value.clone();
    };
    if !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return Value::String("http://127.0.0.1:{port}".to_string());
    }
    value.clone()
}

fn trimmed(bytes: &[u8], limit: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.trim().chars().take(limit).collect()
}

fn first_bytes(bytes: &[u8], limit: usize) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(limit)]).to_string()
}
