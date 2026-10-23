//! Library face of `fleet-check-rs`, the Rust port of `the original Python fleet-check`.
//!
//! Parse the command line, load the registry (with the local LAN override),
//! check every selected engine, and write either the table or the JSON
//! document. Exit codes are Python's: 0 all good, 1 an engine down/wrong/slow
//! or a usage/registry error, 2 feature drift only.
//!
//! The binary is a thin wrapper over [`run`], and `fleet-mcp` calls the same
//! function, so a tool call and a command-line run take one code path.

pub mod cli;
pub mod json;
pub mod table;

use std::io::Write;

use fleet_core::check_engine::check_engine;
use fleet_core::registry::registry_path;
use fleet_core::{exit_code, CheckResult};

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

    let mut results: Vec<CheckResult> = Vec::with_capacity(names.len());
    for name in &names {
        let cfg = registry
            .engine(name)
            .expect("selected engines come from the registry");
        let result = check_engine(name, cfg, &opts);
        // Python dies on an uncaught exception (a broken `model_ids` shape or
        // a non-string id list): stderr and exit 1, no report.
        if let Some(fatal) = result.fatal.clone() {
            let _ = writeln!(err, "fleet-check: {fatal}");
            return 1;
        }
        results.push(result);
    }

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

    /// Serializes the tests that point FLEET_LOCAL at something.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Keep every registry load away from the developer's real local.toml, and
    /// hold the lock so a test that needs its own override cannot race.
    fn isolate_local_override() -> std::sync::MutexGuard<'static, ()> {
        let guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        std::env::set_var("FLEET_LOCAL", "/dev/null");
        guard
    }

    /// Serve one GET /health answer with a fixed body, then close.
    fn serve_health_once(body: &'static str) -> String {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
        let address = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
            let mut stream = stream;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).ok();
            stream.flush().ok();
        });
        format!("http://{address}")
    }

    #[test]
    fn unknown_engine_exits_one_with_a_message() {
        let _env = isolate_local_override();
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
        let _env = isolate_local_override();
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

    #[test]
    fn dash_h_with_a_tail_prints_help() {
        let (code, out, err) = run_args(&["-hfoo"]);
        assert_eq!(code, 0);
        assert!(out.starts_with("usage: fleet-check"), "{out}");
        assert_eq!(err, "");
    }

    #[test]
    fn help_with_an_attached_value_is_a_usage_error() {
        let (code, out, err) = run_args(&["--help=1"]);
        assert_eq!(code, 1);
        assert_eq!(out, "");
        assert!(err.starts_with(cli::USAGE), "{err}");
        assert!(err.ends_with("fleet-check: argument -h/--help: ignored explicit argument '1'\n"));
    }

    #[test]
    fn broken_registry_shapes_exit_one_with_stderr() {
        let _env = isolate_local_override();
        for (tag, text) in [
            ("engines-int", "engines = 5\n"),
            ("engine-string", "[engines]\nfoo = \"x\"\n"),
            ("no-url", "[engines.foo]\nmodel = \"m\"\n"),
            ("no-model", "[engines.foo]\nurl = \"http://127.0.0.1:1\"\n"),
        ] {
            let (dir, path) = temp_registry(tag, text);
            let (code, out, err) = run_args(&["--registry", path.to_str().unwrap()]);
            assert_eq!(code, 1, "{tag}");
            assert_eq!(out, "", "{tag}");
            assert!(
                err.starts_with(&format!(
                    "fleet-check: cannot read registry {}",
                    path.display()
                )),
                "{tag}: {err}"
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn fatal_model_ids_exits_one_with_stderr() {
        let _env = isolate_local_override();
        let base = serve_health_once("{\"ok\": true, \"model_ids\": {\"a\": 1}}");
        let (dir, path) = temp_registry(
            "fatal-model-ids",
            &format!("[engines.e]\nurl = \"{base}\"\nmodel = \"m\"\nkind = \"tensorfold\"\n"),
        );
        let (code, out, err) = run_args(&["--registry", path.to_str().unwrap(), "--quick"]);
        assert_eq!(code, 1);
        assert_eq!(out, "", "Python prints no report after the crash");
        assert_eq!(err, "fleet-check: KeyError: 0\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn infinite_timeout_is_a_down_engine_not_a_panic() {
        let _env = isolate_local_override();
        let (dir, path) = temp_registry(
            "infinite-timeout",
            "[engines.e]\nurl = \"http://127.0.0.1:1\"\nmodel = \"m\"\nkind = \"tensorfold\"\n",
        );
        for timeout in ["inf", "1e400"] {
            let (code, out, err) = run_args(&[
                "--registry",
                path.to_str().unwrap(),
                "--quick",
                "--json",
                "--timeout",
                timeout,
            ]);
            assert_eq!(code, 1, "{timeout}");
            assert_eq!(err, "", "{timeout}");
            assert!(out.contains("\"verdict\": \"down\""), "{timeout}: {out}");
            assert!(out.contains("OverflowError"), "{timeout}: {out}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_port_in_a_local_rewrite_exits_one() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let local = std::env::temp_dir().join(format!(
            "fleet-check-rs-local-port-{}.toml",
            std::process::id()
        ));
        std::fs::write(&local, "[hosts.spark]\naddress = \"192.0.2.10\"\n").expect("write local");
        std::env::set_var("FLEET_LOCAL", &local);

        let (dir, path) = temp_registry(
            "bad-port",
            "[engines.e]\nurl = \"http://spark:abc/health\"\nmodel = \"m\"\nkind = \"tensorfold\"\n",
        );
        let (code, out, err) = run_args(&["--registry", path.to_str().unwrap()]);
        assert_eq!(code, 1);
        assert_eq!(out, "");
        assert!(
            err.contains("Port could not be cast to integer value as 'abc'"),
            "{err}"
        );

        std::fs::remove_file(&local).ok();
        std::fs::remove_dir_all(&dir).ok();
        std::env::set_var("FLEET_LOCAL", "/dev/null");
    }

    #[test]
    fn invalid_utf8_local_override_exits_one() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let local = std::env::temp_dir().join(format!(
            "fleet-check-rs-local-utf8-{}.toml",
            std::process::id()
        ));
        std::fs::write(&local, b"[hosts.spark]\naddress = \"\xff\"\n").expect("write local");
        std::env::set_var("FLEET_LOCAL", &local);

        let (dir, path) = temp_registry(
            "utf8-local",
            "[engines.e]\nurl = \"http://127.0.0.1:1\"\nmodel = \"m\"\nkind = \"tensorfold\"\n",
        );
        let (code, out, err) = run_args(&["--registry", path.to_str().unwrap()]);
        assert_eq!(code, 1);
        assert_eq!(out, "");
        assert!(
            err.starts_with("fleet-check: cannot read local override "),
            "{err}"
        );

        std::fs::remove_file(&local).ok();
        std::fs::remove_dir_all(&dir).ok();
        std::env::set_var("FLEET_LOCAL", "/dev/null");
    }
}
