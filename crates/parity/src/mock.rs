//! Loopback mock engines, ported from `crates/parity.py`'s `Handler`.
//!
//! One `TcpListener` per engine on an ephemeral loopback port. Routes are
//! `(method, path)` exact matches; absent routes answer the same 404 body the
//! Python handler does. Bodies are pre-serialized so a mock can carry the bare
//! `NaN` / `-Infinity` tokens `json.dumps` writes (and that the Python tool
//! reads), which `serde_json` cannot serialize itself.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::Value;

use crate::scenario::{Scenario, SANITY_PROMPT};

/// A response: status, content type and the already-serialized body.
#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn text(status: u16, body: &'static str) -> Self {
        Self {
            status,
            content_type: "text/plain",
            body: body.as_bytes().to_vec(),
        }
    }

    /// `_send_bytes`: verbatim bytes, JSON content type.
    pub fn bytes(status: u16, body: &'static [u8]) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_vec(),
        }
    }
}

pub fn not_found() -> Reply {
    Reply::json(404, r#"{"error": "no route"}"#)
}

/// A predicate over the POST body, Python's `lambda b: ... if COND in b`.
#[derive(Clone, Copy)]
pub enum Predicate {
    GuidedChoice,
    LogprobsTruthy,
    PriorityPresent,
    PriorityBackground,
}

impl Predicate {
    fn holds(self, body: &Value) -> bool {
        match self {
            // "guided_choice" in b
            Predicate::GuidedChoice => body.get("guided_choice").is_some(),
            // b.get("logprobs")
            Predicate::LogprobsTruthy => body.get("logprobs").map(truthy).unwrap_or(false),
            // "priority" in b
            Predicate::PriorityPresent => body.get("priority").is_some(),
            // b.get("priority") == "background"
            Predicate::PriorityBackground => {
                body.get("priority").and_then(Value::as_str) == Some("background")
            }
        }
    }
}

/// Python truthiness, as used by `if b.get("logprobs")`.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// The chat behaviour of one engine: `ideal_chat`, a fixed answer, or one
/// overridden behaviour that falls back to another behaviour.
#[derive(Clone)]
pub enum Chat {
    Ideal,
    Fixed(Reply),
    When {
        predicate: Predicate,
        then: Reply,
        otherwise: Box<Chat>,
    },
}

impl Chat {
    pub fn reply(&self, body: &Value) -> Reply {
        match self {
            Chat::Ideal => ideal_chat(body),
            Chat::Fixed(reply) => reply.clone(),
            Chat::When {
                predicate,
                then,
                otherwise,
            } => {
                if predicate.holds(body) {
                    then.clone()
                } else {
                    otherwise.reply(body)
                }
            }
        }
    }
}

#[derive(Clone)]
pub enum Completions {
    IdealPlp,
    Fixed(Reply),
}

impl Completions {
    fn reply(&self) -> Reply {
        match self {
            Completions::IdealPlp => Reply::json(
                200,
                r#"{"prompt_logprobs": [{"token": " The", "logprob": -0.6}]}"#,
            ),
            Completions::Fixed(reply) => reply.clone(),
        }
    }
}

const OK_CHAT: &str = r#"{"choices": [{"message": {"content": "42"}}]}"#;
const CHAT_APPLE: &str = r#"{"choices": [{"message": {"content": "apple"}}]}"#;
const LOGPROBS_OK: &str = concat!(
    r#"{"choices": [{"message": {"content": "ok"}, "#,
    r#""logprobs": {"content": [{"token": "ok", "logprob": -0.01}]}}]}"#
);

pub fn ok_chat() -> Reply {
    Reply::json(200, OK_CHAT)
}

/// parity.py's `ideal_chat`: guided picks a listed choice, logprobs come back,
/// the sanity prompt gets 42, a background-priority probe is accepted, and
/// everything else gets 42 too.
fn ideal_chat(body: &Value) -> Reply {
    if body.get("guided_choice").is_some() {
        return Reply::json(200, CHAT_APPLE);
    }
    if body.get("logprobs").map(truthy).unwrap_or(false) {
        return Reply::json(200, LOGPROBS_OK);
    }
    let content = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.first())
        .and_then(|message| message.get("content"));
    if matches!(content, Some(Value::String(text)) if text == SANITY_PROMPT) {
        return ok_chat();
    }
    if body.get("priority").and_then(Value::as_str) == Some("background") {
        return Reply::json(200, r#"{"choices": [{"message": {"content": "ok"}}]}"#);
    }
    ok_chat()
}

/// One mock engine on a loopback port, stopped on drop.
pub struct MockEngine {
    pub url: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockEngine {
    pub fn start(scenario: Arc<Scenario>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a mock engine");
        let url = format!(
            "http://{}",
            listener.local_addr().expect("mock engine address")
        );
        listener
            .set_nonblocking(true)
            .expect("nonblocking mock listener");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
                        serve(&mut stream, &scenario);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for MockEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
    }
}

fn serve(stream: &mut TcpStream, scenario: &Scenario) {
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
    // json.loads(self.rfile.read(n) or b"{}")
    let json_body: Value = if body.is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_slice(&body).unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
    };

    let reply = route(&method, &path, &json_body, scenario);
    write_reply(stream, &reply);
}

fn route(method: &str, path: &str, body: &Value, scenario: &Scenario) -> Reply {
    match method {
        "GET" => match path {
            "/" => scenario.root.clone().unwrap_or_else(not_found),
            "/health" => scenario.health.clone().unwrap_or_else(not_found),
            "/v1/models" => scenario
                .models
                .clone()
                .unwrap_or_else(|| Reply::json(404, r#"{"error": "none"}"#)),
            "/v1/opinion/specs" => scenario.opinion_specs.clone().unwrap_or_else(not_found),
            "/ready" => scenario.ready.clone().unwrap_or_else(not_found),
            "/healthz" => scenario
                .healthz
                .clone()
                .unwrap_or_else(|| Reply::text(404, "no route")),
            _ => not_found(),
        },
        "POST" => match path {
            "/v1/chat/completions" => {
                if let Some(raw) = scenario.chat_raw {
                    return Reply::bytes(200, raw);
                }
                match &scenario.chat {
                    Some(chat) => chat.reply(body),
                    None => not_found(),
                }
            }
            "/v1/completions" => match &scenario.completions {
                Some(completions) => completions.reply(),
                None => not_found(),
            },
            "/v1/systemone" => scenario.systemone.clone().unwrap_or_else(not_found),
            "/v1/spans/credentials" => scenario.spans.clone().unwrap_or_else(not_found),
            _ => not_found(),
        },
        _ => not_found(),
    }
}

fn write_reply(stream: &mut TcpStream, reply: &Reply) {
    let reason = match reply.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reason,
        reply.content_type,
        reply.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&reply.body);
    let _ = stream.flush();
}

/// The refused engine's port, held bound but never listened on (parity.py
/// lines 307-309): connects get RST, and no mock can silently take the port.
pub struct HeldPort {
    fd: libc::c_int,
    pub port: u16,
}

impl HeldPort {
    pub fn new() -> Self {
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "socket for the refused port");
            let mut address: libc::sockaddr_in = std::mem::zeroed();
            address.sin_family = libc::AF_INET as libc::sa_family_t;
            address.sin_port = 0;
            address.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
            let rc = libc::bind(
                fd,
                &address as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            );
            assert_eq!(rc, 0, "bind the refused port");
            let mut length = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            let rc = libc::getsockname(
                fd,
                &mut address as *mut libc::sockaddr_in as *mut libc::sockaddr,
                &mut length,
            );
            assert_eq!(rc, 0, "read the refused port");
            Self {
                fd,
                port: u16::from_be(address.sin_port),
            }
        }
    }
}

impl Drop for HeldPort {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}
