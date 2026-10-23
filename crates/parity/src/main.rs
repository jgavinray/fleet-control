//! `parity-rs`: replays crates/parity.py's mock-engine scenarios against the
//! release `fleet-check-rs` binary and compares each run to the goldens that
//! were recorded from the Python tool. No Python is executed at gate time.
//!
//! usage: parity-rs <path-to-fleet-check-binary> [--golden <dir>]

mod compare;
mod golden;
mod mock;
mod scenario;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

fn usage(message: &str) -> ! {
    eprintln!("parity-rs: {message}");
    eprintln!("usage: parity-rs <path-to-fleet-check-binary> [--golden <dir>]");
    exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut bin: Option<PathBuf> = None;
    let mut golden_dir: Option<PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--golden" {
            index += 1;
            match args.get(index) {
                Some(value) => golden_dir = Some(PathBuf::from(value)),
                None => usage("--golden needs a directory"),
            }
        } else if let Some(value) = arg.strip_prefix("--golden=") {
            golden_dir = Some(PathBuf::from(value));
        } else if arg.starts_with('-') {
            usage(&format!("unknown option {arg}"));
        } else if bin.is_none() {
            bin = Some(PathBuf::from(arg));
        } else {
            usage(&format!("unexpected argument {arg}"));
        }
        index += 1;
    }
    let bin = match bin {
        Some(bin) => bin,
        None => usage("missing the fleet-check-rs binary path"),
    };
    if !bin.is_file() {
        eprintln!("parity-rs: no binary at {}", bin.display());
        exit(2);
    }
    // Runs with a relative-registry cwd must still find the binary.
    let bin = fs::canonicalize(&bin).expect("canonicalize the fleet-check-rs binary");
    let golden_dir =
        golden_dir.unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("golden"));

    // Serve every scenario's mock engine on loopback; the refused engine's
    // port is held bound but never listened on (parity.py:307-309).
    let held = mock::HeldPort::new();
    let mut mocks: Vec<mock::MockEngine> = Vec::new();
    let mut entries: Vec<scenario::EngineEntry> = Vec::new();
    for scenario in scenario::scenarios() {
        let url = if scenario.refused {
            format!("http://127.0.0.1:{}", held.port)
        } else {
            let engine = mock::MockEngine::start(Arc::new(scenario.clone()));
            let url = engine.url.clone();
            mocks.push(engine);
            url
        };
        entries.push(scenario::EngineEntry::from_scenario(&scenario, url));
    }

    let dir = temp_dir();
    fs::create_dir_all(&dir).expect("create the parity temp dir");
    let registry = dir.join("fleet.toml");
    fs::write(&registry, scenario::registry_text(&entries)).expect("write the fixture registry");
    for (name, contents) in scenario::BAD_REGISTRIES {
        fs::write(dir.join(format!("bad-{name}.toml")), contents)
            .expect("write a malformed registry");
    }
    let urls: BTreeMap<String, String> = entries
        .iter()
        .map(|entry| (entry.name.to_string(), entry.url.clone()))
        .collect();

    let files = golden_files(&golden_dir);
    if files.is_empty() {
        eprintln!("parity-rs: no golden files under {}", golden_dir.display());
        exit(2);
    }

    let runner = compare::Runner {
        bin: &bin,
        registry: &registry,
        dir: &dir,
        urls: &urls,
    };
    let mut comparisons = 0usize;
    let mut problems: Vec<String> = Vec::new();
    for path in &files {
        let text = fs::read_to_string(path).expect("read a golden file");
        let golden: golden::GoldenFile = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("golden {} is not valid: {error}", path.display()));
        let mut scenario_problems: Vec<String> = Vec::new();
        for run in &golden.runs {
            comparisons += 1;
            scenario_problems.extend(compare::compare(run, &runner));
        }
        let modes: Vec<&str> = golden.runs.iter().map(|run| run.mode.as_str()).collect();
        if scenario_problems.is_empty() {
            println!("PASS  {} ({})", golden.scenario, modes.join(", "));
        } else {
            println!("FAIL  {} ({})", golden.scenario, modes.join(", "));
            problems.extend(scenario_problems);
        }
    }

    fs::remove_dir_all(&dir).ok();

    for problem in &problems {
        eprintln!("GATE FAIL (parity): {problem}");
    }
    println!(
        "parity: {comparisons} comparisons, {} problem(s)",
        problems.len()
    );
    if !problems.is_empty() {
        exit(1);
    }
}

/// Every golden file: one level of scenario files, then `cases/`, both sorted.
fn golden_files(golden_dir: &Path) -> Vec<PathBuf> {
    let mut files = json_files(golden_dir);
    files.sort();
    let cases = golden_dir.join("cases");
    if cases.is_dir() {
        let mut case_files = json_files(&cases);
        case_files.sort();
        files.extend(case_files);
    }
    files
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// A fresh temp dir; `parity.py` uses `tempfile.mkdtemp(prefix="fleetparity-")`.
fn temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("fleetparity-rs-{}-{nanos}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    dir
}
