//! A scripted Streamable HTTP MCP server on loopback TCP for the engine's tests. It answers
//! the handshake, `tools/list`, the resource and prompt requests and each `tools/call` by the
//! [`Reply`] scripted for its tool, and records every request. A test crate uses it with
//! `mod fake_gateway; mod fake_mcp;`.

#![allow(dead_code, reason = "each test crate uses its own part of the fake")]

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

use crate::fake_gateway::{Request, chunk, read_request};

/// Bounds every read and write of the fake, so a broken test fails instead of hanging.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// The session id the fake hands out.
pub(crate) const SESSION_ID: &str = "fake-session-7";

/// How the fake answers a `tools/call` of one tool.
#[derive(Clone)]
pub(crate) enum Reply {
    /// `200` `application/json` with this result.
    Json(Value),
    /// `200` chunked `text/event-stream`: a progress notification, then this result.
    Stream(Value),
    /// No answer until the fake drops.
    Hang,
}

/// What the fake serves.
#[derive(Clone)]
pub(crate) struct Spec {
    /// The `tools/list` items, served on two pages when there are more than two.
    pub(crate) tools: Vec<Value>,
    /// The reply to a call of each tool name; an unscripted tool answers a JSON-RPC error.
    pub(crate) calls: Vec<(String, Reply)>,
    /// The `capabilities` of the initialize answer.
    pub(crate) capabilities: Value,
    /// Answers `initialize` with `500` and this JSON-RPC error message instead.
    pub(crate) reject_initialize: Option<String>,
    /// Answers `HEAD` and `POST` with a `text/html` page, as a web app root does.
    pub(crate) web_page: bool,
}

impl Spec {
    /// Serves `tools`, advertising tools, resources and prompts.
    pub(crate) fn new(tools: Vec<Value>) -> Self {
        Self {
            tools,
            calls: Vec::new(),
            capabilities: json!({"tools": {}, "resources": {}, "prompts": {}}),
            reject_initialize: None,
            web_page: false,
        }
    }

    /// Answers calls of `tool` with `reply`.
    pub(crate) fn reply(mut self, tool: &str, reply: Reply) -> Self {
        self.calls.push((tool.to_owned(), reply));
        self
    }
}

/// One request as the fake read it.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    /// `METHOD /path`.
    pub(crate) target: String,
    /// Header lines after the request line, as sent.
    pub(crate) headers: Vec<(String, String)>,
    /// The JSON body, `Null` without one.
    pub(crate) body: Value,
}

impl Seen {
    /// The value of header `name`, any case.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The JSON-RPC method of the body, empty without one.
    pub(crate) fn method(&self) -> &str {
        self.body["method"].as_str().unwrap_or_default()
    }
}

struct Shared {
    spec: Spec,
    seen: Mutex<Vec<Seen>>,
    arrived: Condvar,
    stopping: Mutex<bool>,
    wake: Condvar,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

/// The running fake; dropping it ends every hanging answer.
pub(crate) struct FakeMcp {
    port: u16,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

impl FakeMcp {
    /// A fake serving `spec` on a free loopback port.
    pub(crate) fn start(spec: Spec) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake MCP server");
        let port = listener.local_addr().expect("fake address").port();
        let shared = Arc::new(Shared {
            spec,
            seen: Mutex::new(Vec::new()),
            arrived: Condvar::new(),
            stopping: Mutex::new(false),
            wake: Condvar::new(),
            workers: Mutex::new(Vec::new()),
        });
        let accept = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || accept_loop(&listener, &shared))
        };
        Self {
            port,
            shared,
            accept: Some(accept),
        }
    }

    /// The URL of the endpoint.
    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// Every request received so far, in arrival order.
    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.shared.seen.lock().expect("seen").clone()
    }

    /// Blocks until a request of JSON-RPC `method` arrived, at most [`IO_TIMEOUT`]; whether one
    /// did.
    pub(crate) fn wait_for(&self, method: &str) -> bool {
        let seen = self.shared.seen.lock().expect("seen");
        let (seen, _) = self
            .shared
            .arrived
            .wait_timeout_while(seen, IO_TIMEOUT, |seen| {
                !seen.iter().any(|s| s.method() == method)
            })
            .expect("seen");
        seen.iter().any(|s| s.method() == method)
    }
}

impl Drop for FakeMcp {
    fn drop(&mut self) {
        *self.shared.stopping.lock().expect("stopping") = true;
        self.shared.wake.notify_all();
        drop(TcpStream::connect(("127.0.0.1", self.port)));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let workers = std::mem::take(&mut *self.shared.workers.lock().expect("workers"));
        for worker in workers {
            let _ = worker.join();
        }
    }
}

fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if *shared.stopping.lock().expect("stopping") {
            return;
        }
        let Ok(stream) = stream else { continue };
        let worker = {
            let shared = Arc::clone(shared);
            thread::spawn(move || serve(stream, &shared))
        };
        shared.workers.lock().expect("workers").push(worker);
    }
}

fn serve(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    let seen = parse(&request);
    shared.seen.lock().expect("seen").push(seen.clone());
    shared.arrived.notify_all();
    if let Some(bytes) = answer(&seen, &shared.spec) {
        let _ = stream.write_all(&bytes);
    } else {
        let stopping = shared.stopping.lock().expect("stopping");
        let _unused = shared.wake.wait_while(stopping, |stop| !*stop);
    }
}

/// The answer to `seen`, `None` for a call that hangs.
fn answer(seen: &Seen, spec: &Spec) -> Option<Vec<u8>> {
    let answer = match (
        seen.target.split(' ').next().unwrap_or_default(),
        seen.method(),
    ) {
        ("HEAD" | "POST", _) if spec.web_page => page(),
        ("POST", "initialize") => match &spec.reject_initialize {
            Some(message) => json_answer(500, &error(&seen.body, message)),
            None => json_answer(
                200,
                &result(
                    &seen.body,
                    &json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": spec.capabilities,
                        "serverInfo": {"name": "fake", "version": "1"}
                    }),
                ),
            ),
        },
        ("POST", "notifications/initialized") => {
            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
        ("POST", "tools/list") => {
            let page = |items: &[Value], next: Option<&str>| {
                let mut page = json!({ "tools": items });
                if let Some(next) = next {
                    page["nextCursor"] = next.into();
                }
                json_answer(200, &result(&seen.body, &page))
            };
            match (seen.body["params"]["cursor"].as_str(), spec.tools.len() > 2) {
                (None, true) => page(&spec.tools[..2], Some("page-2")),
                (Some("page-2"), _) => page(&spec.tools[2..], None),
                _ => page(&spec.tools, None),
            }
        }
        ("POST", "tools/call") => {
            let name = seen.body["params"]["name"].as_str().unwrap_or_default();
            match spec.calls.iter().find(|(tool, _)| tool == name) {
                Some((_, Reply::Json(value))) => json_answer(200, &result(&seen.body, value)),
                Some((_, Reply::Stream(value))) => event_stream(&seen.body, value),
                Some((_, Reply::Hang)) => return None,
                None => json_answer(200, &error(&seen.body, &format!("Unknown tool: {name}"))),
            }
        }
        ("POST", "resources/list") => json_answer(
            200,
            &result(
                &seen.body,
                &json!({"resources": [
                    {"uri": "notes://today", "name": "today", "mimeType": "text/plain"},
                    {"uri": "notes://empty", "name": "empty", "description": ""}
                ]}),
            ),
        ),
        ("POST", "resources/read") => json_answer(
            200,
            &result(
                &seen.body,
                &json!({"contents": [
                    {"uri": seen.body["params"]["uri"], "text": "line one"},
                    {"uri": "notes://bin", "blob": "AAEC", "mimeType": "application/octet-stream"}
                ]}),
            ),
        ),
        ("POST", "prompts/list") => json_answer(
            200,
            &result(
                &seen.body,
                &json!({"prompts": [{
                    "name": "greet",
                    "description": "Greets",
                    "arguments": [{"name": "who", "required": true}]
                }]}),
            ),
        ),
        ("POST", "prompts/get") => json_answer(
            200,
            &result(
                &seen.body,
                &json!({"messages": [{
                    "role": "user",
                    "content": {"type": "text", "text": format!("Hello {}", seen.body["params"]["arguments"]["who"].as_str().unwrap_or("nobody"))}
                }]}),
            ),
        ),
        ("DELETE", _) => {
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
        _ => b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec(),
    };
    Some(answer)
}

fn parse(request: &Request) -> Seen {
    let head = String::from_utf8_lossy(&request.head);
    let mut lines = head.split("\r\n");
    let target = lines
        .next()
        .and_then(|l| l.rsplit_once(' '))
        .map_or(String::new(), |(t, _)| t.to_owned());
    let headers = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(n, v)| (n.to_owned(), v.to_owned()))
        .collect();
    let body = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    Seen {
        target,
        headers,
        body,
    }
}

fn result(request: &Value, result: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
}

fn error(request: &Value, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": message}})
}

/// A `Content-Length` JSON answer that carries the session id.
fn json_answer(status: u16, body: &Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\nMcp-Session-Id: \
         {SESSION_ID}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A chunked event stream of a progress notification, a priming event, then the response.
fn event_stream(request: &Value, value: &Value) -> Vec<u8> {
    let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: \
chunked\r\nConnection: close\r\n\r\n"
        .to_vec();
    let progress = json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": {"progressToken": 1, "progress": 0.5}
    });
    out.extend(chunk(
        format!("event: message\r\ndata: {progress}\r\n\r\n").as_bytes(),
    ));
    out.extend(chunk(b": keep-alive\n\nid: 1\ndata:\n\n"));
    let response = result(request, value).to_string();
    let (first, second) = response.split_at(response.len() / 2);
    out.extend(chunk(format!("data: {first}").as_bytes()));
    out.extend(chunk(format!("{second}\n\n").as_bytes()));
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

/// A web page, which is no MCP answer.
fn page() -> Vec<u8> {
    let body = "<html>hi</html>";
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}
