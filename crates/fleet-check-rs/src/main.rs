//! `fleet-check-rs`: the Rust port of `check/fleet_check.py`.
//!
//! Parse the command line, load the registry (with the local LAN override),
//! check every selected engine, and print either the table or the JSON
//! document. Exit codes are Python's: 0 all good, 1 an engine down/wrong/slow
//! or a usage/registry error, 2 feature drift only.

mod cli;
mod json;
mod table;

use std::io::Write;

use fleet_core::check_engine::check_engine;
use fleet_core::registry::registry_path;
use fleet_core::{exit_code, CheckResult};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    std::process::exit(run(&args, &mut out, &mut err));
}

/// The whole run, with injectable streams so it can be exercised without
/// touching the process's stdout, stderr or exit status.
pub fn run(args: &[String], out: &mut impl Write, err: &mut impl Write) -> i32 {
    let opts = match cli::parse(args) {
        Ok(opts) => opts,
        Err(cli::ParseError::Help) => {
            let _ = out.write_all(cli::HELP.as_bytes());
            return 0;
        }
        Err(cli::ParseError::Usage(message)) => {
            let _ = writeln!(err, "{}", cli::USAGE);
            let _ = writeln!(err, "fleet-check: {message}");
            return 1;
        }
    };

    let path = registry_path(opts.registry.as_deref());
    let registry = match cli::load_registry_at(&path) {
        Ok(registry) => registry,
        Err(message) => {
            let _ = writeln!(err, "fleet-check: {message}");
            return 1;
        }
    };

    let names = match cli::select_engines(&registry, &opts.engines) {
        Ok(names) => names,
        Err(message) => {
            let _ = writeln!(err, "fleet-check: {message}");
            return 1;
        }
    };

    let results: Vec<CheckResult> = names
        .iter()
        .map(|name| {
            let cfg = registry
                .engine(name)
                .expect("selected engines come from the registry");
            check_engine(name, cfg, &opts)
        })
        .collect();

    let code = exit_code(&results);
    if opts.json {
        let _ = writeln!(out, "{}", json::render(&results, code));
    } else {
        let _ = writeln!(
            out,
            "{}\n\n{} (exit {})",
            table::render_table(&results, opts.quick),
            table::summary(code),
            code
        );
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry(tag: &str, text: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("fleet-check-rs-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("fleet.toml");
        std::fs::write(&path, text).expect("write registry");
        (dir, path)
    }

    fn run_args(args: &[&str]) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            &args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).expect("utf-8 stdout"),
            String::from_utf8(err).expect("utf-8 stderr"),
        )
    }

    #[test]
    fn unknown_engine_exits_one_with_a_message() {
        let (dir, path) = temp_registry(
            "unknown-engine",
            "[engines.alpha]\nurl = \"http://127.0.0.1:1\"\nmodel = \"m\"\nkind = \"tensorfold\"\n",
        );
        let (code, out, err) = run_args(&["--registry", path.to_str().unwrap(), "nope"]);
        assert_eq!(code, 1);
        assert_eq!(out, "");
        assert_eq!(err, "fleet-check: unknown engine nope; known: alpha\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_registry_exits_one_with_a_message() {
        let dir =
            std::env::temp_dir().join(format!("fleet-check-rs-no-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("missing.toml");
        let (code, out, err) = run_args(&["--registry", path.to_str().unwrap()]);
        assert_eq!(code, 1);
        assert_eq!(out, "");
        assert!(
            err.starts_with(&format!(
                "fleet-check: cannot read registry {}",
                path.display()
            )),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn help_exits_zero_on_stdout_only() {
        let (code, out, err) = run_args(&["--help"]);
        assert_eq!(code, 0);
        assert!(out.starts_with("usage: fleet-check"), "{out}");
        assert_eq!(err, "");
    }

    #[test]
    fn usage_error_exits_one_with_usage_on_stderr() {
        let (code, out, err) = run_args(&["--no-such-flag"]);
        assert_eq!(code, 1);
        assert_eq!(out, "");
        assert!(err.starts_with(cli::USAGE), "{err}");
        assert!(err.ends_with("fleet-check: unrecognized arguments: --no-such-flag\n"));
    }
}
