//! The MCP Streamable HTTP endpoint `/mcp` (protocol 2025-11-25 and the handshake versions the
//! engine accepts): the `Host`, `Origin` and bearer checks, sessions, and the JSON-RPC methods
//! `initialize`, `ping`, `tools/list` and `tools/call` (ADR-0071 section 6).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read as _;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::Settings;
use crate::hive;
use crate::http::{self, Request, Response};
use crate::tools::{self, Call};

/// The endpoint's path.
pub const PATH: &str = "/mcp";
/// Protocol revisions the server speaks, oldest first; the last is offered by default.
pub const VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
/// The most sessions kept; a new one drops the oldest.
pub const MAX_SESSIONS: usize = 32;
/// The most connections served at once.
pub const MAX_CONNECTIONS: usize = 4;
/// The longest wait for one read or write of a connection.
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// What runs a hive CLI call: the real CLI, or a stand-in in tests.
pub type Runner = dyn Fn(&[String]) -> Result<hive::Exit, hive::Failure> + Send + Sync;

/// The shared state of the server.
pub struct Server {
    settings: Settings,
    sessions: Mutex<VecDeque<String>>,
    /// One CLI run at a time.
    cli: Mutex<()>,
    runner: Box<Runner>,
    connections: AtomicUsize,
}

impl Server {
    /// A server of `settings` that runs the hive's CLI it names.
    pub fn new(settings: Settings) -> Self {
        let (command, config, timeout) = (
            settings.command.clone(),
            settings.hive_config.clone(),
            settings.timeout,
        );
        let runner = Box::new(move |args: &[String]| hive::run(&command, args, &config, timeout));
        Self::with_runner(settings, runner)
    }

    /// A server of `settings` whose CLI runs go to `runner`.
    pub fn with_runner(settings: Settings, runner: Box<Runner>) -> Self {
        Self {
            settings,
            sessions: Mutex::new(VecDeque::new()),
            cli: Mutex::new(()),
            runner,
            connections: AtomicUsize::new(0),
        }
    }

    /// Serves `listener` for ever, one thread per connection, at most [`MAX_CONNECTIONS`].
    pub fn serve(self: &Arc<Self>, listener: &TcpListener) {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            if self.connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                self.connections.fetch_sub(1, Ordering::AcqRel);
                let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                let _ = Response::empty(503).write(&mut stream, false);
                continue;
            }
            let server = Arc::clone(self);
            let spawned = thread::Builder::new().spawn(move || {
                server.connection(stream);
                server.connections.fetch_sub(1, Ordering::AcqRel);
            });
            if spawned.is_err() {
                self.connections.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    fn connection(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        let (response, head_only) = match http::read_request(&mut stream) {
            Ok(request) => (self.handle(&request), request.method == "HEAD"),
            Err(Some(status)) => (Response::empty(status), false),
            Err(None) => return,
        };
        let _ = response.write(&mut stream, head_only);
    }

    /// The answer to one request.
    pub fn handle(&self, request: &Request) -> Response {
        if !request.header("host").is_some_and(loopback_host) {
            return Response::empty(403);
        }
        if let Some(origin) = request.header("origin")
            && !loopback_origin(origin)
        {
            return error_body(403, "Forbidden: Origin is not a loopback origin");
        }
        if !self.authorized(request.header("authorization")) {
            return Response::empty(401).with("WWW-Authenticate", "Bearer");
        }
        if request.path != PATH {
            return Response::empty(404);
        }
        match request.method.as_str() {
            "POST" => self.post(request),
            "DELETE" => self.delete(request),
            _ => Response::empty(405).with("Allow", "POST, DELETE"),
        }
    }

    fn authorized(&self, header: Option<&str>) -> bool {
        let Some(given) = header.and_then(|h| {
            let (scheme, token) = h.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
        }) else {
            return false;
        };
        let want = self.settings.token.as_bytes();
        let given = given.as_bytes();
        given.len() == want.len()
            && given
                .iter()
                .zip(want)
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }

    fn known(&self, id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|s| s == id)
    }

    fn delete(&self, request: &Request) -> Response {
        let Some(id) = request.header("mcp-session-id") else {
            return Response::empty(400);
        };
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        match sessions.iter().position(|s| s == id) {
            Some(at) => {
                sessions.remove(at);
                Response::empty(204)
            }
            None => Response::empty(404),
        }
    }

    fn post(&self, request: &Request) -> Response {
        let media = request
            .header("content-type")
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if media != "application/json" {
            return Response::empty(415);
        }
        if let Some(accept) = request.header("accept")
            && !(accept.contains("application/json") || accept.contains("*/*"))
        {
            return Response::empty(406);
        }
        if let Some(version) = request.header("mcp-protocol-version")
            && !VERSIONS.contains(&version)
        {
            return error_body(400, "Bad Request: unsupported MCP-Protocol-Version");
        }
        let Ok(message) = serde_json::from_slice::<Value>(&request.body) else {
            return Response::json(400, &rpc_error(&Value::Null, PARSE_ERROR, "Parse error"));
        };
        let Some(object) = message.as_object() else {
            return Response::json(
                400,
                &rpc_error(
                    &Value::Null,
                    INVALID_REQUEST,
                    "Invalid Request: one JSON-RPC message per POST",
                ),
            );
        };
        let method = object.get("method").and_then(Value::as_str);
        let id = object.get("id").cloned();
        if method == Some("initialize")
            && let Some(id) = id
        {
            return self.initialize(&id, object.get("params"));
        }
        let session = request.header("mcp-session-id");
        match session {
            None => return error_body(400, "Bad Request: Mcp-Session-Id header is required"),
            Some(s) if !self.known(s) => return error_body(404, "Session not found"),
            Some(_) => {}
        }
        let (Some(method), Some(id)) = (method, id) else {
            // A notification or a response.
            return Response::empty(202);
        };
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        let answer = match method {
            "ping" => rpc_result(&id, &json!({})),
            "tools/list" => rpc_result(&id, &tools::list_tools()),
            "tools/call" => self.call(&id, &params),
            _ => rpc_error(&id, METHOD_NOT_FOUND, "Method not found"),
        };
        Response::json(200, &answer)
    }

    fn initialize(&self, id: &Value, params: Option<&Value>) -> Response {
        let asked = params
            .and_then(|p| p.get("protocolVersion"))
            .and_then(Value::as_str);
        let version = asked
            .filter(|v| VERSIONS.contains(v))
            .unwrap_or(VERSIONS[VERSIONS.len() - 1]);
        let Some(session) = session_id() else {
            return Response::json(500, &rpc_error(id, -32603, "no session id"));
        };
        {
            let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            while sessions.len() >= MAX_SESSIONS {
                sessions.pop_front();
            }
            sessions.push_back(session.clone());
        }
        let result = json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "ulei-hive-mcp", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "The hive's tools: delegate work as a card (hive_create), follow it (hive_list, hive_show), steer it (hive_steer), cancel it (hive_cancel) or decide its dispute (hive_resolve), and claim, send or withdraw a due reminder or watchdog alert (hive_reminder).",
        });
        Response::json(200, &rpc_result(id, &result)).with("Mcp-Session-Id", session)
    }

    fn call(&self, id: &Value, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return rpc_error(id, INVALID_PARAMS, "tools/call needs a tool name");
        };
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
        let started = Instant::now();
        let Some(parsed) = tools::parse(name, &arguments) else {
            return rpc_error(
                id,
                INVALID_PARAMS,
                &format!("Unknown tool: {}", cut(name, 64)),
            );
        };
        let (outcome, card) = match parsed {
            Err(refusal) => (Err(refusal), None),
            Ok(call) => {
                let card = call.card().map(str::to_owned);
                (self.run(&call), card)
            }
        };
        let elapsed = started.elapsed().as_millis();
        let card = card.as_deref().unwrap_or("-");
        let result = match outcome {
            Ok(output) => {
                eprintln!("ulei-hive-mcp: {name} {card} ok {elapsed} ms");
                json!({
                    "content": [{"type": "text", "text": output.to_string()}],
                    "structuredContent": output,
                    "isError": false,
                })
            }
            Err(refusal) => {
                eprintln!("ulei-hive-mcp: {name} {card} error {elapsed} ms");
                json!({
                    "content": [{"type": "text", "text": refusal.0}],
                    "isError": true,
                })
            }
        };
        rpc_result(id, &result)
    }

    fn run(&self, call: &Call) -> Result<Value, tools::Refusal> {
        tools::admit(call, &self.settings)?;
        let args = tools::argv(call, &self.settings);
        let exit = {
            let _one = self.cli.lock().unwrap_or_else(PoisonError::into_inner);
            (self.runner)(&args)
        };
        match exit {
            Ok(exit) => tools::answer(call, &exit),
            Err(failure) => Err(tools::Refusal(failure.to_string())),
        }
    }
}

fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn rpc_result(id: &Value, result: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// An error status with a JSON-RPC error body without id, as the transport allows.
fn error_body(status: u16, message: &str) -> Response {
    Response::json(
        status,
        &json!({"jsonrpc": "2.0", "error": {"code": INVALID_REQUEST, "message": message}}),
    )
}

/// 32 hex digits from `/dev/urandom`.
fn session_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .ok()?;
    let mut text = String::with_capacity(32);
    for b in bytes {
        let _ = write!(text, "{b:02x}");
    }
    Some(text)
}

/// The host of `authority` (`host[:port]`), lowercase, brackets kept for IPv6.
fn host_of(authority: &str) -> String {
    let authority = authority.trim().to_ascii_lowercase();
    if authority.starts_with('[') {
        return authority
            .split_once(']')
            .map_or(authority.clone(), |(h, _)| format!("{h}]"));
    }
    authority
        .split_once(':')
        .map_or(authority.clone(), |(h, _)| h.to_owned())
}

/// Whether a `Host` value names a loopback host.
pub fn loopback_host(value: &str) -> bool {
    matches!(host_of(value).as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

/// Whether an `Origin` value is an `http` or `https` origin on a loopback host.
pub fn loopback_origin(value: &str) -> bool {
    let value = value.trim();
    let rest = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"));
    rest.is_some_and(|authority| !authority.contains(['/', '@']) && loopback_host(authority))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn hosts_and_origins() {
        for host in ["127.0.0.1:8740", "localhost", "LOCALHOST:1", "[::1]:8740"] {
            assert!(loopback_host(host), "{host}");
        }
        for host in ["evil.test", "127.0.0.1.evil.test:80", "", "[::2]:1"] {
            assert!(!loopback_host(host), "{host}");
        }
        for origin in [
            "http://localhost:3000",
            "https://127.0.0.1",
            "http://[::1]:9",
        ] {
            assert!(loopback_origin(origin), "{origin}");
        }
        for origin in [
            "http://evil.test",
            "null",
            "file://",
            "http://localhost.evil.test",
            "http://u@localhost",
        ] {
            assert!(!loopback_origin(origin), "{origin}");
        }
    }

    #[test]
    fn session_ids_are_32_hex_digits() {
        let id = session_id().unwrap();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(Some(id), session_id());
    }
}
