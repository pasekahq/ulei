//! The engine's gateway client against `qbd` and `qb gateway` built at the qb pin of
//! `ecosystem.lock`, with fake vendor endpoints, and each failure of those cases through the fake
//! gateway. The qb cases are ignored in the ordinary run; the CI job `qb-contract` runs them with
//! the binary named by `ULEI_QB_BIN`. Their fake vendor listens on 127.0.0.1:443, the only port
//! the gateway's sandbox connects to, so they need `net.ipv4.ip_unprivileged_port_start` <= 443.

mod fake_gateway;

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fake_gateway::{
    Answer, FakeGateway, Scratch, error_event, gateway_error, json_bytes, route, text,
};
use serde_json::{Value, json};
use ulei_contracts::generated::c4g::{
    CancelAnswer, Error as GatewayError, ErrorClass, EventType, StopReason, StreamEvent,
    TurnRequest, Usage,
};
use ulei_engine::gateway::{CANCEL_TIMEOUT, Client, Error};

/// Names the qb binary built at the pin.
const QB_BIN: &str = "ULEI_QB_BIN";
/// Where the fake vendor listens.
const VENDOR: &str = "127.0.0.1:443";
/// Bounds every wait for qb, the fake vendor and the engine's client.
const WAIT: Duration = Duration::from_secs(10);
/// `max_request_bytes` of the gateway: far below [`large_request`], so qb refuses it from its
/// head while the engine is still writing.
const MAX_REQUEST_BYTES: usize = 65_536;

/// Serializes the qb cases, each of which binds [`VENDOR`].
static VENDOR_PORT: Mutex<()> = Mutex::new(());

/// How a turn failed, as the engine's client yields it.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    /// HTTP status of a rejection; `None` for the `error` event that ends a `200` stream.
    status: Option<u16>,
    class: ErrorClass,
    retryable: bool,
    retry_after: Option<u64>,
    resets_at: Option<u64>,
}

impl Seen {
    fn of(status: Option<u16>, error: &GatewayError) -> Self {
        Self {
            status,
            class: error.class,
            retryable: error.retryable,
            retry_after: error.retry_after,
            resets_at: error.resets_at,
        }
    }
}

/// An `error` event that qb ends a `200` stream with.
const fn ended(class: ErrorClass, retryable: bool, retry_after: Option<u64>) -> Seen {
    Seen {
        status: None,
        class,
        retryable,
        retry_after,
        resets_at: None,
    }
}

/// A rejection qb answers before the turn starts.
const fn rejected(
    status: u16,
    class: ErrorClass,
    retryable: bool,
    retry_after: Option<u64>,
) -> Seen {
    Seen {
        status: Some(status),
        class,
        retryable,
        retry_after,
        resets_at: None,
    }
}

const AUTH: Seen = ended(ErrorClass::Auth, false, None);
const NO_ROUTE: Seen = ended(ErrorClass::NoRoute, false, None);
const CONTEXT_TOO_LONG: Seen = ended(ErrorClass::ContextTooLong, false, None);
/// A vendor overload on a pin route: the re-pick that avoids the vendor finds none, and that
/// error carries no `retry_after`.
const OVERLOADED_PIN: Seen = ended(ErrorClass::Overloaded, true, None);
/// A vendor overload with `retry-after: 5` on the last step of a chain route.
const OVERLOADED_CHAIN: Seen = ended(ErrorClass::Overloaded, true, Some(5));
const UNAVAILABLE: Seen = ended(ErrorClass::Unavailable, true, None);
const CANCELLED: Seen = ended(ErrorClass::Cancelled, false, None);
/// The gateway already runs `max_turns` turns.
const BUSY: Seen = rejected(429, ErrorClass::Overloaded, true, Some(1));
const UNKNOWN_ROUTE: Seen = rejected(400, ErrorClass::BadRequest, false, None);
const TOO_LARGE: Seen = rejected(413, ErrorClass::BadRequest, false, None);

fn quota_exhausted(resets_at: u64) -> Seen {
    Seen {
        resets_at: Some(resets_at),
        ..ended(ErrorClass::QuotaExhausted, true, None)
    }
}

/// How the turn of `items` failed: its only item, a rejection, or the `error` event that ends it.
fn seen(items: &[Result<StreamEvent, Error>]) -> Seen {
    match items.last() {
        Some(Err(Error::Rejected { status, error })) if items.len() == 1 => {
            Seen::of(Some(*status), error)
        }
        Some(Ok(StreamEvent {
            r#type: EventType::Error,
            error: Some(error),
            ..
        })) => Seen::of(None, error),
        _ => panic!("not a failed turn: {items:?}"),
    }
}

fn request(request_id: &str, route: &str) -> TurnRequest {
    serde_json::from_value(json!({
        "route": route,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "Say hello."}]}],
        "max_tokens": 64,
        "request_id": request_id,
    }))
    .expect("turn request")
}

/// A turn on `chat` that answers the result of the tool call `call_7-x` and offers `read`.
fn tool_request(request_id: &str) -> TurnRequest {
    serde_json::from_value(json!({
        "route": "chat",
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "Read the notes."}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_7-x", "name": "read", "input": {"path": "todo.txt"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "id": "call_7-x", "content": "no such file", "is_error": true}
            ]},
        ],
        "tools": [{
            "name": "read",
            "description": "Read a file.",
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
        }],
        "max_tokens": 64,
        "request_id": request_id,
    }))
    .expect("tool turn request")
}

/// [`request`] with a text of 8 MiB, far over [`MAX_REQUEST_BYTES`] and a socket's send buffer.
fn large_request(request_id: &str) -> TurnRequest {
    let mut request = request(request_id, "chat");
    request.messages[0].content[0].text = Some("x".repeat(8 << 20));
    request
}

/// Posts `POST /v1/model/cancel` for `request_id` to the gateway at `socket` as another caller
/// does, outside the engine's client: the status and body of the answer.
fn post_cancel(socket: &Path, request_id: &str) -> (u16, Vec<u8>) {
    let body = json!({ "request_id": request_id }).to_string();
    let mut stream = UnixStream::connect(socket).expect("connect for the cancel");
    stream.set_read_timeout(Some(WAIT)).expect("read timeout");
    write!(
        stream,
        "POST /v1/model/cancel HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("send the cancel");
    let mut answer = Vec::new();
    stream
        .read_to_end(&mut answer)
        .expect("read the cancel answer");
    let end = answer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("answer head");
    let head = String::from_utf8_lossy(&answer[..end]);
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status");
    (status, answer[end + 4..].to_vec())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs()
}

/// What the fake vendor answers to one request.
enum Reply {
    /// Status, headers and body, framed by `Content-Length`.
    Full(u16, Vec<(&'static str, String)>, Vec<u8>),
    /// A `200` event stream that sends these bytes, then stays silent until qb closes it.
    Hold(Vec<u8>),
}

/// One request the fake vendor read.
#[derive(Clone)]
struct Received {
    path: String,
    /// Header names in lowercase.
    headers: Vec<(String, String)>,
    body: Value,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        let found = self.headers.iter().find(|(n, _)| n == name);
        found.map(|(_, v)| v.as_str())
    }
}

/// A vendor API endpoint on [`VENDOR`] that answers requests with its [`Reply`]s in order and
/// with a 500 once they run out; dropping it closes the port.
struct Vendor {
    received: Arc<Mutex<Vec<Received>>>,
    /// One message each time qb closes a held stream.
    closed: Receiver<()>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Vendor {
    fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind(VENDOR).unwrap_or_else(|e| {
            panic!("bind {VENDOR}: {e}; set net.ipv4.ip_unprivileged_port_start to 443")
        });
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let received = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, closed) = channel();
        let accept = {
            let (received, stop) = (Arc::clone(&received), Arc::clone(&stop));
            thread::spawn(move || {
                for conn in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(conn) = conn else { continue };
                    let (replies, received, tx) =
                        (Arc::clone(&replies), Arc::clone(&received), tx.clone());
                    thread::spawn(move || serve_vendor(conn, &replies, &received, &tx));
                }
            })
        };
        Self {
            received,
            closed,
            stop,
            accept: Some(accept),
        }
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().expect("received").clone()
    }

    /// Whether qb closes a held stream within [`WAIT`].
    fn sees_a_close(&self) -> bool {
        self.closed.recv_timeout(WAIT).is_ok()
    }
}

impl Drop for Vendor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if TcpStream::connect(VENDOR).is_ok()
            && let Some(accept) = self.accept.take()
        {
            let _ = accept.join();
        }
    }
}

fn serve_vendor(
    mut conn: TcpStream,
    replies: &Mutex<VecDeque<Reply>>,
    received: &Mutex<Vec<Received>>,
    closed: &Sender<()>,
) {
    let _ = conn.set_read_timeout(Some(WAIT));
    let _ = conn.set_write_timeout(Some(WAIT));
    let Some(request) = read_vendor_request(&mut conn) else {
        return;
    };
    received.lock().expect("received").push(request);
    let reply = replies.lock().expect("replies").pop_front();
    match reply.unwrap_or(Reply::Full(500, Vec::new(), b"no reply scripted".to_vec())) {
        Reply::Full(status, headers, body) => {
            let mut out = format!("HTTP/1.1 {status} Reply\r\n");
            for (name, value) in headers {
                let _ = write!(out, "{name}: {value}\r\n");
            }
            let _ = write!(
                out,
                "content-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let mut bytes = out.into_bytes();
            bytes.extend_from_slice(&body);
            let _ = conn.write_all(&bytes);
        }
        Reply::Hold(prefix) => {
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
            let mut bytes = head.as_bytes().to_vec();
            bytes.extend_from_slice(&fake_gateway::chunk(&prefix));
            if conn.write_all(&bytes).is_err() {
                return;
            }
            match conn.read_to_end(&mut Vec::new()) {
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                _ => {
                    let _ = closed.send(());
                }
            }
        }
    }
}

/// Reads one request: its head and exactly `Content-Length` bytes of JSON body.
fn read_vendor_request(conn: &mut TcpStream) -> Option<Received> {
    let mut raw = Vec::new();
    let mut piece = [0u8; 4096];
    let end = loop {
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        match conn.read(&mut piece) {
            Ok(0) | Err(_) => return None,
            Ok(n) => raw.extend_from_slice(&piece[..n]),
        }
    };
    let head = String::from_utf8_lossy(&raw[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let path = lines.next()?.split(' ').nth(1)?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())?;
    let mut body = raw.split_off(end + 4);
    while body.len() < length {
        match conn.read(&mut piece) {
            Ok(0) | Err(_) => return None,
            Ok(n) => body.extend_from_slice(&piece[..n]),
        }
    }
    Some(Received {
        path,
        headers,
        body: serde_json::from_slice(&body).ok()?,
    })
}

/// The Messages event stream of `events`, each `data` under the event name of its `type`.
fn sse(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .flat_map(|e| {
            let name = e["type"].as_str().expect("event type");
            format!("event: {name}\ndata: {e}\n\n").into_bytes()
        })
        .collect()
}

/// `message_start` with 25 input tokens, 2000 read from and 100 written to the cache.
fn message_start() -> Value {
    json!({"type": "message_start", "message": {
        "id": "msg_contract_1", "type": "message", "role": "assistant", "model": "model-a",
        "content": [], "stop_reason": null, "stop_sequence": null,
        "usage": {"input_tokens": 25, "cache_creation_input_tokens": 100,
                  "cache_read_input_tokens": 2000, "output_tokens": 1},
    }})
}

fn block_start(index: u32, block: &Value) -> Value {
    json!({"type": "content_block_start", "index": index, "content_block": block})
}

fn block_delta(index: u32, delta: &Value) -> Value {
    json!({"type": "content_block_delta", "index": index, "delta": delta})
}

fn text_delta(index: u32, text: &str) -> Value {
    block_delta(index, &json!({"type": "text_delta", "text": text}))
}

fn block_stop(index: u32) -> Value {
    json!({"type": "content_block_stop", "index": index})
}

/// `message_delta` with `reason` and `output` tokens, then `message_stop`.
fn message_end(reason: &str, output: u64) -> [Value; 2] {
    [
        json!({"type": "message_delta", "delta": {"stop_reason": reason, "stop_sequence": null},
               "usage": {"output_tokens": output}}),
        json!({"type": "message_stop"}),
    ]
}

fn streamed(events: &[Value]) -> Reply {
    Reply::Full(
        200,
        vec![("content-type", "text/event-stream".to_owned())],
        sse(events),
    )
}

/// A Messages API error answer of `status` with `headers`.
fn vendor_error(
    status: u16,
    mut headers: Vec<(&'static str, String)>,
    kind: &str,
    message: &str,
) -> Reply {
    headers.push(("content-type", "application/json".to_owned()));
    let body = json!({"type": "error", "error": {"type": kind, "message": message}});
    Reply::Full(status, headers, body.to_string().into_bytes())
}

/// The 429 of a login past its five-hour limit, which resets at `resets_at`.
fn usage_limit(resets_at: u64) -> Reply {
    let reset = resets_at.to_string();
    let headers = vec![
        ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
        ("anthropic-ratelimit-unified-reset", reset.clone()),
        (
            "anthropic-ratelimit-unified-representative-claim",
            "five_hour".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-5h-status",
            "rejected".to_owned(),
        ),
        (
            "anthropic-ratelimit-unified-5h-utilization",
            "1.0".to_owned(),
        ),
        ("anthropic-ratelimit-unified-5h-reset", reset),
        (
            "retry-after",
            resets_at.saturating_sub(unix_now()).to_string(),
        ),
    ];
    vendor_error(
        429,
        headers,
        "rate_limit_error",
        "This request would exceed your account's rate limit. Please try again later.",
    )
}

fn overloaded_529() -> Reply {
    vendor_error(
        529,
        vec![("retry-after", "5".to_owned())],
        "overloaded_error",
        "Overloaded",
    )
}

/// The start of a text answer that the vendor then holds open.
fn held_text() -> Reply {
    Reply::Hold(sse(&[
        message_start(),
        block_start(0, &json!({"type": "text", "text": ""})),
        text_delta(0, "Partial"),
    ]))
}

/// `qbd` and `qb gateway` of the binary named by [`QB_BIN`] on fresh sockets, killed on drop.
/// qbd has `login-1` of `vendor-a` and `login-2` of `vendor-b`; the gateway sends both to the
/// fake vendor as Anthropic API keys and serves `chat`, a pin of `vendor-a`, and `chain`, a chain
/// of one step on `vendor-b`.
struct Qb {
    dir: Scratch,
    qbd: Child,
    gateway: Child,
}

impl Qb {
    fn start(max_turns: u32) -> Self {
        let bin = std::env::var_os(QB_BIN).map_or_else(
            || panic!("{QB_BIN} must name the qb binary built at the pin"),
            PathBuf::from,
        );
        assert!(bin.is_file(), "{QB_BIN}={} is not a file", bin.display());
        let dir = Scratch::new();
        let qbd_socket = dir.path("qb.sock");
        fs::write(dir.path("qbd.toml"), qbd_config()).expect("write qbd.toml");
        let mut qbd = spawn(
            Command::new(&bin)
                .arg("--socket")
                .arg(&qbd_socket)
                .args(["daemon", "--config"])
                .arg(dir.path("qbd.toml")),
            &dir.path("qbd.log"),
        );
        wait_for(&mut qbd, &qbd_socket, &dir.path("qbd.log"));
        for login in ["login-1", "login-2"] {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(dir.path(&format!("{login}.json")))
                .expect("create a credential file");
            let key = format!("synthetic-key-{login}");
            file.write_all(
                json!({"type": "api_key", "key": key})
                    .to_string()
                    .as_bytes(),
            )
            .expect("write a credential file");
        }
        fs::write(dir.path("gateway.toml"), gateway_config(&dir, max_turns))
            .expect("write gateway.toml");
        let mut gateway = spawn(
            Command::new(&bin)
                .args(["gateway", "--config"])
                .arg(dir.path("gateway.toml")),
            &dir.path("gateway.log"),
        );
        wait_for(&mut gateway, &dir.path("gw.sock"), &dir.path("gateway.log"));
        Self { dir, qbd, gateway }
    }

    fn socket(&self) -> PathBuf {
        self.dir.path("gw.sock")
    }

    fn client(&self) -> Client {
        Client::new(self.socket(), WAIT, WAIT)
    }

    /// Every item of a turn of `request`.
    fn turn(&self, request: &TurnRequest) -> Vec<Result<StreamEvent, Error>> {
        self.client()
            .turn(request)
            .expect("send the turn")
            .collect()
    }
}

impl Drop for Qb {
    fn drop(&mut self) {
        for child in [&mut self.gateway, &mut self.qbd] {
            let _ = child.kill();
            let _ = child.wait();
        }
        if thread::panicking() {
            for log in ["qbd.log", "gateway.log"] {
                let text = fs::read_to_string(self.dir.path(log)).unwrap_or_default();
                eprintln!("--- {log}\n{text}");
            }
        }
    }
}

fn spawn(command: &mut Command, log: &Path) -> Child {
    let log = File::create(log).expect("create a log");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("start the qb binary")
}

/// Waits until `socket` accepts a connection, at most [`WAIT`]; a `child` that exits first fails
/// with its log.
fn wait_for(child: &mut Child, socket: &Path, log: &Path) {
    let deadline = Instant::now() + WAIT;
    while UnixStream::connect(socket).is_err() {
        if let Some(status) = child.try_wait().expect("child status") {
            let text = fs::read_to_string(log).unwrap_or_default();
            panic!(
                "qb exited with {status} before {} came up:\n{text}",
                socket.display()
            );
        }
        assert!(
            Instant::now() < deadline,
            "{} did not come up",
            socket.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn qbd_config() -> String {
    let uid = rustix::process::geteuid().as_raw();
    format!(
        r#"[journal]
mode = "off"

[limits]
rate_per_s = 1000.0
burst = 1000

[[vendors]]
name = "vendor-a"
windows = [{{ name = "five_hour", length_s = 18000, t_burn_s = 1800 }}]

[[vendors]]
name = "vendor-b"
windows = [{{ name = "five_hour", length_s = 18000, t_burn_s = 1800 }}]

[[logins]]
name = "login-1"
vendor = "vendor-a"

[[logins]]
name = "login-2"
vendor = "vendor-b"

[[roles]]
name = "gateway"

[[roles.ladder]]
difficulty = 1
candidates = [
  {{ vendor = "vendor-a", model = "model-a", tier = 1 }},
  {{ vendor = "vendor-b", model = "model-b", tier = 1 }},
]

[[clients]]
name = "gateway"
uid = {uid}
role = "gateway"
"#
    )
}

fn gateway_config(dir: &Scratch, max_turns: u32) -> String {
    let path = |name: &str| dir.path(name).display().to_string();
    format!(
        r#"socket = "{socket}"
qbd_socket = "{qbd}"
max_turns = {max_turns}
max_request_bytes = {MAX_REQUEST_BYTES}

[[logins]]
name = "login-1"
kind = "api_key"
api = "anthropic"
base_url = "http://{VENDOR}"
credentials = "{login_1}"

[[logins]]
name = "login-2"
kind = "api_key"
api = "anthropic"
base_url = "http://{VENDOR}"
credentials = "{login_2}"

[[routes]]
alias = "chat"
role = "gateway"
vendor = "vendor-a"
model = "model-a"

[[routes]]
alias = "chain"
role = "gateway"
chain = [{{ vendor = "vendor-b", model = "model-b" }}]
"#,
        socket = path("gw.sock"),
        qbd = path("qb.sock"),
        login_1 = path("login-1.json"),
        login_2 = path("login-2.json"),
    )
}

/// Holds [`VENDOR_PORT`] for one qb case.
fn vendor_port() -> std::sync::MutexGuard<'static, ()> {
    VENDOR_PORT.lock().unwrap_or_else(PoisonError::into_inner)
}

fn events(items: Vec<Result<StreamEvent, Error>>) -> Vec<StreamEvent> {
    items.into_iter().map(|i| i.expect("event")).collect()
}

fn kinds(events: &[StreamEvent]) -> Vec<EventType> {
    events.iter().map(|e| e.r#type).collect()
}

fn usages(events: &[StreamEvent]) -> Vec<Usage> {
    events.iter().filter_map(|e| e.usage.clone()).collect()
}

fn usage(output: u64) -> Usage {
    Usage {
        input: 25,
        output,
        cache_read: 2000,
        cache_write: 100,
    }
}

/// The route event of `events`, which comes first, as `(vendor, model, login_label)`.
fn route_of(events: &[StreamEvent]) -> (String, String, String) {
    let route = events[0].route.clone().expect("route first");
    assert!(!route.lease.is_empty(), "{route:?}");
    (route.vendor, route.model, route.login_label)
}

fn on_login_1() -> (String, String, String) {
    ("vendor-a".into(), "model-a".into(), "login-1".into())
}

#[test]
#[ignore = "needs the qb binary of the pin; the CI job qb-contract runs it"]
fn qb_streams_a_text_turn_with_its_route_usage_and_stop() {
    let _port = vendor_port();
    let mut answer = vec![
        message_start(),
        block_start(0, &json!({"type": "text", "text": ""})),
        text_delta(0, "Hello"),
        text_delta(0, ", world"),
        block_stop(0),
    ];
    answer.extend(message_end("end_turn", 12));
    let vendor = Vendor::start(vec![streamed(&answer)]);
    let qb = Qb::start(4);

    let got = events(qb.turn(&request("turn-text", "chat")));
    assert_eq!(
        kinds(&got),
        [
            EventType::Route,
            EventType::Usage,
            EventType::TextDelta,
            EventType::TextDelta,
            EventType::Usage,
            EventType::Stop
        ],
        "{got:?}"
    );
    assert_eq!(route_of(&got), on_login_1());
    let texts: Vec<_> = got.iter().filter_map(|e| e.text.as_deref()).collect();
    assert_eq!(texts, ["Hello", ", world"]);
    assert_eq!(usages(&got), [usage(1), usage(12)]);
    assert_eq!(got[5].reason, Some(StopReason::End));

    let sent = vendor.received();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].path, "/v1/messages");
    assert_eq!(sent[0].header("x-api-key"), Some("synthetic-key-login-1"));
    assert_eq!(sent[0].body["model"], "model-a");
    assert_eq!(
        sent[0].body["messages"][0]["content"][0]["text"],
        "Say hello."
    );
}

#[test]
#[ignore = "needs the qb binary of the pin; the CI job qb-contract runs it"]
fn qb_passes_tool_call_ids_and_input_unchanged_and_stops_for_tool_use() {
    let _port = vendor_port();
    let call = json!({"type": "tool_use", "id": "toolu_01-Ab_9", "name": "read", "input": {}});
    let partial = |json: &str| json!({"type": "input_json_delta", "partial_json": json});
    let mut answer = vec![
        message_start(),
        block_start(0, &call),
        block_delta(0, &partial("")),
        block_delta(0, &partial("{\"path\": \"no")),
        block_delta(0, &partial("tes.txt\", \"lines\": [1, 2]}")),
        block_stop(0),
    ];
    answer.extend(message_end("tool_use", 48));
    let vendor = Vendor::start(vec![streamed(&answer)]);
    let qb = Qb::start(4);

    let got = events(qb.turn(&tool_request("turn-tool")));
    assert_eq!(
        kinds(&got),
        [
            EventType::Route,
            EventType::Usage,
            EventType::ToolCall,
            EventType::Usage,
            EventType::Stop
        ],
        "{got:?}"
    );
    assert_eq!(route_of(&got), on_login_1());
    let call = got[2].tool_call.clone().expect("tool call");
    assert_eq!(
        (call.id.as_str(), call.name.as_str()),
        ("toolu_01-Ab_9", "read")
    );
    assert_eq!(
        Value::Object(call.input),
        json!({"path": "notes.txt", "lines": [1, 2]})
    );
    assert_eq!(usages(&got), [usage(1), usage(48)]);
    assert_eq!(got[4].reason, Some(StopReason::ToolUse));

    let sent = vendor.received();
    assert_eq!(sent.len(), 1);
    let blocks: Vec<&Value> = sent[0].body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|m| m["content"].as_array().expect("content"))
        .collect();
    let id_of = |kind: &str, field: &str| {
        let block = blocks.iter().find(|b| b["type"] == kind);
        block.map(|b| b[field].clone())
    };
    assert_eq!(
        id_of("tool_use", "id"),
        Some(json!("call_7-x")),
        "{blocks:?}"
    );
    assert_eq!(
        id_of("tool_result", "tool_use_id"),
        Some(json!("call_7-x")),
        "{blocks:?}"
    );
    assert_eq!(sent[0].body["tools"][0]["name"], "read");
}

#[test]
#[ignore = "needs the qb binary of the pin; the CI job qb-contract runs it"]
fn qb_ends_a_turn_the_engine_cancels_and_closes_the_vendor_request() {
    let _port = vendor_port();
    let vendor = Vendor::start(vec![held_text()]);
    let qb = Qb::start(4);

    let mut turn = qb
        .client()
        .turn(&request("turn-cancel", "chat"))
        .expect("send the turn");
    let first: Vec<StreamEvent> = turn.by_ref().take(3).map(|i| i.expect("event")).collect();
    assert_eq!(
        kinds(&first),
        [EventType::Route, EventType::Usage, EventType::TextDelta]
    );
    assert_eq!(route_of(&first), on_login_1());

    let started = Instant::now();
    let answer = turn.cancel_handle().cancel();
    let took = started.elapsed();
    assert!(matches!(turn.next(), Some(Err(Error::Cancelled))));
    assert!(turn.next().is_none());
    match answer {
        Ok(answer) => assert_eq!(answer, CancelAnswer { cancelled: true }),
        Err(Error::Rejected { status: 404, error }) => {
            assert_eq!(error.class, ErrorClass::BadRequest);
        }
        Err(e) => panic!("cancel answered {e:?}"),
    }
    assert!(took <= CANCEL_TIMEOUT, "{took:?}");
    assert!(vendor.sees_a_close(), "qb did not close the vendor request");
}

#[test]
#[ignore = "needs the qb binary of the pin; the CI job qb-contract runs it"]
fn qb_refuses_a_turn_over_max_turns_and_ends_a_turn_another_caller_cancels() {
    let _port = vendor_port();
    let vendor = Vendor::start(vec![held_text()]);
    let qb = Qb::start(1);

    let mut turn = qb
        .client()
        .turn(&request("turn-held", "chat"))
        .expect("send the turn");
    let first: Vec<StreamEvent> = turn.by_ref().take(3).map(|i| i.expect("event")).collect();
    assert_eq!(
        kinds(&first),
        [EventType::Route, EventType::Usage, EventType::TextDelta]
    );
    assert_eq!(seen(&qb.turn(&request("turn-busy", "chat"))), BUSY);

    let (status, body) = post_cancel(&qb.socket(), "turn-held");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let answer: CancelAnswer = serde_json::from_slice(&body).expect("cancel answer");
    assert_eq!(answer, CancelAnswer { cancelled: true });
    let rest: Vec<_> = turn.collect();
    assert_eq!(rest.len(), 1, "{rest:?}");
    assert_eq!(seen(&rest), CANCELLED);
    assert!(vendor.sees_a_close(), "qb did not close the vendor request");
    assert_eq!(vendor.received().len(), 1);
}

#[test]
#[ignore = "needs the qb binary of the pin; the CI job qb-contract runs it"]
fn qb_ends_a_usage_limit_with_quota_exhausted_until_the_reset() {
    let _port = vendor_port();
    let resets_at = unix_now() + 3600;
    let vendor = Vendor::start(vec![usage_limit(resets_at)]);
    let qb = Qb::start(4);

    let got = qb.turn(&request("turn-quota", "chat"));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(seen(&got), quota_exhausted(resets_at));
    assert_eq!(vendor.received().len(), 1);
}

#[test]
#[ignore = "needs the qb binary of the pin; the CI job qb-contract runs it"]
fn qb_ends_a_rejected_key_with_auth_and_then_has_no_route() {
    let _port = vendor_port();
    let refused = vendor_error(401, Vec::new(), "authentication_error", "invalid x-api-key");
    let vendor = Vendor::start(vec![refused]);
    let qb = Qb::start(4);

    let got = qb.turn(&request("turn-auth", "chat"));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(seen(&got), AUTH);
    let got = qb.turn(&request("turn-dead", "chat"));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(seen(&got), NO_ROUTE);
    assert_eq!(vendor.received().len(), 1, "a dead login is not called");
}

#[test]
#[ignore = "needs the qb binary of the pin; the CI job qb-contract runs it"]
fn qb_ends_a_too_long_prompt_with_context_too_long() {
    let _port = vendor_port();
    let too_long = vendor_error(
        400,
        Vec::new(),
        "invalid_request_error",
        "prompt is too long: 1048577 tokens > 1048576 maximum",
    );
    let vendor = Vendor::start(vec![too_long]);
    let qb = Qb::start(4);

    let got = qb.turn(&request("turn-long", "chat"));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(seen(&got), CONTEXT_TOO_LONG);
    assert_eq!(vendor.received().len(), 1);
}

#[test]
#[ignore = "needs the qb binary of the pin; the CI job qb-contract runs it"]
fn qb_ends_a_vendor_overload_with_overloaded_on_a_pin_and_on_a_chain() {
    let _port = vendor_port();
    let vendor = Vendor::start(vec![overloaded_529(), overloaded_529()]);
    let qb = Qb::start(4);

    let got = qb.turn(&request("turn-pin", "chat"));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(seen(&got), OVERLOADED_PIN);
    let got = qb.turn(&request("turn-chain", "chain"));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(seen(&got), OVERLOADED_CHAIN);
    let keys: Vec<_> = vendor
        .received()
        .iter()
        .map(|r| r.header("x-api-key").map(str::to_owned))
        .collect();
    assert_eq!(
        keys,
        [
            Some("synthetic-key-login-1".to_owned()),
            Some("synthetic-key-login-2".to_owned())
        ]
    );
}

#[test]
#[ignore = "needs the qb binary of the pin; the CI job qb-contract runs it"]
fn qb_ends_a_vendor_failure_with_unavailable() {
    let _port = vendor_port();
    let html = vec![("content-type", "text/html".to_owned())];
    let failed = Reply::Full(500, html, b"<html>bad gateway</html>".to_vec());
    let vendor = Vendor::start(vec![failed]);
    let qb = Qb::start(4);

    let got = qb.turn(&request("turn-down", "chat"));
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(seen(&got), UNAVAILABLE);
    assert_eq!(vendor.received().len(), 1);
}

#[test]
#[ignore = "needs the qb binary of the pin; the CI job qb-contract runs it"]
fn qb_rejects_an_unknown_route_alias_and_a_body_over_max_request_bytes() {
    let _port = vendor_port();
    let vendor = Vendor::start(Vec::new());
    let qb = Qb::start(4);

    assert_eq!(
        seen(&qb.turn(&request("turn-alias", "nope"))),
        UNKNOWN_ROUTE
    );
    assert_eq!(seen(&qb.turn(&large_request("turn-large"))), TOO_LARGE);
    assert_eq!(vendor.received().len(), 0);
}

/// Every item of one turn of `request` against a fake answering `answer`.
fn fake_turn(answer: Answer, request: &TurnRequest) -> Vec<Result<StreamEvent, Error>> {
    let fake = FakeGateway::start(vec![answer]);
    Client::new(fake.socket(), WAIT, WAIT)
        .turn(request)
        .expect("send the turn")
        .collect()
}

#[test]
fn the_fake_gateway_classifies_each_qb_failure_as_qb_does() {
    let resets_at = unix_now() + 3600;
    let error = |error: GatewayError| Answer::Events(vec![error_event(error)]);
    let cases = [
        (
            error(GatewayError {
                resets_at: Some(resets_at),
                ..gateway_error(ErrorClass::QuotaExhausted)
            }),
            quota_exhausted(resets_at),
        ),
        (error(gateway_error(ErrorClass::Auth)), AUTH),
        (error(gateway_error(ErrorClass::NoRoute)), NO_ROUTE),
        (
            error(gateway_error(ErrorClass::ContextTooLong)),
            CONTEXT_TOO_LONG,
        ),
        (error(gateway_error(ErrorClass::Overloaded)), OVERLOADED_PIN),
        (
            error(GatewayError {
                retry_after: Some(5),
                ..gateway_error(ErrorClass::Overloaded)
            }),
            OVERLOADED_CHAIN,
        ),
        (error(gateway_error(ErrorClass::Unavailable)), UNAVAILABLE),
        (
            Answer::Reject(
                429,
                GatewayError {
                    retry_after: Some(1),
                    ..gateway_error(ErrorClass::Overloaded)
                },
            ),
            BUSY,
        ),
        (
            Answer::Reject(400, gateway_error(ErrorClass::BadRequest)),
            UNKNOWN_ROUTE,
        ),
    ];
    for (answer, want) in cases {
        let got = fake_turn(answer, &request("turn-fake", "chat"));
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(seen(&got), want);
    }
}

#[test]
fn the_fake_gateway_refuses_a_body_over_its_limit_from_the_head_as_qb_does() {
    let fake = FakeGateway::start(Vec::new());
    let body = serde_json::to_vec(&gateway_error(ErrorClass::BadRequest)).expect("error JSON");
    fake.answer_after_head(json_bytes(413, &body));
    let got: Vec<_> = Client::new(fake.socket(), WAIT, WAIT)
        .turn(&large_request("turn-large"))
        .expect("send the turn")
        .collect();
    assert_eq!(seen(&got), TOO_LARGE);
}

#[test]
fn the_fake_gateway_ends_a_turn_another_caller_cancels_as_qb_does() {
    let fake = FakeGateway::start(vec![Answer::Hang(vec![route(), text("Partial")])]);
    let mut turn = Client::new(fake.socket(), WAIT, WAIT)
        .turn(&request("turn-held", "chat"))
        .expect("send the turn");
    let first: Vec<StreamEvent> = turn.by_ref().take(2).map(|i| i.expect("event")).collect();
    assert_eq!(kinds(&first), [EventType::Route, EventType::TextDelta]);

    let (status, body) = post_cancel(fake.socket(), "turn-held");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let answer: CancelAnswer = serde_json::from_slice(&body).expect("cancel answer");
    assert_eq!(answer, CancelAnswer { cancelled: true });
    let rest: Vec<_> = turn.collect();
    assert_eq!(rest.len(), 1, "{rest:?}");
    assert_eq!(seen(&rest), CANCELLED);
}
