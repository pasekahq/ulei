//! Scripted Bot API on `127.0.0.1:0`: records every request and replays queued replies.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// One scripted answer.
#[derive(Clone, Debug)]
pub(crate) enum Reply {
    /// HTTP status and JSON body.
    Json(u16, Value),
    /// HTTP 200 with raw bytes (a file download).
    Bytes(Vec<u8>),
    /// Close the connection without answering.
    Drop,
    /// Answer after a pause (a slow long poll or a stalled server).
    Delayed(Duration, Box<Reply>),
}

/// `{ok: true, result}` with HTTP 200.
pub(crate) fn ok(result: Value) -> Reply {
    let mut body = json!({ "ok": true });
    body["result"] = result;
    Reply::Json(200, body)
}

/// `{ok: false, error_code, description[, parameters]}` with the same HTTP status.
pub(crate) fn fail(status: u16, description: &str, parameters: Option<Value>) -> Reply {
    let mut body = json!({ "ok": false, "error_code": status, "description": description });
    if let Some(parameters) = parameters {
        body["parameters"] = parameters;
    }
    Reply::Json(status, body)
}

/// A request as the fake received it.
#[derive(Clone, Debug)]
pub(crate) struct Recorded {
    /// Request path, token included.
    pub(crate) path: String,
    /// Header names in lower case.
    pub(crate) headers: HashMap<String, String>,
    /// Raw body.
    pub(crate) body: Vec<u8>,
}

impl Recorded {
    /// Last path segment: the Bot API method, or the file name of a download.
    pub(crate) fn method(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or_default()
    }

    /// The body parsed as JSON.
    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("JSON request body")
    }
}

#[derive(Default)]
struct State {
    scripts: HashMap<String, VecDeque<Reply>>,
    requests: Vec<Recorded>,
    /// Updates Telegram holds for the bot, served by unscripted `getUpdates`.
    queue: Vec<Value>,
    /// Lowest `update_id` still unconfirmed, as Telegram keeps it between pollers.
    confirmed: i64,
    /// Pause before an unscripted `getUpdates` returns queued updates.
    poll_delay: Duration,
}

impl State {
    /// Telegram's queue: an `offset` confirms everything below it, `drop_pending_updates`
    /// discards the queue, and a poll returns what is left.
    fn queued_reply(&mut self, request: &Recorded) -> Reply {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        match request.method() {
            "getUpdates" => {
                if let Some(offset) = body.get("offset").and_then(Value::as_i64) {
                    self.confirmed = self.confirmed.max(offset);
                }
                let confirmed = self.confirmed;
                self.queue
                    .retain(|u| u["update_id"].as_i64().is_some_and(|id| id >= confirmed));
                if self.queue.is_empty() {
                    default_reply(request)
                } else {
                    let batch = ok(Value::Array(self.queue.clone()));
                    Reply::Delayed(self.poll_delay, Box::new(batch))
                }
            }
            "deleteWebhook" => {
                if body["drop_pending_updates"] == json!(true) {
                    self.queue.clear();
                }
                default_reply(request)
            }
            _ => default_reply(request),
        }
    }
}

/// The running fake; stops when dropped.
pub(crate) struct FakeBotApi {
    port: u16,
    state: Arc<Mutex<State>>,
    closing: Arc<AtomicBool>,
}

impl FakeBotApi {
    /// Starts the fake on a free loopback port.
    pub(crate) fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let state = Arc::new(Mutex::new(State::default()));
        let closing = Arc::new(AtomicBool::new(false));
        let (shared, stop) = (Arc::clone(&state), Arc::clone(&closing));
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let shared = Arc::clone(&shared);
                thread::spawn(move || serve(stream, &shared));
            }
        });
        Self {
            port,
            state,
            closing,
        }
    }

    /// Method base for `Config::base_url`.
    pub(crate) fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/bot", self.port)
    }

    /// File base for `Config::file_base_url`.
    pub(crate) fn file_base_url(&self) -> String {
        format!("http://127.0.0.1:{}/file/bot", self.port)
    }

    /// Queues `reply` for the next request whose last path segment is `method`.
    pub(crate) fn script(&self, method: &str, reply: Reply) {
        let mut state = self.state.lock().expect("fake state");
        state
            .scripts
            .entry(method.to_owned())
            .or_default()
            .push_back(reply);
    }

    /// Adds `updates` to the queue unscripted `getUpdates` calls serve.
    pub(crate) fn queue_updates(&self, updates: &[Value]) {
        let mut state = self.state.lock().expect("fake state");
        state.queue.extend_from_slice(updates);
    }

    /// Makes later unscripted `getUpdates` calls hold queued updates for `pause` before answering.
    pub(crate) fn delay_polls(&self, pause: Duration) {
        self.state.lock().expect("fake state").poll_delay = pause;
    }

    /// Every request so far, in arrival order.
    pub(crate) fn requests(&self) -> Vec<Recorded> {
        self.state.lock().expect("fake state").requests.clone()
    }

    /// Requests of one method so far.
    pub(crate) fn calls(&self, method: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method() == method)
            .collect()
    }
}

impl Drop for FakeBotApi {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve(stream: TcpStream, shared: &Mutex<State>) {
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut writer = writer;
    while let Some(request) = read_request(&mut reader) {
        let reply = {
            let mut state = shared.lock().expect("fake state");
            let scripted = state
                .scripts
                .get_mut(request.method())
                .and_then(VecDeque::pop_front);
            let reply = scripted.unwrap_or_else(|| state.queued_reply(&request));
            state.requests.push(request);
            reply
        };
        if !answer(&mut writer, reply) {
            let _ = writer.shutdown(Shutdown::Both);
            return;
        }
    }
}

/// Unscripted calls succeed; an unscripted long poll returns no updates after a moment.
fn default_reply(request: &Recorded) -> Reply {
    match request.method() {
        "getUpdates" => Reply::Delayed(Duration::from_millis(20), Box::new(ok(json!([])))),
        _ => ok(json!(true)),
    }
}

fn answer(writer: &mut TcpStream, reply: Reply) -> bool {
    let (status, body, kind) = match reply {
        Reply::Json(status, value) => (status, value.to_string().into_bytes(), "application/json"),
        Reply::Bytes(bytes) => (200, bytes, "application/octet-stream"),
        Reply::Drop => return false,
        Reply::Delayed(pause, inner) => {
            let until = Instant::now() + pause;
            thread::sleep(until.saturating_duration_since(Instant::now()));
            return answer(writer, *inner);
        }
    };
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    writer.write_all(head.as_bytes()).is_ok() && writer.write_all(&body).is_ok()
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Recorded> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let path = line.split_whitespace().nth(1)?.to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let body = if let Some(len) = headers.get("content-length") {
        let mut body = vec![0; len.parse().ok()?];
        reader.read_exact(&mut body).ok()?;
        body
    } else if headers
        .get("transfer-encoding")
        .is_some_and(|v| v.contains("chunked"))
    {
        read_chunked(reader)?
    } else {
        Vec::new()
    };
    Some(Recorded {
        path,
        headers,
        body,
    })
}

fn read_chunked(reader: &mut BufReader<TcpStream>) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size = String::new();
        reader.read_line(&mut size).ok()?;
        let size = usize::from_str_radix(size.trim(), 16).ok()?;
        let mut chunk = vec![0; size + 2];
        reader.read_exact(&mut chunk).ok()?;
        if size == 0 {
            return Some(body);
        }
        body.extend_from_slice(&chunk[..size]);
    }
}
