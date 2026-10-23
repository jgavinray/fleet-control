//! Integration tests for the stopgap tick, ported from stopgap/tests/test_stopgap.py.
//! They fake fleet-check and the notifier; nothing touches the fleet.

use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const OK: &str = "glm=ok,deepseek=ok";
const D: &str = "glm=down,deepseek=ok";

fn fixtures(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn tempdir() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "stopgap-rs-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

struct Harness {
    dir: PathBuf,
    seq: PathBuf,
    notes: PathBuf,
    fail_flag: PathBuf,
    state_dir: PathBuf,
}

impl Harness {
    fn new(seq: &[&str]) -> Harness {
        let dir = tempdir();
        let seq_path = dir.join("seq");
        fs::write(&seq_path, seq.join("\n") + "\n").unwrap();
        Harness {
            notes: dir.join("notes"),
            fail_flag: dir.join("notify-fails"),
            state_dir: dir.join("state"),
            dir,
            seq: seq_path,
        }
    }

    fn env_cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fleet-stopgap"));
        cmd.args(args)
            .env("FAKE_SEQ", &self.seq)
            .env("FAKE_NOTES", &self.notes)
            .env("FAKE_NOTIFY_FAIL", &self.fail_flag)
            .env("FLEET_STOPGAP_CHECK", fixtures("fake-fleet-check"))
            .env("FLEET_STOPGAP_NOTIFIER", fixtures("fake-notifier"))
            .env("FLEET_STOPGAP_STATE_DIR", &self.state_dir);
        cmd
    }

    fn run(&self, times: usize) {
        for _ in 0..times {
            let out = self.env_cmd(&["run"]).output().unwrap();
            assert!(
                out.status.success(),
                "run failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn notes_list(&self) -> Vec<String> {
        match fs::read_to_string(&self.notes) {
            Ok(s) => s.lines().map(String::from).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn history(&self) -> Vec<Value> {
        fs::read_to_string(self.state_dir.join("history.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn set_seq(&self, seq: &[&str]) {
        fs::write(&self.seq, seq.join("\n") + "\n").unwrap();
        let _ = fs::remove_file(self.seq.with_extension("n"));
    }
}

fn go(seq: &[&str]) -> Harness {
    let h = Harness::new(seq);
    h.run(seq.len());
    h
}

fn alerts(notes: &[String]) -> Vec<String> {
    notes
        .iter()
        .filter(|n| !n.contains("back up"))
        .cloned()
        .collect()
}

fn recoveries(notes: &[String]) -> Vec<String> {
    notes
        .iter()
        .filter(|n| n.contains("back up"))
        .cloned()
        .collect()
}

#[test]
fn healthy_fleet_is_silent() {
    let h = go(&[OK; 10]);
    assert_eq!(h.notes_list(), Vec::<String>::new());
}

#[test]
fn one_failure_in_five_is_silent() {
    let h = go(&[OK, OK, D, OK, OK, OK]);
    assert_eq!(h.notes_list(), Vec::<String>::new());
}

#[test]
fn two_failures_in_five_is_silent() {
    for seq in [
        vec![D, D, OK, OK, OK],
        vec![D, OK, D, OK, OK],
        vec![OK, D, OK, OK, D, OK, OK],
    ] {
        assert_eq!(
            go(&seq).notes_list(),
            Vec::<String>::new(),
            "silent for {seq:?}"
        );
    }
}

#[test]
fn three_failures_in_five_alerts_once() {
    let h = go(&[D, D, OK, D]); // third failure within five runs, on run 4
    let a = alerts(&h.notes_list());
    assert_eq!(a.len(), 1);
    assert!(a[0].contains("glm is down"), "{}", a[0]);
    assert!(!a[0].contains("deepseek"), "{}", a[0]);
    assert!(recoveries(&h.notes_list()).is_empty());
}

#[test]
fn alert_fires_on_the_run_that_makes_three() {
    let h = Harness::new(&[D, D, D, D]);
    h.run(2);
    assert_eq!(h.notes_list(), Vec::<String>::new());
    h.run(1);
    assert_eq!(h.notes_list().len(), 1);
    h.run(1);
    assert_eq!(
        h.notes_list().len(),
        1,
        "a continuing outage must not re-alert"
    );
}

#[test]
fn sustained_outage_then_recovery_sends_one_each() {
    let mut seq = vec![D; 8];
    seq.extend(std::iter::repeat_n(OK, 6));
    let h = go(&seq);
    assert_eq!(alerts(&h.notes_list()).len(), 1);
    let r = recoveries(&h.notes_list());
    assert_eq!(r.len(), 1);
    assert!(r[0].contains("glm is back up"), "{}", r[0]);
}

#[test]
fn recovery_needs_two_passes_in_a_row() {
    let mut seq = vec![D; 4];
    seq.push(OK);
    assert!(recoveries(&go(&seq).notes_list()).is_empty());
    let mut seq = vec![D; 4];
    seq.extend([OK, OK]);
    assert_eq!(recoveries(&go(&seq).notes_list()).len(), 1);
}

#[test]
fn old_failures_do_not_reopen_after_recovery() {
    // after recovery the window is cleared: two stray failures afterwards are silent
    let h = go(&[D, D, D, OK, OK, D, OK, D, OK, OK]);
    assert_eq!(alerts(&h.notes_list()).len(), 1);
    assert_eq!(recoveries(&h.notes_list()).len(), 1);
}

#[test]
fn second_outage_after_recovery_alerts_again() {
    let h = go(&[D, D, D, OK, OK, OK, D, D, D]);
    assert_eq!(alerts(&h.notes_list()).len(), 2);
    assert_eq!(recoveries(&h.notes_list()).len(), 1);
}

#[test]
fn flapping_engine_alerts_exactly_once() {
    let h = go(&[D, OK, D, OK, D, OK]);
    assert_eq!(alerts(&h.notes_list()).len(), 1);
    assert!(recoveries(&h.notes_list()).is_empty());
    let flap: Vec<&str> = std::iter::repeat([D, OK].as_slice())
        .flatten()
        .take(20)
        .copied()
        .collect();
    let h = go(&flap);
    assert_eq!(alerts(&h.notes_list()).len(), 1);
    assert!(recoveries(&h.notes_list()).is_empty());
}

#[test]
fn two_engines_down_together_make_one_notification() {
    let both = "glm=down,deepseek=down";
    let h = go(&[both, both, both]);
    let notes = h.notes_list();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("deepseek, glm are down"), "{}", notes[0]);
}

#[test]
fn slow_busy_and_drift_do_not_count() {
    for status in ["slow", "skipped-busy", "drift"] {
        let line = format!("glm={status},deepseek=ok");
        let seq: Vec<&str> = std::iter::repeat_n(line.as_str(), 8).collect();
        assert_eq!(
            go(&seq).notes_list(),
            Vec::<String>::new(),
            "{status} must not count"
        );
    }
}

#[test]
fn wrong_counts_as_failed() {
    let h = go(&[
        "glm=wrong,deepseek=ok",
        "glm=wrong,deepseek=ok",
        "glm=wrong,deepseek=ok",
    ]);
    assert_eq!(alerts(&h.notes_list()).len(), 1);
}

#[test]
fn fleet_check_crash_is_logged_and_says_nothing() {
    let h = go(&[D, D, "CRASH", "CRASH", "EMPTY", "CRASH"]);
    assert_eq!(h.notes_list(), Vec::<String>::new());
    let hist = h.history();
    let results: Vec<&str> = hist.iter().map(|x| x["result"].as_str().unwrap()).collect();
    assert_eq!(
        results,
        ["fail", "fail", "error", "error", "error", "error"]
    );
    for x in &hist[2..] {
        let note = x["note"].as_str().unwrap();
        assert!(
            note.starts_with("fleet-check output was not usable (exit "),
            "{note}"
        );
    }
    // errors do not count: the third real failure still alerts, and only then
    h.set_seq(&[D]);
    h.run(1);
    assert_eq!(alerts(&h.notes_list()).len(), 1);
}

#[test]
fn history_has_one_line_per_run_with_results() {
    let h = go(&[OK, D, D, D, OK, OK]);
    let hist = h.history();
    assert_eq!(hist.len(), 6);
    let results: Vec<&str> = hist.iter().map(|x| x["result"].as_str().unwrap()).collect();
    assert_eq!(results, ["ok", "fail", "fail", "fail", "ok", "ok"]);
    assert_eq!(hist[1]["engines"], json!({"deepseek": "ok", "glm": "down"}));
    assert_eq!(hist[3]["events"], json!(["alert:glm"]));
    assert_eq!(hist[3]["open"], json!(["glm"]));
    assert_eq!(hist[5]["events"], json!(["recovered:glm"]));
    assert_eq!(hist[5]["open"], json!([]));
    for x in &hist {
        assert!(x.get("ts").is_some());
        // local ISO with offset, seconds precision — old Python lines look identical
        let ts = x["ts"].as_str().unwrap();
        assert_eq!(ts.len(), 25, "{ts}");
        assert_eq!(&ts[10..11], "T");
        assert!(&ts[19..20] == "+" || &ts[19..20] == "-", "{ts}");
        assert_eq!(&ts[22..23], ":");
    }
}

#[test]
fn history_ts_honours_the_tz_environment() {
    // The README's jq recipes string-compare Python and Rust stamps, so the
    // tick must write local wall time with the same ±HH:MM offset that
    // datetime.now().astimezone() produced — driven by TZ, like Python's.
    let h = Harness::new(&[OK]);
    let mut cmd = h.env_cmd(&["run"]);
    cmd.env("TZ", "America/Los_Angeles");
    assert!(cmd.output().unwrap().status.success());
    let ts = h.history()[0]["ts"].as_str().unwrap().to_string();
    // The tick consulted /bin/date +%z under this TZ; demand the same answer.
    let z = String::from_utf8_lossy(
        &Command::new("/bin/date")
            .arg("+%z")
            .env("TZ", "America/Los_Angeles")
            .env("LC_ALL", "C")
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    let expect = match z.as_str() {
        "-0700" => "-07:00",
        "-0800" => "-08:00",
        other => panic!("unexpected date output {other:?}"),
    };
    assert!(ts.ends_with(expect), "{ts} must end with {expect}");
    // UTC pins the zero form, independent of the machine's zone.
    let utc = Harness::new(&[OK]);
    let mut cmd = utc.env_cmd(&["run"]);
    cmd.env("TZ", "UTC");
    assert!(cmd.output().unwrap().status.success());
    let ts = utc.history()[0]["ts"].as_str().unwrap().to_string();
    assert!(ts.ends_with("+00:00"), "{ts} must be UTC-tagged");
}
#[test]
fn runs_fleet_check_quick_and_json() {
    let h = go(&[OK]);
    assert_eq!(
        fs::read_to_string(h.seq.parent().unwrap().join("seq.args"))
            .unwrap()
            .trim(),
        "--quick --json"
    );
}

#[test]
fn secrets_in_problem_text_are_masked_in_history_and_notice() {
    let h = go(&[D, D, D]);
    let mut text = fs::read_to_string(h.state_dir.join("history.jsonl")).unwrap();
    text.push('\n');
    text.push_str(&h.notes_list().join("\n"));
    assert!(!text.contains("hunter2hunter2"), "secret leaked");
    assert!(
        text.contains("[REDACTED:"),
        "masked by the real rules, not dropped"
    );
}

#[test]
fn failed_notification_is_retried_next_run() {
    let h = Harness::new(&[D, D, D, D, D]);
    fs::write(&h.fail_flag, "x").unwrap();
    h.run(3);
    assert_eq!(h.notes_list(), Vec::<String>::new());
    assert!(h.history().last().unwrap().get("notify_error").is_some());
    fs::remove_file(&h.fail_flag).unwrap();
    h.run(1);
    assert_eq!(h.notes_list().len(), 1);
    h.run(1);
    assert_eq!(h.notes_list().len(), 1);
}

#[test]
fn state_survives_between_runs() {
    let h = Harness::new(&[D, D, D]);
    h.run(2);
    assert_eq!(h.notes_list(), Vec::<String>::new());
    h.run(1); // a fresh process, same state dir
    assert_eq!(h.notes_list().len(), 1);
}

#[test]
fn python_written_state_keeps_counters_and_recovers() {
    // A state file in exactly the Python save_state shape (indent 1, sorted)
    // must carry the open alert forward and close it after two passes.
    let h = Harness::new(&[]);
    fs::create_dir_all(&h.state_dir).unwrap();
    let since = "2026-10-21T19:23:05-07:00";
    let state = format!(
        "{{\n \"engines\": {{\n  \"glm\": {{\n   \"open_since\": \"{since}\",\n   \"recent\": [\n    1,\n    1,\n    1\n   ]\n  }}\n }}\n}}"
    );
    fs::write(h.state_dir.join("state.json"), &state).unwrap();
    h.set_seq(&[OK, OK]);
    h.run(2);
    let notes = h.notes_list();
    assert_eq!(recoveries(&notes).len(), 1, "{notes:?}");
    assert!(notes[0].contains("glm since 7:23 PM"), "{}", notes[0]);
    let saved: Value =
        serde_json::from_str(&fs::read_to_string(h.state_dir.join("state.json")).unwrap()).unwrap();
    assert_eq!(saved["engines"]["glm"]["open_since"], json!(null));
    assert_eq!(saved["engines"]["glm"]["recent"], json!([]));
    assert!(
        !h.state_dir.join("state.tmp").exists(),
        "temp file must not linger"
    );
}

#[test]
fn could_not_run_is_an_error_entry_without_notifications() {
    let h = Harness::new(&[]);
    let mut cmd = h.env_cmd(&["run"]);
    cmd.env("FLEET_STOPGAP_CHECK", h.dir.join("no-such-binary"));
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let hist = h.history();
    assert_eq!(hist[0]["result"], json!("error"));
    let note = hist[0]["note"].as_str().unwrap();
    assert!(note.starts_with("could not run fleet-check: "), "{note}");
    // Divergence from the .py: std::io::ErrorKind name (NotFound) where
    // Python named the exception (FileNotFoundError).
    assert!(note.contains("NotFound"), "{note}");
    assert_eq!(hist[0]["exit"], json!(-1));
    assert_eq!(hist[0]["engines"], json!({}));
    assert_eq!(h.notes_list(), Vec::<String>::new());
}

// ------------------------------------------------------------------ redact subcommand

fn redact(args: &[&str], stdin_data: &[u8]) -> (String, String, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fleet-stopgap"))
        .arg("redact")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin_data).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn redact_masks_stdin_to_stdout() {
    let (out, err, code) = redact(
        &[],
        b"token ghp_AbCdEf123456GhIjKlMnOpQrStUvWx and password=hunter2hunter2",
    );
    assert_eq!(code, 0);
    assert_eq!(err, "");
    assert_eq!(
        out,
        "token [REDACTED:github-token] and password=[REDACTED:secret]"
    );
}

#[test]
fn redact_count_goes_to_stderr() {
    let (out, err, code) = redact(&["--count"], b"password=aaa111 and ghp_AbCdEf123456");
    assert_eq!(code, 0);
    assert!(out.contains("[REDACTED:secret]"), "{out}");
    assert_eq!(err, "2\n");
    let (_, err, _) = redact(&["--count"], b"nothing secret");
    assert_eq!(err, "0\n");
}

#[test]
fn redact_list_rules_and_max_chars() {
    let (out, _, code) = redact(&["--list-rules"], b"");
    assert_eq!(code, 0);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 31, "one line per rule");
    assert_eq!(lines[0], "pem\tpem");
    assert_eq!(lines[30], "mixed-token\ttoken");
    let long = "password=aaa111 xxxxx".repeat(20);
    let (masked, _, _) = redact(&[], long.as_bytes());
    let (cut, _, code) = redact(&["--max-chars", "10"], long.as_bytes());
    assert_eq!(code, 0);
    assert_eq!(cut, masked.chars().take(10).collect::<String>());
    assert_eq!(
        redact(&["--max-chars", "oops"], b"x").2,
        2,
        "argparse parity: bad int exits 2"
    );
}

#[test]
fn masking_corpus_matches_the_python_redactor() {
    // Input/expected pairs captured from lib/fleetlib/redact.py (see
    // tests/masking_corpus.json): every rule plus its near-misses.
    let corpus: Vec<Value> = serde_json::from_str(include_str!("masking_corpus.json")).unwrap();
    assert!(corpus.len() >= 15, "the corpus must pin every rule");
    let mut failures = Vec::new();
    for case in &corpus {
        let input = case["input"].as_str().unwrap();
        let expected = case["expected"].as_str().unwrap();
        let got = crate_redact(input);
        if got != expected {
            failures.push(format!(
                "rule {} input {input:?}\n  expected {expected:?}\n  got      {got:?}",
                case["rule"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} cases failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Redact in-process: same code the CLI runs, minus the process.
fn crate_redact(text: &str) -> String {
    let (out, _, code) = redact(&[], text.as_bytes());
    assert_eq!(code, 0);
    out
}

// ------------------------------------------------------------------ installer

#[cfg(target_os = "macos")]
#[test]
fn install_writes_plist_and_uninstall_removes_it() {
    let plist_dir = tempdir();
    let out = Command::new(env!("CARGO_BIN_EXE_fleet-stopgap"))
        .args([
            "install",
            "--plist-dir",
            plist_dir.to_str().unwrap(),
            "--no-launchctl",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        printed.starts_with(
            "installed com.jgavinray.fleet-stopgap: runs every 2 minutes; history in "
        ),
        "{printed}"
    );
    let plist = fs::read_to_string(plist_dir.join("com.jgavinray.fleet-stopgap.plist")).unwrap();
    // Same skeleton as stopgap/com.jgavinray.fleet-stopgap.plist.in, with the
    // interpreter pair replaced by this executable.
    assert!(plist.contains("<string>com.jgavinray.fleet-stopgap</string>"));
    assert!(plist.contains("<key>StartInterval</key>\n  <integer>120</integer>"));
    assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
    assert!(plist.contains("<key>ProcessType</key>\n  <string>Background</string>"));
    assert!(plist.contains("launchd.log"));
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_fleet-stopgap"))
        .canonicalize()
        .unwrap();
    assert!(
        plist.contains(&format!("<string>{}</string>", exe.display())),
        "{plist}"
    );
    let dir_of_exe = exe.parent().unwrap().to_string_lossy().into_owned();
    assert!(plist.contains(&format!(
        "<string>{dir_of_exe}:/usr/local/bin:/usr/bin:/bin</string>"
    )));

    let out = Command::new(env!("CARGO_BIN_EXE_fleet-stopgap"))
        .args([
            "uninstall",
            "--plist-dir",
            plist_dir.to_str().unwrap(),
            "--no-launchctl",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout)
        .starts_with("stopped and removed com.jgavinray.fleet-stopgap; history kept in "));
    assert!(!plist_dir.join("com.jgavinray.fleet-stopgap.plist").exists());
    // uninstall twice keeps the rm -f semantics of install.sh
    let out = Command::new(env!("CARGO_BIN_EXE_fleet-stopgap"))
        .args([
            "uninstall",
            "--plist-dir",
            plist_dir.to_str().unwrap(),
            "--no-launchctl",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
}

#[test]
fn help_covers_every_subcommand() {
    let out = Command::new(env!("CARGO_BIN_EXE_fleet-stopgap"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    for needle in [
        "run",
        "install",
        "uninstall",
        "redact",
        "FLEET_STOPGAP_CHECK",
        "FLEET_STOPGAP_NOTIFIER",
        "FLEET_STOPGAP_STATE_DIR",
    ] {
        assert!(text.contains(needle), "help lacks {needle}");
    }
    assert_eq!(
        Command::new(env!("CARGO_BIN_EXE_fleet-stopgap"))
            .arg("frobnicate")
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
}
