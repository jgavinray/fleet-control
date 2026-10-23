//! Stopgap fleet watcher for the owner's Mac. Rust port of
//! `stopgap/fleet-stopgap.py`; the Python file is the behavioral spec.
//!
//! Run on a schedule (launchd, every 2 minutes). Each `run` tick:
//!
//! 1. runs `fleet-check --quick --json`,
//! 2. appends one line to the history file (so "was it up overnight?" has an answer),
//! 3. keeps the last 5 results for each engine,
//! 4. shows a desktop notification when an engine failed in 3 of its last 5 runs,
//!    and one when it has passed its last 2 runs again.
//!
//! Paths and commands are overridden with the same FLEET_STOPGAP_* environment
//! variables the Python tests used (check command, notifier, state dir). The
//! old FLEET_STOPGAP_LIB hook is gone: masking is in-crate (`redact` module,
//! also exposed as the `redact` subcommand).
use serde_json::{Map, Value};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

mod redact;

const WINDOW: usize = 5; // how many recent runs to look at
const THRESHOLD: i64 = 3; // failures within WINDOW that open an alert
const RECOVER: usize = 2; // consecutive passes that close an alert
const FAIL_STATUSES: [&str; 2] = ["down", "wrong"];
const CHECK_TIMEOUT: Duration = Duration::from_secs(240); // fleet-check waits up to 80 s on one slow engine
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(30);

const LABEL: &str = "com.jgavinray.fleet-stopgap";

fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").expect("HOME is not set"))
}

fn default_check() -> String {
    home()
        .join(".local/bin/fleet-check")
        .to_string_lossy()
        .into_owned()
}

fn default_state_dir() -> PathBuf {
    home().join(".local/state/fleet-stopgap")
}

// ------------------------------------------------------------------ time

/// Local offset east of UTC in seconds, from `/bin/date +%z` — the same
/// zone (TZ environment included) that Python's `datetime.now().astimezone()`
/// read. `tz` overrides the zone for this call only (tests pin it on the
/// child, so the machine's own zone never leaks into an assertion).
fn date_offset_secs(tz: Option<&str>) -> Option<i64> {
    let mut cmd = Command::new("/bin/date");
    cmd.arg("+%z").env("LC_ALL", "C");
    if let Some(tz) = tz {
        cmd.env("TZ", tz);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_offset(String::from_utf8_lossy(&out.stdout).trim())
}

/// Parses `date +%z` output: `±HHMM` (also the GNU `%:z` form, `±HH:MM`).
fn parse_offset(s: &str) -> Option<i64> {
    let (sign, digits) = match s.as_bytes().first()? {
        b'+' => (1i64, &s[1..]),
        b'-' => (-1i64, &s[1..]),
        _ => return None,
    };
    let (hh, mm) = match digits.as_bytes().get(2)? {
        b':' => (
            digits[..2].parse::<i64>().ok()?,
            digits[3..5].parse::<i64>().ok()?,
        ),
        _ => (
            digits[..2].parse::<i64>().ok()?,
            digits[2..4].parse::<i64>().ok()?,
        ),
    };
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(sign * (hh * 3600 + mm * 60))
}

/// Formats epoch seconds `secs` as `YYYY-MM-DDTHH:MM:SS±HH:MM` at a fixed
/// `offset` — the wall-clock string Python wrote via
/// `datetime.now().astimezone().isoformat(timespec="seconds")`.
fn format_ts(secs: i64, offset: i64) -> String {
    let (y, mo, d, h, mi, ss) = civil_from_epochs(secs + offset);
    let (sign, abs) = if offset < 0 {
        ('-', -offset)
    } else {
        ('+', offset)
    };
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{ss:02}{sign}{:02}:{:02}",
        abs / 3600,
        (abs % 3600) / 60
    )
}

/// Local wall-clock time plus the local hour and minute that `hhmm`
/// formats. The offset comes from one `/bin/date +%z` call taken at tick
/// start; if date is unavailable it falls back to `+00:00` — `run` must
/// work anywhere.
fn now_local(offset: i64) -> (String, u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (_y, _mo, _d, h, mi, _ss) = civil_from_epochs(secs + offset);
    (format_ts(secs, offset), h, mi)
}

/// Days-from-civil (Hinnant) inverse, used only when /bin/date is missing.
fn civil_from_epochs(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let ss = (rem % 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, h, mi, ss)
}

/// `%I:%M %p` with leading zeros stripped, from the stored ISO ts. Python
/// would raise on a malformed stamp; we keep it verbatim instead (the only
/// stamps in state are ones this binary wrote).
fn hhmm(ts: &str) -> String {
    let Some(tpos) = ts.find('T') else {
        return ts.to_string();
    };
    let rest = &ts[tpos + 1..];
    let bytes = rest.as_bytes();
    if bytes.len() < 5 || bytes[2] != b':' || !bytes[..2].iter().all(u8::is_ascii_digit) {
        return ts.to_string();
    }
    let Ok(hour) = rest[..2].parse::<u32>() else {
        return ts.to_string();
    };
    let minute = &rest[3..5];
    let hh12 = match hour % 24 {
        0 => 12,
        h if h > 12 => h - 12,
        h => h,
    };
    let ampm = if hour % 24 < 12 { "AM" } else { "PM" };
    format!("{hh12:02}:{minute} {ampm}")
        .trim_start_matches('0')
        .to_string()
}

// ------------------------------------------------------------------ processes

enum RunError {
    Spawn(std::io::ErrorKind),
    Timeout,
}

/// Run `cmd args…` capturing stdout/stderr, killing it past `timeout`.
fn run_captured(cmd: &str, args: &[&str], timeout: Duration) -> Result<Output, RunError> {
    let child = match Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(RunError::Spawn(e.kind())),
    };
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(RunError::Spawn(e.kind())),
        Err(_) => {
            // std::process::Child was moved into the waiter thread; /bin/kill
            // (present on macOS and Linux) plays Child::kill. A timed-out run
            // is TimeoutExpired no matter what partial output it left behind,
            // exactly like subprocess.run raising before the caller sees data.
            let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
            let _ = rx.recv(); // reap the waiter, discard partial output
            Err(RunError::Timeout)
        }
    }
}

// ------------------------------------------------------------------ check

/// `(parsed json or None, exit code, error text)` — same shape as the
/// Python `run_fleet_check`.
fn run_fleet_check(cmd: &str) -> (Option<Value>, i64, String) {
    match run_captured(cmd, &["--quick", "--json"], CHECK_TIMEOUT) {
        // Divergence: a spawn failure names the std::io::ErrorKind Debug form
        // (`NotFound`, `PermissionDenied`) where Python named the exception
        // (`FileNotFoundError`, `PermissionError`). The timeout line is the
        // Python text verbatim: subprocess raises TimeoutExpired, not an OSError.
        Err(RunError::Spawn(kind)) => (None, -1, format!("could not run fleet-check: {kind:?}")),
        Err(RunError::Timeout) => (
            None,
            -1,
            "could not run fleet-check: TimeoutExpired".to_string(),
        ),
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1) as i64;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            // jsonpy parses like Python's json.loads, NaN/Infinity included.
            // Divergence: a top-level JSON array made Python crash with an
            // AttributeError; both "not an object" and "no engines list" are
            // recorded as unusable output here.
            let unusable = || {
                (
                    None,
                    code,
                    format!("fleet-check output was not usable (exit {code})"),
                )
            };
            let Some(data) = fleet_core::jsonpy::parse(&text) else {
                return unusable();
            };
            match data.get("engines") {
                Some(Value::Array(_)) => (Some(data), code, String::new()),
                _ => unusable(),
            }
        }
    }
}

// ------------------------------------------------------------------ notify

/// `on run argv … end run` — the exact AppleScript the Python script passed
/// to `osascript -e`, argv[1] title, argv[2] body.
const OSASCRIPT: &str =
    "on run argv\n display notification (item 2 of argv) with title (item 1 of argv)\nend run";

fn notify(notifier: Option<&String>, title: &str, body: &str) -> bool {
    let (prog, args): (String, Vec<&str>) = match notifier {
        Some(cmd) => (cmd.clone(), vec![title, body]),
        None => ("osascript".to_string(), vec!["-e", OSASCRIPT, title, body]),
    };
    match run_captured(&prog, &args, NOTIFY_TIMEOUT) {
        Ok(out) => out.status.code() == Some(0),
        Err(_) => false,
    }
}

// ------------------------------------------------------------------ state json

/// Serialize like `json.dumps(obj, sort_keys=True, separators=(", ", ": "),
/// ensure_ascii=True)` — one history line.
fn dumps_sorted(value: &Value) -> String {
    let mut out = String::new();
    write_compact(value, &mut out);
    out
}

fn write_compact(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in sorted_entries(map).iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_json_string(k, out);
                out.push_str(": ");
                write_compact(v, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_compact(v, out);
            }
            out.push(']');
        }
        other => write_scalar(other, out),
    }
}

/// Serialize like `json.dumps(obj, sort_keys=True, indent=1)` — state.json,
/// down to the `state.tmp` sibling name the Python code used.
fn dumps_indent1(value: &Value) -> String {
    let mut out = String::new();
    write_indent(value, 0, &mut out);
    out
}

fn write_indent(value: &Value, depth: usize, out: &mut String) {
    let inner = " ".repeat(depth + 1);
    let outer = " ".repeat(depth);
    match value {
        Value::Object(map) if !map.is_empty() => {
            out.push_str("{\n");
            for (i, (k, v)) in sorted_entries(map).iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&inner);
                write_json_string(k, out);
                out.push_str(": ");
                write_indent(v, depth + 1, out);
            }
            out.push('\n');
            out.push_str(&outer);
            out.push('}');
        }
        Value::Array(items) if !items.is_empty() => {
            out.push_str("[\n");
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&inner);
                write_indent(v, depth + 1, out);
            }
            out.push('\n');
            out.push_str(&outer);
            out.push(']');
        }
        Value::Object(_) => out.push_str("{}"),
        Value::Array(_) => out.push_str("[]"),
        other => write_scalar(other, out),
    }
}

fn write_scalar(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_json_string(s, out),
        _ => unreachable!("handled by callers"),
    }
}

fn sorted_entries(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut v: Vec<_> = map.iter().collect();
    // Python sorted() on str keys compares code points; byte order on UTF-8
    // strings agrees.
    v.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    v
}

/// json.dumps ensure_ascii=True escaping.
fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x7f => out.push(c),
            c if (c as u32) <= 0xffff => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => {
                let cp = c as u32 - 0x1_0000;
                out.push_str(&format!(
                    "\\u{:04x}\\u{:04x}",
                    0xd800 + (cp >> 10),
                    0xdc00 + (cp & 0x3ff)
                ));
            }
        }
    }
    out.push('"');
}

fn load_state(path: &Path) -> Value {
    let default = || {
        Value::Object(Map::from_iter([(
            "engines".to_string(),
            Value::Object(Map::new()),
        )]))
    };
    let Ok(text) = fs::read_to_string(path) else {
        return default();
    };
    match fleet_core::jsonpy::parse(&text) {
        // Python kept the whole loaded object only when engines was a dict;
        // anything else (or unparseable) restarts from {"engines": {}}.
        Some(v @ Value::Object(_)) if v.get("engines").is_some_and(Value::is_object) => v,
        _ => default(),
    }
}

fn save_state(path: &Path, state: &Value) {
    let tmp = path.with_extension("tmp"); // state.json -> state.tmp, as in the .py
    fs::write(&tmp, dumps_indent1(state)).expect("cannot write state tmp");
    fs::rename(&tmp, path).expect("cannot replace state file");
}

// ------------------------------------------------------------------ helpers

fn py_str(v: &Value) -> String {
    fleet_core::py_str(v)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(n)) => n.as_f64().is_none_or(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `mask(str(probs[0]))[:160]` — the single masking call site (fleet-stopgap.py:148).
fn detail_from_problems(probs: &Value) -> String {
    let first: String = match probs {
        Value::Array(items) => {
            let Some(first) = items.first() else {
                return String::new(); // handled by the caller's truthiness check
            };
            py_str(first)
        }
        Value::String(s) => s.chars().next().unwrap_or_default().to_string(), // Python indexes strings: "abc"[0]
        other => py_str(other), // divergence: Python raised TypeError for un-subscriptable problems
    };
    truncate_chars(&redact::redact(&first).0, 160)
}

fn open_names(state: &Value) -> Vec<String> {
    let mut v: Vec<String> = state
        .get("engines")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(_, e)| truthy(e.get("open_since")))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

// ------------------------------------------------------------------ tick

fn tick() -> i32 {
    let state_dir = match env::var_os("FLEET_STOPGAP_STATE_DIR") {
        Some(d) => PathBuf::from(d),
        None => default_state_dir(),
    };
    fs::create_dir_all(&state_dir).expect("cannot create state dir");
    let history_path = state_dir.join("history.jsonl");
    let state_path = state_dir.join("state.json");
    let notifier = env::var("FLEET_STOPGAP_NOTIFIER")
        .ok()
        .filter(|s| !s.is_empty());
    let check_cmd = env::var("FLEET_STOPGAP_CHECK").unwrap_or_else(|_| default_check());

    // One `date +%z` per tick, taken up front like the check command; it
    // respects TZ and the machine zone exactly like astimezone(). UTC
    // (+00:00) only if date itself is unavailable.
    let tz_offset = date_offset_secs(None).unwrap_or(0);
    let (ts, hour, minute) = now_local(tz_offset);
    let mut state = load_state(&state_path);
    let (data, code, err) = run_fleet_check(&check_cmd);

    let mut line = Map::new();
    line.insert("ts".into(), Value::String(ts.clone()));
    line.insert("exit".into(), Value::from(code));
    let mut to_alert: Vec<String> = Vec::new();
    let mut to_recover: Vec<String> = Vec::new();
    let mut details: Map<String, Value> = Map::new();

    let engines_arr = data
        .as_ref()
        .and_then(|d| d.get("engines"))
        .and_then(Value::as_array)
        .cloned();

    if data.is_none() {
        line.insert("result".into(), Value::String("error".into()));
        line.insert("note".into(), Value::String(err));
        line.insert("engines".into(), Value::Object(Map::new()));
        line.insert(
            "open".into(),
            Value::Array(open_names(&state).into_iter().map(Value::String).collect()),
        );
    } else {
        let mut statuses = Map::new();
        for eng in engines_arr.unwrap_or_default() {
            // Python called eng.get and would crash on a non-object entry.
            let eng = eng
                .as_object()
                .expect("fleet-check engines list contains a non-object");
            let name = eng.get("engine").map_or_else(|| "?".to_string(), py_str);
            let status = eng.get("status").map_or_else(|| "?".to_string(), py_str);
            statuses.insert(name.clone(), Value::String(status.clone()));
            if status == "skipped-busy" {
                continue; // nobody was asked, so it says nothing either way
            }
            let failed = FAIL_STATUSES.contains(&status.as_str());
            let engines = state["engines"]
                .as_object_mut()
                .expect("state engines is an object");
            let es = engines.entry(name.clone()).or_insert_with(|| {
                Value::Object(Map::from_iter([
                    ("recent".to_string(), Value::Array(Vec::new())),
                    ("open_since".to_string(), Value::Null),
                ]))
            });
            let es = es.as_object_mut().expect("engine state is an object");
            // A loaded state without these keys crashes in Python (KeyError);
            // we materialise them instead.
            let (window_sum, last_two) = {
                let recent = es
                    .entry("recent".to_string())
                    .or_insert_with(|| Value::Array(Vec::new()));
                let recent = recent.as_array_mut().expect("recent is a list");
                recent.push(Value::from(if failed { 1 } else { 0 }));
                let overflow = recent.len().saturating_sub(WINDOW);
                recent.drain(0..overflow);
                let sum: i64 = recent
                    .iter()
                    .map(|v| v.as_i64().expect("recent holds ints"))
                    .sum();
                let last_two: Vec<i64> = recent
                    .iter()
                    .rev()
                    .take(RECOVER)
                    .map(|v| v.as_i64().expect("recent holds ints"))
                    .collect();
                (sum, last_two)
            };
            if failed {
                let probs = eng.get("problems");
                let detail = if truthy(probs) {
                    detail_from_problems(probs.unwrap())
                } else {
                    status.clone()
                };
                details.insert(name.clone(), Value::String(detail));
            }
            let open_since = es.get("open_since");
            if open_since.is_none() || open_since == Some(&Value::Null) {
                if window_sum >= THRESHOLD {
                    to_alert.push(name);
                }
            } else if last_two.len() == RECOVER && last_two.iter().all(|v| *v == 0) {
                to_recover.push(name);
            }
        }
        let any_fail = statuses
            .values()
            .any(|v| v.as_str().is_some_and(|s| FAIL_STATUSES.contains(&s)));
        line.insert(
            "result".into(),
            Value::String((if any_fail { "fail" } else { "ok" }).into()),
        );
        line.insert("engines".into(), Value::Object(statuses));
        line.insert("problems".into(), Value::Object(details.clone()));
    }

    let mut events: Vec<Value> = Vec::new();
    let mut notify_error: Option<String> = None;

    if !to_alert.is_empty() {
        let mut sorted = to_alert.clone();
        sorted.sort();
        let who = sorted.join(", ");
        let verb = if sorted.len() == 1 { "is" } else { "are" };
        let body = sorted
            .iter()
            .map(|n| {
                let d = details.get(n).and_then(Value::as_str).unwrap_or("failed");
                format!("{n}: {d}")
            })
            .collect::<Vec<_>>()
            .join("; ");
        let body = format!("Failed {THRESHOLD} or more of the last {WINDOW} checks. {body}");
        if notify(
            notifier.as_ref(),
            &format!("Fleet: {who} {verb} down"),
            &truncate_chars(&body, 300),
        ) {
            for n in &to_alert {
                set_engine(&mut state, n, "open_since", Value::String(ts.clone()));
                events.push(Value::String(format!("alert:{n}")));
            }
        } else {
            notify_error = Some("alert notification failed; will retry next run".into());
        }
    }
    if !to_recover.is_empty() {
        let mut sorted = to_recover.clone();
        sorted.sort();
        let who = sorted.join(", ");
        let verb = if sorted.len() == 1 { "is" } else { "are" };
        let since = sorted
            .iter()
            .map(|n| {
                let since = state
                    .get("engines")
                    .and_then(|e| e.get(n))
                    .and_then(|e| e.get("open_since"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                format!("{n} since {}", hhmm(since))
            })
            .collect::<Vec<_>>()
            .join("; ");
        let body = format!("Passed the last {RECOVER} checks. Was down: {since}.");
        if notify(
            notifier.as_ref(),
            &format!("Fleet: {who} {verb} back up"),
            &truncate_chars(&body, 300),
        ) {
            for n in &to_recover {
                set_engine(&mut state, n, "open_since", Value::Null);
                set_engine(&mut state, n, "recent", Value::Array(Vec::new())); // old failures must not re-open it
                events.push(Value::String(format!("recovered:{n}")));
            }
        } else {
            notify_error = Some("recovery notification failed; will retry next run".into());
        }
    }

    line.insert("events".into(), Value::Array(events));
    if let Some(err) = notify_error {
        line.insert("notify_error".into(), Value::String(err));
    }
    if data.is_some() {
        line.insert(
            "open".into(),
            Value::Array(open_names(&state).into_iter().map(Value::String).collect()),
        );
        save_state(&state_path, &state);
    } else {
        // (hour, minute) unused on the error path; keep the compiler honest.
        let _ = (hour, minute);
    }
    let mut fh = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history_path)
        .expect("cannot open history file");
    writeln!(fh, "{}", dumps_sorted(&Value::Object(line))).expect("cannot write history");
    0
}

fn set_engine(state: &mut Value, name: &str, key: &str, value: Value) {
    state["engines"][name][key] = value;
}

// ------------------------------------------------------------------ install

fn plist_xml(exe: &str, state_dir: &Path) -> String {
    let exe_dir = Path::new(exe)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;");
    let state = state_dir.to_string_lossy();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
  </array>
  <key>StartInterval</key>
  <integer>120</integer>
  <key>RunAtLoad</key>
  <true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{}:/usr/local/bin:/usr/bin:/bin</string>
  </dict>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{}/launchd.log</string>
  <key>StandardErrorPath</key>
  <string>{}/launchd.log</string>
</dict>
</plist>
"#,
        esc(exe),
        esc(&exe_dir),
        esc(&state),
        esc(&state),
    )
}

fn uid() -> Result<u32, String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| format!("cannot run id: {e}"))?;
    if !out.status.success() {
        return Err("cannot run id: not usable".into());
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    text.parse()
        .map_err(|_| format!("cannot parse uid from {text}"))
}

fn launchctl(args: &[&str]) -> Result<(), String> {
    let status = Command::new("launchctl")
        .args(args)
        .status()
        .map_err(|e| format!("cannot run launchctl: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("launchctl {} failed", args.join(" ")))
    }
}

struct InstallOpts {
    plist_dir: Option<PathBuf>,
    no_launchctl: bool,
}

fn cmd_install(opts: InstallOpts) -> i32 {
    if env::consts::OS != "macos" {
        eprintln!("fleet-stopgap install is macOS-only (launchd); run works anywhere.");
        return 1;
    }
    let exe = env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let state_dir = default_state_dir();
    let plist_dir = opts
        .plist_dir
        .unwrap_or_else(|| home().join("Library/LaunchAgents"));
    let plist = plist_dir.join(format!("{LABEL}.plist"));
    if let Err(e) = fs::create_dir_all(&plist_dir)
        .and_then(|()| fs::create_dir_all(&state_dir))
        .and_then(|()| fs::write(&plist, plist_xml(&exe, &state_dir)))
    {
        eprintln!("install failed: {e}");
        return 1;
    }
    // plutil -lint, like the shell installer: a bad plist never bootstraps.
    match Command::new("plutil")
        .arg("-lint")
        .arg(&plist)
        .stdout(Stdio::null())
        .status()
    {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("install failed: plutil rejected {LABEL}.plist");
            return 1;
        }
    }
    if !opts.no_launchctl {
        let domain = match uid().map(|u| format!("gui/{u}")) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("install failed: {e}");
                return 1;
            }
        };
        let _ = launchctl(&["bootout", &format!("{domain}/{LABEL}")]); // like install.sh: ignore failure
        if let Err(e) = launchctl(&["bootstrap", &domain, &plist.to_string_lossy()]) {
            eprintln!("install failed: {e}");
            return 1;
        }
    }
    println!(
        "installed {LABEL}: runs every 2 minutes; history in {}/history.jsonl",
        state_dir.display()
    );
    0
}

fn cmd_uninstall(opts: InstallOpts) -> i32 {
    if env::consts::OS != "macos" {
        eprintln!("fleet-stopgap uninstall is macOS-only (launchd); run works anywhere.");
        return 1;
    }
    if !opts.no_launchctl {
        if let Ok(u) = uid() {
            let _ = launchctl(&["bootout", &format!("gui/{u}/{LABEL}")]); // || true, as in install.sh
        }
    }
    let plist_dir = opts
        .plist_dir
        .unwrap_or_else(|| home().join("Library/LaunchAgents"));
    let plist = plist_dir.join(format!("{LABEL}.plist"));
    if let Err(e) = fs::remove_file(&plist) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!("uninstall failed: {e}");
            return 1;
        }
    }
    println!(
        "stopped and removed {LABEL}; history kept in {}",
        default_state_dir().display()
    );
    0
}

// ------------------------------------------------------------------ redact CLI

/// `python3 -m fleetlib.redact < in > out`, in-crate: stdin in, masked text
/// out. `--count` prints the masked-span count to stderr, `--max-chars` cuts
/// the masked output, `--list-rules` lists the rule table.
fn cmd_redact(args: &[String]) -> i32 {
    let mut count = false;
    let mut max_chars: usize = 0;
    let mut list_rules = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--count" => count = true,
            "--max-chars" => {
                i += 1;
                let Some(raw) = args.get(i) else {
                    eprintln!("error: argument --max-chars: expected one argument");
                    return 2;
                };
                match raw.parse::<usize>() {
                    Ok(n) => max_chars = n,
                    Err(_) => {
                        eprintln!("error: argument --max-chars: invalid int value: '{raw}'");
                        return 2;
                    }
                }
            }
            "--list-rules" => list_rules = true,
            "-h" | "--help" => {
                println!("usage: fleet-stopgap redact [--count] [--max-chars N] [--list-rules]");
                println!("Read text on stdin, write it to stdout with secrets masked.");
                return 0;
            }
            other => {
                eprintln!("error: unrecognized arguments: {other}");
                return 2;
            }
        }
        i += 1;
    }
    if list_rules {
        for (id, kind) in redact::rule_table() {
            println!("{id}\t{kind}");
        }
        return 0;
    }
    let mut raw = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut raw) {
        eprintln!("error: cannot read stdin: {e}");
        return 1;
    }
    // Python decoded with errors="replace"; from_utf8_lossy matches.
    let (mut masked, n) = redact::redact(&String::from_utf8_lossy(&raw));
    if max_chars > 0 && masked.chars().count() > max_chars {
        masked = masked.chars().take(max_chars).collect();
    }
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(masked.as_bytes());
    let _ = lock.flush();
    if count {
        eprintln!("{n}");
    }
    0
}

// ------------------------------------------------------------------ main

const HELP: &str = "\
fleet-stopgap — fleet watcher (Rust port of stopgap/fleet-stopgap.py)

usage:
  fleet-stopgap                       one tick (what launchd runs)
  fleet-stopgap run                   same as above
  fleet-stopgap install [--plist-dir D] [--no-launchctl]
                                      write the LaunchAgent plist and bootstrap it
  fleet-stopgap uninstall [--plist-dir D] [--no-launchctl]
                                      bootout and remove the plist (history is kept)
  fleet-stopgap redact [--count] [--max-chars N] [--list-rules]
                                      mask secrets: stdin in, text out

environment (same hooks as the Python tests):
  FLEET_STOPGAP_CHECK     check command (default ~/.local/bin/fleet-check)
  FLEET_STOPGAP_NOTIFIER  notifier override (title, body args)
  FLEET_STOPGAP_STATE_DIR state/history directory
                          (default ~/.local/state/fleet-stopgap)
";

fn parse_install_flags(args: &[String]) -> Result<InstallOpts, String> {
    let mut opts = InstallOpts {
        plist_dir: None,
        no_launchctl: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--plist-dir" => {
                i += 1;
                let dir = args.get(i).ok_or("--plist-dir needs a directory")?;
                opts.plist_dir = Some(PathBuf::from(dir));
            }
            "--no-launchctl" => opts.no_launchctl = true,
            "-h" | "--help" => opts.plist_dir = opts.plist_dir.clone(), // no-op; help printed below
            other => return Err(format!("unrecognized argument: {other}")),
        }
        i += 1;
    }
    Ok(opts)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None | Some("run") => tick(),
        Some("install") => match parse_install_flags(&args[1..]) {
            Ok(opts) => cmd_install(opts),
            Err(e) => {
                eprintln!("error: {e}");
                eprint!("{HELP}");
                2
            }
        },
        Some("uninstall") => match parse_install_flags(&args[1..]) {
            Ok(opts) => cmd_uninstall(opts),
            Err(e) => {
                eprintln!("error: {e}");
                eprint!("{HELP}");
                2
            }
        },
        Some("redact") => cmd_redact(&args[1..]),
        Some("-h") | Some("--help") | Some("help") => {
            print!("{HELP}");
            0
        }
        Some(other) => {
            eprintln!("error: unknown command: {other}");
            eprint!("{HELP}");
            2
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hhmm_matches_strftime_lstrip() {
        // Ported semantics: %I:%M %p, leading zero stripped from the field.
        assert_eq!(hhmm("2026-10-21T19:23:05-07:00"), "7:23 PM");
        assert_eq!(hhmm("2026-10-21T07:05:09+08:00"), "7:05 AM");
        assert_eq!(hhmm("2026-10-21T00:05:09+08:00"), "12:05 AM");
        assert_eq!(hhmm("2026-10-21T12:00:00+00:00"), "12:00 PM");
        assert_eq!(hhmm("2026-10-21T23:59:59+00:00"), "11:59 PM");
        assert_eq!(hhmm("not a timestamp"), "not a timestamp");
    }

    #[test]
    fn sorted_dump_matches_json_dumps_sort_keys() {
        let mut outer = Map::new();
        let mut engines = Map::new();
        engines.insert("zeta".into(), json!({"recent": [1], "open_since": null}));
        engines.insert("alpha".into(), json!({"recent": [], "open_since": "s"}));
        outer.insert("engines".into(), Value::Object(engines));
        let got = dumps_sorted(&Value::Object(outer));
        assert_eq!(
            got,
            "{\"engines\": {\"alpha\": {\"open_since\": \"s\", \"recent\": []}, \"zeta\": {\"open_since\": null, \"recent\": [1]}}}"
        );
    }

    #[test]
    fn indent1_dump_matches_python_state_shape() {
        // The exact bytes save_state wrote: json.dumps(..., sort_keys=True, indent=1).
        let mut engines = Map::new();
        let mut glm = Map::new();
        glm.insert("open_since".into(), json!("x"));
        glm.insert("recent".into(), json!([1, 0]));
        let mut deepseek = Map::new();
        deepseek.insert("open_since".into(), Value::Null);
        deepseek.insert("recent".into(), json!([]));
        engines.insert("glm".into(), Value::Object(glm));
        engines.insert("deepseek".into(), Value::Object(deepseek));
        let mut top = Map::new();
        top.insert("engines".into(), Value::Object(engines));
        let expected = "{\n \"engines\": {\n  \"deepseek\": {\n   \"open_since\": null,\n   \"recent\": []\n  },\n  \"glm\": {\n   \"open_since\": \"x\",\n   \"recent\": [\n    1,\n    0\n   ]\n  }\n }\n}";
        assert_eq!(dumps_indent1(&Value::Object(top)), expected);
    }

    #[test]
    fn truthy_matches_python() {
        assert!(truthy(Some(&json!(1))));
        assert!(truthy(Some(&json!("x"))));
        assert!(truthy(Some(&json!([0]))));
        assert!(!truthy(Some(&json!(0))));
        assert!(!truthy(Some(&json!(""))));
        assert!(!truthy(Some(&Value::Null)));
        assert!(!truthy(Some(&json!(false))));
        assert!(!truthy(Some(&json!({}))));
        assert!(!truthy(None));
    }

    #[test]
    fn plist_xml_matches_the_template_skeleton() {
        let xml = plist_xml("/opt/fleet-stopgap", std::path::Path::new("/state"));
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist"));
        assert!(xml.contains(&format!("<string>{LABEL}</string>")));
        assert!(xml.contains("<key>StartInterval</key>\n  <integer>120</integer>"));
        assert!(xml.ends_with("</dict>\n</plist>\n"));
    }

    #[test]
    fn parse_offset_accepts_the_date_forms() {
        assert_eq!(parse_offset("+0800"), Some(8 * 3600));
        assert_eq!(parse_offset("-0700"), Some(-7 * 3600));
        assert_eq!(parse_offset("+0530"), Some(5 * 3600 + 30 * 60));
        assert_eq!(parse_offset("+08:00"), Some(8 * 3600)); // GNU %:z form
        assert_eq!(parse_offset("0800"), None); // sign is mandatory
        assert_eq!(parse_offset("+2400"), None);
        assert_eq!(parse_offset("+0560"), None);
        assert_eq!(parse_offset(""), None);
    }

    #[test]
    fn format_ts_pins_epoch_and_offset() {
        // 1792733383 = 2026-10-23T05:29:43Z; at -0700 the wall clock reads
        // 2026-10-22T22:29:43, exactly what astimezone().isoformat wrote.
        assert_eq!(
            format_ts(1_792_733_383, -7 * 3600),
            "2026-10-22T22:29:43-07:00"
        );
        assert_eq!(format_ts(1_792_733_383, 0), "2026-10-23T05:29:43+00:00");
        assert_eq!(
            format_ts(1_792_733_383, 8 * 3600),
            "2026-10-23T13:29:43+08:00"
        );
    }

    #[test]
    fn date_offset_follows_the_tz_environment() {
        // TZ is pinned on the child process, so the machine's own zone can
        // never leak into the assertion.
        assert_eq!(date_offset_secs(Some("UTC")), Some(0));
        let la = date_offset_secs(Some("America/Los_Angeles"));
        // Same zone date(1) resolves for the current instant: -0700 under
        // daylight saving, -0800 in winter; /bin/date decides which today.
        assert!(la == Some(-7 * 3600) || la == Some(-8 * 3600), "got {la:?}");
        // And the rendered stamp carries that same offset.
        let (ts, _, _) = now_local(la.unwrap_or(0));
        let want = match la {
            Some(-25_200) => "-07:00",
            _ => "-08:00",
        };
        assert!(ts.ends_with(want), "{ts}");
    }

    #[test]
    fn now_local_falls_back_to_utc_only_via_the_offset_argument() {
        let (ts, hour, minute) = now_local(0);
        assert_eq!(ts.len(), 25, "{ts}");
        assert!(ts.ends_with("+00:00"), "{ts}");
        assert!(hour < 24 && minute < 60);
    }
}
