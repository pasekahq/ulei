//! A scripted Streamable HTTP MCP server on loopback TCP for the engine's tests. It answers
//! the handshake, `tools/list`, the resource and prompt requests and each `tools/call` by the
//! [`Reply`] scripted for its tool, and records every request. A test crate uses it with
//! `mod fake_gateway; mod fake_mcp;`.

#![allow(dead_code, reason = "each test crate uses its own part of the fake")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

use crate::fake_gateway::{Request, chunk, read_request};

/// Bounds every read and write of the fake, so a broken test fails instead of hanging.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounds the waits for the client, so a broken test fails instead of hanging.
const HANG: Duration = Duration::from_secs(60);

/// The session id the fake hands out first.
pub(crate) const SESSION_ID: &str = "fake-session-7";

/// The session id of generation `generation`: [`SESSION_ID`], then `fake-session-7-<n>`.
pub(crate) fn session_id(generation: usize) -> String {
    if generation == 0 {
        SESSION_ID.to_owned()
    } else {
        format!("{SESSION_ID}-{generation}")
    }
}

/// How the fake answers a `tools/call` of one tool.
#[derive(Clone)]
pub(crate) enum Reply {
    /// `200` `application/json` with this result.
    Json(Value),
    /// `200` chunked `text/event-stream`: a progress notification, a response to another
    /// request, then this result.
    Stream(Value),
    /// `200` `application/json` with this result under another request's id.
    Stray(Value),
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
    /// Forgets the session after answering this many `tools/call`s: later requests with its id
    /// get `404` and the next `initialize` starts the next generation.
    pub(crate) forget_after: Option<usize>,
    /// From this connection on, counted from 0, the fake leaves each connection unread until
    /// [`FakeMcp::listen`] or the drop of the fake, then reads it and records whether the client
    /// had closed it.
    pub(crate) deaf_from: Option<usize>,
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
            forget_after: None,
            deaf_from: None,
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
    /// The session generation; requests with an older id get `404`.
    generation: AtomicUsize,
    /// How many more `POST`s are dropped unanswered; `usize::MAX` drops all.
    down: AtomicUsize,
    connections: AtomicUsize,
    calls: AtomicUsize,
    /// Whether each deaf connection was closed by the client, in order.
    deaf: Mutex<Vec<bool>>,
    /// Whether deaf connections may be read.
    listening: Mutex<bool>,
    listened: Condvar,
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
            generation: AtomicUsize::new(0),
            down: AtomicUsize::new(0),
            connections: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            deaf: Mutex::new(Vec::new()),
            listening: Mutex::new(false),
            listened: Condvar::new(),
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

    /// Drops the next `n` `POST`s without an answer, as a server that went away mid-request;
    /// `usize::MAX` drops every one, `0` serves again.
    pub(crate) fn set_down(&self, n: usize) {
        self.shared.down.store(n, Ordering::SeqCst);
    }

    /// Lets the deaf connections be read, then tells whether the client had closed the first
    /// one.
    pub(crate) fn listen(&self) -> Option<bool> {
        *self.shared.listening.lock().expect("listening") = true;
        self.shared.listened.notify_all();
        let deaf = self.shared.deaf.lock().expect("deaf");
        let (deaf, _) = self
            .shared
            .arrived
            .wait_timeout_while(deaf, HANG, |deaf| deaf.is_empty())
            .expect("deaf");
        deaf.first().copied()
    }

    /// The JSON-RPC methods of the `POST`s so far, each with its session id.
    pub(crate) fn posts(&self) -> Vec<(String, Option<String>)> {
        self.seen()
            .iter()
            .filter(|s| s.target.starts_with("POST"))
            .map(|s| {
                (
                    s.method().to_owned(),
                    s.header("mcp-session-id").map(str::to_owned),
                )
            })
            .collect()
    }

    /// Every request received so far, in arrival order.
    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.shared.seen.lock().expect("seen").clone()
    }

    /// Blocks until `count` requests of JSON-RPC `method` arrived, at most [`HANG`]; whether
    /// they did.
    pub(crate) fn wait_for(&self, method: &str, count: usize) -> bool {
        let arrived = |seen: &Vec<Seen>| seen.iter().filter(|s| s.method() == method).count();
        let seen = self.shared.seen.lock().expect("seen");
        let (seen, _) = self
            .shared
            .arrived
            .wait_timeout_while(seen, HANG, |seen| arrived(seen) < count)
            .expect("seen");
        arrived(&seen) >= count
    }
}

impl Drop for FakeMcp {
    fn drop(&mut self) {
        *self.shared.stopping.lock().expect("stopping") = true;
        self.shared.wake.notify_all();
        *self.shared.listening.lock().expect("listening") = true;
        self.shared.listened.notify_all();
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
    let index = shared.connections.fetch_add(1, Ordering::SeqCst);
    if shared.spec.deaf_from.is_some_and(|from| index >= from) {
        let listening = shared.listening.lock().expect("listening");
        drop(
            shared
                .listened
                .wait_while(listening, |heard| !*heard)
                .expect("listening"),
        );
        let mut sink = vec![0_u8; 64 * 1024];
        let closed = loop {
            match stream.read(&mut sink) {
                Ok(0) => break true,
                Ok(_) => {}
                Err(_) => break false,
            }
        };
        shared.deaf.lock().expect("deaf").push(closed);
        shared.arrived.notify_all();
        return;
    }
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    let seen = parse(&request);
    shared.seen.lock().expect("seen").push(seen.clone());
    shared.arrived.notify_all();
    let down = shared.down.load(Ordering::SeqCst);
    if seen.target.starts_with("POST") && down > 0 {
        if down != usize::MAX {
            shared.down.store(down - 1, Ordering::SeqCst);
        }
        return;
    }
    if let Some(bytes) = answer(&seen, shared) {
        let _ = stream.write_all(&bytes);
    } else {
        let stopping = shared.stopping.lock().expect("stopping");
        let _unused = shared.wake.wait_while(stopping, |stop| !*stop);
    }
}

/// The answer to `seen`, `None` for a call that hangs.
fn answer(seen: &Seen, shared: &Shared) -> Option<Vec<u8>> {
    let spec = &shared.spec;
    let id = session_id(shared.generation.load(Ordering::SeqCst));
    let json_answer = |status, body: &Value| json_answer(status, &id, body);
    let verb = seen.target.split(' ').next().unwrap_or_default();
    if verb != "HEAD"
        && verb != "GET"
        && seen.method() != "initialize"
        && seen.header("mcp-session-id").is_some_and(|sent| sent != id)
    {
        return Some(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
    }
    let answer = match (verb, seen.method()) {
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
        ("POST", "tools/call") => return tools_call(seen, shared, &id),
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

/// The answer to the `tools/call` `seen` in session `id` by the reply scripted for its tool.
fn tools_call(seen: &Seen, shared: &Shared, id: &str) -> Option<Vec<u8>> {
    let spec = &shared.spec;
    let name = seen.body["params"]["name"].as_str().unwrap_or_default();
    let answer = match spec.calls.iter().find(|(tool, _)| tool == name) {
        Some((_, Reply::Json(value))) => json_answer(200, id, &result(&seen.body, value)),
        Some((_, Reply::Stream(value))) => event_stream(&seen.body, value),
        Some((_, Reply::Stray(value))) => {
            json_answer(200, id, &result(&json!({"id": 9999}), value))
        }
        Some((_, Reply::Hang)) => return None,
        None => json_answer(
            200,
            id,
            &error(&seen.body, &format!("Unknown tool: {name}")),
        ),
    };
    let calls = shared.calls.fetch_add(1, Ordering::SeqCst) + 1;
    if spec.forget_after == Some(calls) {
        shared.generation.fetch_add(1, Ordering::SeqCst);
    }
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

/// A `Content-Length` JSON answer that carries the session id `id`.
fn json_answer(status: u16, id: &str, body: &Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\nMcp-Session-Id: \
         {id}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    let stray = result(&json!({"id": 9999}), &json!({"content": []}));
    out.extend(chunk(format!("data: {stray}\n\n").as_bytes()));
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
