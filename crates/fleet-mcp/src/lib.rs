//! `fleet-mcp`: an MCP stdio server exposing the fleet registry check.
//!
//! The transport is newline-delimited JSON-RPC 2.0 on stdin/stdout, hand-rolled
//! over serde_json: no async runtime and no protocol crate. A `tools/call` for
//! `fleet_check` goes through [`fleet_check_rs::run`], the same function the
//! `fleet-check-rs` binary uses, so a tool report and a command-line run are
//! one artifact.
//!
//! Nothing but response lines ever reaches stdout; diagnostics go to stderr.

use std::io::{BufRead, Write};

use serde_json::{json, Map, Value};

use fleet_check_rs::run as run_check;

/// The tool this server publishes.
pub const TOOL_NAME: &str = "fleet_check";

/// The protocol revision echoed when a client omits `protocolVersion`.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// What one fleet-check run produced, with the process exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// Run the check exactly the way the `fleet-check-rs` binary does, capturing
/// both streams instead of writing to the process's.
pub fn run_cli(args: &[String]) -> RunOutput {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run_check(args, &mut stdout, &mut stderr);
    RunOutput {
        stdout: String::from_utf8(stdout).expect("the check writes UTF-8"),
        stderr: String::from_utf8(stderr).expect("the check writes UTF-8"),
        code,
    }
}

/// Serve requests from `input` until EOF. Each non-empty line is one JSON-RPC
/// message; each message that needs an answer gets exactly one response line on
/// `out`. `run` is the check itself, injectable so the protocol can be tested
/// without touching the registry or the network.
pub fn serve(
    input: impl BufRead,
    out: &mut impl Write,
    err: &mut impl Write,
    mut run: impl FnMut(&[String]) -> RunOutput,
) {
    for line in input.lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                let _ = writeln!(err, "fleet-mcp: stdin: {error}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&line, &mut run, err) {
            let _ = writeln!(out, "{response}");
            let _ = out.flush();
        }
    }
}

/// One message in, at most one rendered response out. `None` covers
/// notifications and id-less messages, which are never answered.
fn handle_line(
    line: &str,
    run: &mut impl FnMut(&[String]) -> RunOutput,
    err: &mut impl Write,
) -> Option<String> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(error) => {
            let _ = writeln!(err, "fleet-mcp: parse error: {error}");
            return Some(render(&error_response(Value::Null, -32700, "Parse error")));
        }
    };
    let Some(object) = message.as_object() else {
        let _ = writeln!(err, "fleet-mcp: ignoring a message that is not an object");
        return None;
    };
    let method = object.get("method").and_then(Value::as_str);
    if method.is_some_and(|method| method.starts_with("notifications/"))
        || !object.contains_key("id")
    {
        return None;
    }
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = method else {
        return Some(render(&error_response(id, -32600, "Invalid Request")));
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    let outcome = match method {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list_result()),
        "tools/call" => call_tool(&params, run),
        _ => Err((-32601, "Method not found".to_string())),
    };
    Some(match outcome {
        Ok(result) => render(&json!({"jsonrpc": "2.0", "id": id, "result": result})),
        Err((code, message)) => render(&error_response(id, code, &message)),
    })
}

fn render(message: &Value) -> String {
    serde_json::to_string(message).expect("JSON values serialize")
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}

/// Echo the client's protocol version, advertise the tool capability, and
/// identify the server.
fn initialize_result(params: &Value) -> Value {
    let protocol = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "fleet-mcp", "version": env!("CARGO_PKG_VERSION")},
    })
}

fn tools_list_result() -> Value {
    json!({"tools": [tool_definition()]})
}

/// The single tool, with the registry check's flags as optional arguments.
fn tool_definition() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "Run the fleet registry check: probe every engine in fleet.toml and return the machine-readable JSON report. Exit 0 means all good; exit 1 means an engine is down, wrong, or slow.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "engines": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Engine names to check; default is every engine in the registry",
                },
                "quick": {
                    "type": "boolean",
                    "description": "Skip the feature probes",
                },
                "force_shared": {
                    "type": "boolean",
                    "description": "Run sanity and probes on a shared engine even when it is busy",
                },
                "registry": {
                    "type": "string",
                    "description": "Path to fleet.toml; default resolves like the CLI",
                },
                "timeout": {
                    "type": "number",
                    "description": "Seconds per request (default 60)",
                },
                "bg_timeout": {
                    "type": "number",
                    "description": "Seconds a background-priority sanity request may wait before it is re-sent at normal priority (default 20)",
                },
            },
            "additionalProperties": false,
        },
    })
}

/// `tools/call`. A fleet-check exit code of 1 is a successful call that
/// reports problems: `isError` stays false and the report travels as text.
fn call_tool(
    params: &Value,
    run: &mut impl FnMut(&[String]) -> RunOutput,
) -> Result<Value, (i64, String)> {
    let Some(params) = params.as_object() else {
        return Err((
            -32602,
            "Invalid params: tools/call expects an object".to_string(),
        ));
    };
    match params.get("name").and_then(Value::as_str) {
        Some(TOOL_NAME) => {}
        Some(other) => return Err((-32602, format!("Unknown tool: {other}"))),
        None => {
            return Err((
                -32602,
                "Invalid params: tools/call needs a tool name".to_string(),
            ))
        }
    }
    let empty = Value::Object(Map::new());
    let arguments = params.get("arguments").unwrap_or(&empty);
    let args = tool_args(arguments).map_err(|message| (-32602, message))?;
    let output = run(&args);
    let text = if output.stdout.is_empty() {
        output.stderr
    } else {
        output.stdout
    };
    Ok(json!({
        "content": [{"type": "text", "text": text}],
        "isError": false,
    }))
}

/// Map the tool arguments onto the CLI's flags, in the CLI's accepted shape:
/// options first, engine names (positional) last. `--json` is always sent so
/// the text content is the machine-readable report.
fn tool_args(arguments: &Value) -> Result<Vec<String>, String> {
    let Some(arguments) = arguments.as_object() else {
        return Err("Invalid params: arguments must be an object".to_string());
    };
    for key in arguments.keys() {
        if !matches!(
            key.as_str(),
            "engines" | "quick" | "force_shared" | "registry" | "timeout" | "bg_timeout"
        ) {
            return Err(format!("Invalid params: unknown argument '{key}'"));
        }
    }
    let mut args = vec!["--json".to_string()];
    if let Some(value) = arguments.get("quick") {
        if bool_arg("quick", value)? {
            args.push("--quick".to_string());
        }
    }
    if let Some(value) = arguments.get("force_shared") {
        if bool_arg("force_shared", value)? {
            args.push("--force-shared".to_string());
        }
    }
    if let Some(value) = arguments.get("registry") {
        args.push("--registry".to_string());
        args.push(string_arg("registry", value)?);
    }
    if let Some(value) = arguments.get("timeout") {
        args.push("--timeout".to_string());
        args.push(number_arg("timeout", value)?);
    }
    if let Some(value) = arguments.get("bg_timeout") {
        args.push("--bg-timeout".to_string());
        args.push(number_arg("bg_timeout", value)?);
    }
    if let Some(value) = arguments.get("engines") {
        let Some(engines) = value.as_array() else {
            return Err("Invalid params: 'engines' must be an array of strings".to_string());
        };
        for engine in engines {
            let Some(engine) = engine.as_str() else {
                return Err("Invalid params: 'engines' must be an array of strings".to_string());
            };
            args.push(engine.to_string());
        }
    }
    Ok(args)
}

fn bool_arg(key: &str, value: &Value) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("Invalid params: '{key}' must be a boolean"))
}

fn string_arg(key: &str, value: &Value) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("Invalid params: '{key}' must be a string"))
}

/// The CLI takes its timeouts as Python float text; a JSON number's own text
/// round-trips (`30` -> "30", `2.5` -> "2.5").
fn number_arg(key: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::Number(number) => Ok(number.to_string()),
        _ => Err(format!("Invalid params: '{key}' must be a number")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{BufReader, Cursor, Read};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Serializes the tests that point FLEET_LOCAL at something.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Keep every registry load away from the developer's real local.toml, and
    /// hold the lock so a test that needs its own override cannot race.
    fn isolate_local_override() -> std::sync::MutexGuard<'static, ()> {
        let guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        std::env::set_var("FLEET_LOCAL", "/dev/null");
        guard
    }

    fn session(lines: &[&str], mut run: impl FnMut(&[String]) -> RunOutput) -> (String, String) {
        let input = lines.join("\n") + "\n";
        let mut out = Vec::new();
        let mut err = Vec::new();
        serve(Cursor::new(input), &mut out, &mut err, &mut run);
        (
            String::from_utf8(out).expect("utf-8 stdout"),
            String::from_utf8(err).expect("utf-8 stderr"),
        )
    }

    fn no_run(_args: &[String]) -> RunOutput {
        RunOutput {
            stdout: String::new(),
            stderr: String::new(),
            code: 0,
        }
    }

    fn response_lines(out: &str) -> Vec<Value> {
        out.lines()
            .map(|line| serde_json::from_str(line).expect("each response is one JSON line"))
            .collect()
    }

    fn temp_registry(tag: &str, text: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("fleet-mcp-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("fleet.toml");
        std::fs::write(&path, text).expect("write registry");
        (dir, path)
    }

    /// A loopback engine that answers `/health` with a healthy tensorfold body
    /// and `/v1/chat/completions` with the sanity answer "42".
    struct MockEngine {
        url: String,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl MockEngine {
        fn start() -> MockEngine {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
            let address = listener.local_addr().expect("local addr");
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => serve_connection(stream),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            });
            MockEngine {
                url: format!("http://{address}"),
                stop,
                handle: Some(handle),
            }
        }
    }

    impl Drop for MockEngine {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn serve_connection(mut stream: std::net::TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_string();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        if content_length > 0 {
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).ok();
        }
        let (status, payload) = match path.as_str() {
            "/health" => (200, r#"{"ok": true, "model_ids": ["m"]}"#.to_string()),
            "/v1/chat/completions" => (
                200,
                r#"{"choices": [{"message": {"content": "42"}}]}"#.to_string(),
            ),
            _ => (404, "{}".to_string()),
        };
        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        stream.write_all(response.as_bytes()).ok();
        stream.flush().ok();
    }

    #[test]
    fn initialize_echoes_the_client_protocol_version() {
        let (out, err) = session(
            &[
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
            ],
            no_run,
        );
        assert_eq!(err, "");
        let responses = response_lines(&out);
        assert_eq!(responses.len(), 1);
        let result = &responses[0]["result"];
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["capabilities"], json!({"tools": {}}));
        assert_eq!(result["serverInfo"]["name"], "fleet-mcp");
        assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn initialize_without_a_protocol_version_uses_the_default() {
        let (out, _) = session(
            &[r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}"#],
            no_run,
        );
        let responses = response_lines(&out);
        assert_eq!(
            responses[0]["result"]["protocolVersion"],
            DEFAULT_PROTOCOL_VERSION
        );
    }

    #[test]
    fn tools_list_returns_the_exact_tool_schema() {
        let (out, err) = session(
            &[r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#],
            no_run,
        );
        assert_eq!(err, "");
        let responses = response_lines(&out);
        assert_eq!(responses.len(), 1);
        let expected = json!({
            "tools": [{
                "name": "fleet_check",
                "description": "Run the fleet registry check: probe every engine in fleet.toml and return the machine-readable JSON report. Exit 0 means all good; exit 1 means an engine is down, wrong, or slow.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "engines": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Engine names to check; default is every engine in the registry",
                        },
                        "quick": {
                            "type": "boolean",
                            "description": "Skip the feature probes",
                        },
                        "force_shared": {
                            "type": "boolean",
                            "description": "Run sanity and probes on a shared engine even when it is busy",
                        },
                        "registry": {
                            "type": "string",
                            "description": "Path to fleet.toml; default resolves like the CLI",
                        },
                        "timeout": {
                            "type": "number",
                            "description": "Seconds per request (default 60)",
                        },
                        "bg_timeout": {
                            "type": "number",
                            "description": "Seconds a background-priority sanity request may wait before it is re-sent at normal priority (default 20)",
                        },
                    },
                    "additionalProperties": false,
                },
            }],
        });
        assert_eq!(responses[0]["result"], expected);
    }

    #[test]
    fn tools_call_green_run_returns_the_report_json() {
        let _env = isolate_local_override();
        let mock = MockEngine::start();
        let (dir, path) = temp_registry(
            "green",
            &format!(
                "[engines.alpha]\nurl = \"{}\"\nmodel = \"m\"\nkind = \"tensorfold\"\n",
                mock.url
            ),
        );
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"fleet_check","arguments":{{"quick":true,"registry":"{}"}}}}}}"#,
            path.display()
        );
        let (out, err) = session(&[&request], run_cli);
        assert_eq!(err, "");
        let responses = response_lines(&out);
        assert_eq!(responses.len(), 1);
        let result = &responses[0]["result"];
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().expect("text content");
        assert_eq!(result["content"][0]["type"], "text");
        let report: Value = serde_json::from_str(text).expect("text is the report JSON");
        assert_eq!(report["exit_code"], 0);
        assert_eq!(report["engines"][0]["engine"], "alpha");
        assert_eq!(report["engines"][0]["verdict"], "ok");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tools_call_down_engine_returns_the_exit_one_report() {
        let _env = isolate_local_override();
        let (dir, path) = temp_registry(
            "down",
            "[engines.alpha]\nurl = \"http://127.0.0.1:1\"\nmodel = \"m\"\nkind = \"tensorfold\"\n",
        );
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{{"name":"fleet_check","arguments":{{"quick":true,"registry":"{}"}}}}}}"#,
            path.display()
        );
        let (out, err) = session(&[&request], run_cli);
        assert_eq!(err, "");
        let responses = response_lines(&out);
        let result = &responses[0]["result"];
        assert_eq!(
            result["isError"], false,
            "an engine being down is a successful check"
        );
        let text = result["content"][0]["text"].as_str().expect("text content");
        let report: Value = serde_json::from_str(text).expect("text is the report JSON");
        assert_eq!(report["exit_code"], 1);
        assert_eq!(report["engines"][0]["verdict"], "down");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tools_call_registry_error_reports_stderr_and_stays_is_error_false() {
        let _env = isolate_local_override();
        let dir =
            std::env::temp_dir().join(format!("fleet-mcp-no-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("missing.toml");
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{{"name":"fleet_check","arguments":{{"registry":"{}"}}}}}}"#,
            path.display()
        );
        let (out, err) = session(&[&request], run_cli);
        assert_eq!(err, "");
        let responses = response_lines(&out);
        let result = &responses[0]["result"];
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().expect("text content");
        assert!(
            text.starts_with(&format!(
                "fleet-check: cannot read registry {}",
                path.display()
            )),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn notifications_and_id_less_messages_produce_no_output() {
        let (out, err) = session(
            &[
                r#"{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
                r#"{"jsonrpc":"2.0","id":7,"method":"notifications/progress","params":{"progress":1}}"#,
                r#"{"jsonrpc":"2.0","method":"tools/list"}"#,
            ],
            no_run,
        );
        assert_eq!(out, "");
        assert_eq!(err, "");
    }

    #[test]
    fn unknown_method_is_minus_32601() {
        let (out, err) = session(
            &[r#"{"jsonrpc":"2.0","id":8,"method":"resources/list"}"#],
            no_run,
        );
        assert_eq!(err, "");
        let responses = response_lines(&out);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], 8);
        assert_eq!(responses[0]["error"]["code"], -32601);
        assert!(responses[0].get("result").is_none());
    }

    #[test]
    fn malformed_json_is_minus_32700_and_the_server_keeps_serving() {
        let (out, err) = session(
            &[
                r#"{"jsonrpc":"2.0","id":9,"method":"ping""#,
                r#"{"jsonrpc":"2.0","id":10,"method":"ping"}"#,
            ],
            no_run,
        );
        assert!(err.contains("parse error"), "{err}");
        let responses = response_lines(&out);
        assert_eq!(responses.len(), 2, "one error line, then the valid answer");
        assert_eq!(responses[0]["error"]["code"], -32700);
        assert_eq!(responses[0]["id"], Value::Null);
        assert_eq!(responses[1]["id"], 10);
        assert_eq!(responses[1]["result"], json!({}));
    }

    #[test]
    fn ping_returns_an_empty_result() {
        let (out, err) = session(&[r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#], no_run);
        assert_eq!(err, "");
        let responses = response_lines(&out);
        assert_eq!(responses[0]["jsonrpc"], "2.0");
        assert_eq!(responses[0]["id"], "abc");
        assert_eq!(responses[0]["result"], json!({}));
    }

    #[test]
    fn two_sequential_requests_get_two_responses_in_order() {
        let (out, err) = session(
            &[
                r#"{"jsonrpc":"2.0","id":11,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
                r#"{"jsonrpc":"2.0","id":12,"method":"tools/list"}"#,
            ],
            no_run,
        );
        assert_eq!(err, "");
        let responses = response_lines(&out);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["id"], 11);
        assert_eq!(responses[1]["id"], 12);
        assert_eq!(responses[1]["result"]["tools"][0]["name"], TOOL_NAME);
    }

    #[test]
    fn arguments_map_to_the_cli_flags_exactly() {
        let seen: std::cell::RefCell<Vec<Vec<String>>> = std::cell::RefCell::new(Vec::new());
        let (out, err) = session(
            &[
                r#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"fleet_check","arguments":{"engines":["alpha","beta"],"quick":true,"force_shared":true,"registry":"/tmp/x.toml","timeout":30,"bg_timeout":2.5}}}"#,
            ],
            |args| {
                seen.borrow_mut().push(args.to_vec());
                RunOutput {
                    stdout: "{\"exit_code\": 0}".to_string(),
                    stderr: String::new(),
                    code: 0,
                }
            },
        );
        assert_eq!(err, "");
        assert_eq!(
            seen.borrow().as_slice(),
            &[vec![
                "--json".to_string(),
                "--quick".to_string(),
                "--force-shared".to_string(),
                "--registry".to_string(),
                "/tmp/x.toml".to_string(),
                "--timeout".to_string(),
                "30".to_string(),
                "--bg-timeout".to_string(),
                "2.5".to_string(),
                "alpha".to_string(),
                "beta".to_string(),
            ]]
        );
        let responses = response_lines(&out);
        assert_eq!(
            responses[0]["result"]["content"][0]["text"],
            "{\"exit_code\": 0}"
        );
    }

    #[test]
    fn absent_arguments_send_only_json() {
        let seen: std::cell::RefCell<Vec<Vec<String>>> = std::cell::RefCell::new(Vec::new());
        let (out, _) = session(
            &[r#"{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"fleet_check"}}"#],
            |args| {
                seen.borrow_mut().push(args.to_vec());
                RunOutput {
                    stdout: String::new(),
                    stderr: "fleet-check: boom".to_string(),
                    code: 1,
                }
            },
        );
        assert_eq!(seen.borrow().as_slice(), &[vec!["--json".to_string()]]);
        let responses = response_lines(&out);
        assert_eq!(
            responses[0]["result"]["content"][0]["text"], "fleet-check: boom",
            "stderr stands in when stdout is empty"
        );
        assert_eq!(responses[0]["result"]["isError"], false);
    }

    #[test]
    fn stderr_stays_clean_of_json_during_a_session() {
        let (out, err) = session(
            &[
                r#"{"jsonrpc":"2.0","id":15,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                r#"{"jsonrpc":"2.0","id":16,"method":"ping"}"#,
                r#"{"jsonrpc":"2.0","id":17,"method":"tools/list"}"#,
                r#"{"jsonrpc":"2.0","id":18,"method":"tools/call","params":{"name":"fleet_check","arguments":{}}}"#,
            ],
            |_args| RunOutput {
                stdout: "{}".to_string(),
                stderr: String::new(),
                code: 0,
            },
        );
        assert_eq!(err, "", "protocol traffic never reaches stderr");
        assert_eq!(response_lines(&out).len(), 4);
    }

    #[test]
    fn tools_call_with_an_unknown_tool_name_is_invalid_params() {
        let (out, err) = session(
            &[r#"{"jsonrpc":"2.0","id":19,"method":"tools/call","params":{"name":"other_tool"}}"#],
            no_run,
        );
        assert_eq!(err, "");
        let responses = response_lines(&out);
        assert_eq!(responses[0]["error"]["code"], -32602);
        assert_eq!(responses[0]["error"]["message"], "Unknown tool: other_tool");
    }

    #[test]
    fn tools_call_rejects_unknown_or_mistyped_arguments() {
        for (id, arguments, needle) in [
            (20, r#"{"quick":"yes"}"#, "'quick' must be a boolean"),
            (
                21,
                r#"{"engines":"alpha"}"#,
                "'engines' must be an array of strings",
            ),
            (
                22,
                r#"{"engines":["alpha",7]}"#,
                "'engines' must be an array of strings",
            ),
            (23, r#"{"timeout":"60"}"#, "'timeout' must be a number"),
            (24, r#"{"registry":3}"#, "'registry' must be a string"),
            (25, r#"{"bogus":true}"#, "unknown argument 'bogus'"),
        ] {
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"fleet_check","arguments":{arguments}}}}}"#
            );
            let (out, _) = session(&[&request], no_run);
            let responses = response_lines(&out);
            let expected = format!("Invalid params: {needle}");
            assert_eq!(responses[0]["error"]["code"], -32602, "{arguments}");
            assert_eq!(
                responses[0]["error"]["message"].as_str(),
                Some(expected.as_str()),
                "{arguments}"
            );
        }
    }

    #[test]
    fn a_line_that_is_not_an_object_is_ignored() {
        let (out, err) = session(&["5", "[]"], no_run);
        assert_eq!(out, "");
        assert!(err.contains("not an object"), "{err}");
    }

    #[test]
    fn blank_lines_are_skipped() {
        let (out, err) = session(
            &["", "   ", r#"{"jsonrpc":"2.0","id":26,"method":"ping"}"#],
            no_run,
        );
        assert_eq!(err, "");
        let responses = response_lines(&out);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], 26);
    }

    #[test]
    fn a_request_without_a_method_is_invalid_request() {
        let (out, err) = session(&[r#"{"jsonrpc":"2.0","id":27}"#], no_run);
        assert_eq!(err, "");
        let responses = response_lines(&out);
        assert_eq!(responses[0]["error"]["code"], -32600);
    }
}
