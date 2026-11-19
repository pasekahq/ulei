//! `ulei-engine serve` started from the built binary in a scratch directory, and the C6 calls
//! on its socket as raw HTTP/1.1 bytes. A test crate uses it with
//! `mod fake_gateway; mod c6_wire;`.

#![allow(dead_code, reason = "each test crate uses its own part of the helpers")]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use ulei_contracts::generated::c6::{
    IdleStatus, InjectResponse, InterruptResponse, RouteSet, SessionStatus, SubmitTurnResponse,
    TurnEventsPage, TurnReport, TurnState,
};

use crate::fake_gateway::{Answer, FakeGateway, Scratch};

const BIN: &str = env!("CARGO_BIN_EXE_ulei-engine");
pub(crate) const OWNER: &str = "owner";

/// A running `ulei-engine serve`, killed on drop.
pub(crate) struct Engine {
    child: Child,
    pub(crate) socket: PathBuf,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The settings of a serve run in `dir` against `gateway`, its C6 socket at `socket`.
pub(crate) fn config(dir: &Path, gateway: &Path, socket: &Path) -> PathBuf {
    let brain = dir.join("brain");
    fs::create_dir_all(&brain).unwrap();
    fs::write(brain.join("SOUL.md"), "You are the test assistant.\n").unwrap();
    let text = format!(
        "[qb]\nsocket = \"{}\"\n\n[engine]\njournal = \"{}\"\nterminal_output = \"{}\"\nsoul_file = \"{}\"\nindex_file = \"{}\"\nmemory_file = \"{}\"\nuser_file = \"{}\"\nskills_dir = \"{}\"\nc6_socket = \"{}\"\nowner_session = \"{OWNER}\"\ndefault_route = \"main\"\n",
        gateway.display(),
        dir.join("journal.sqlite3").display(),
        dir.join("terminal-output").display(),
        brain.join("SOUL.md").display(),
        brain.join("INDEX.md").display(),
        brain.join("MEMORY.md").display(),
        brain.join("USER.md").display(),
        brain.join("skills").display(),
        socket.display(),
    );
    let path = dir.join("ulei.toml");
    fs::write(&path, text).unwrap();
    path
}

/// `ulei-engine <args>` in `dir` with an environment of `PATH` and `HOME` only.
pub(crate) fn command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir);
    command
}

/// Starts `serve` with the settings at `config` and waits for its ready line.
pub(crate) fn start(dir: &Path, config: &Path) -> Engine {
    let mut child = command(dir, &["serve", "--config", config.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let mut stderr = String::new();
    if line.is_empty() {
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
    }
    let socket = line
        .strip_prefix("ulei-engine ready ")
        .unwrap_or_else(|| panic!("no ready line: {stderr}"))
        .trim_end()
        .into();
    Engine { child, socket }
}

/// A gateway with `script` and a started engine in a fresh directory.
pub(crate) fn setup(script: Vec<Answer>) -> (FakeGateway, Scratch, Engine) {
    let fake = FakeGateway::start(script);
    let dir = Scratch::new();
    let config = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let engine = start(&dir.0, &config);
    (fake, dir, engine)
}

/// One answer as read from the wire.
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) head: String,
    pub(crate) body: Vec<u8>,
}

/// Sends `raw` on a fresh connection and reads the answer up to the end of the stream.
pub(crate) fn exchange(socket: &Path, raw: &[u8]) -> Response {
    let mut stream = connect(socket, raw);
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).unwrap();
    parse(&answer)
}

pub(crate) fn connect(socket: &Path, raw: &[u8]) -> UnixStream {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(raw).unwrap();
    stream
}

/// The answer to `raw`, which the server refuses before reading its last byte: closing with
/// unread bytes resets the stream after the answer.
pub(crate) fn exchange_refused(socket: &Path, raw: &[u8]) -> Response {
    let mut stream = connect(socket, raw);
    let mut answer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return parse(&answer),
            Err(_) if !answer.is_empty() => return parse(&answer),
            Ok(n) => answer.extend_from_slice(&chunk[..n]),
            Err(e) => panic!("read: {e}"),
        }
    }
}

pub(crate) fn parse(answer: &[u8]) -> Response {
    let end = answer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no head in {:?}", String::from_utf8_lossy(answer)));
    let head = String::from_utf8(answer[..end].to_vec()).unwrap();
    let status = head[9..12].parse().unwrap();
    let body = answer[end + 4..].to_vec();
    let length = format!("\r\nContent-Length: {}\r\n", body.len());
    assert!(head.starts_with("HTTP/1.1 "), "{head}");
    assert!(
        head.contains("\r\nContent-Type: application/json\r\n"),
        "{head}"
    );
    assert!(
        head.contains(&length),
        "exactly the body, then the end: {head}"
    );
    assert!(head.ends_with("\r\nConnection: close"), "{head}");
    Response { status, head, body }
}

pub(crate) fn request(method: &str, body: &[u8]) -> Vec<u8> {
    let mut raw = format!(
        "POST /v1/{method} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(body);
    raw
}

pub(crate) fn post(socket: &Path, method: &str, body: &Value) -> Response {
    exchange(socket, &request(method, body.to_string().as_bytes()))
}

pub(crate) fn ok(response: &Response) -> &[u8] {
    assert_eq!(
        response.status,
        200,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    &response.body
}

pub(crate) fn submit(socket: &Path, body: &Value) -> SubmitTurnResponse {
    serde_json::from_slice(ok(&post(socket, "submit_turn", body))).unwrap()
}

pub(crate) fn prompt(socket: &Path, session: &str, prompt: &str) -> String {
    let answer = submit(socket, &json!({"session": session, "prompt": prompt}));
    assert!(answer.accepted);
    answer.turn.unwrap()
}

pub(crate) fn status(socket: &Path, session: &str, turn: Option<&str>) -> SessionStatus {
    let mut body = json!({"session": session});
    if let Some(turn) = turn {
        body["turn"] = turn.into();
    }
    serde_json::from_slice(ok(&post(socket, "status", &body))).unwrap()
}

pub(crate) fn report(socket: &Path, session: &str, turn: &str) -> TurnReport {
    status(socket, session, Some(turn)).turn.unwrap()
}

pub(crate) fn interrupt(socket: &Path, session: &str) -> InterruptResponse {
    serde_json::from_slice(ok(&post(socket, "interrupt", &json!({"session": session})))).unwrap()
}

pub(crate) fn is_idle(socket: &Path) -> IdleStatus {
    serde_json::from_slice(ok(&post(socket, "is_idle", &json!({})))).unwrap()
}

/// The report of `turn` once it has ended, waiting at most 10 s.
pub(crate) fn ended(socket: &Path, session: &str, turn: &str) -> TurnReport {
    until_ended(turn, || report(socket, session, turn))
}

/// The first report `read` gives of `turn` in state `ended`, polling at most 10 s.
pub(crate) fn until_ended(turn: &str, mut read: impl FnMut() -> TurnReport) -> TurnReport {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let report = read();
        if report.state == TurnState::Ended {
            return report;
        }
        assert!(Instant::now() < deadline, "{turn} did not end: {report:?}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub(crate) fn inject(socket: &Path, event_id: &str, attempt: u64) -> InjectResponse {
    let body = json!({
        "event_id": event_id,
        "kind": "task_result",
        "prompt": format!("RESULT of {event_id}."),
        "lease": "0123abcd",
        "attempt": attempt,
    });
    serde_json::from_slice(ok(&post(socket, "inject", &body))).unwrap()
}

pub(crate) fn set_route(socket: &Path, session: &str, route: &str) -> RouteSet {
    let body = json!({"session": session, "route": route});
    serde_json::from_slice(ok(&post(socket, "set_route", &body))).unwrap()
}

/// The raw `turn_events` request for the events of `turn` of `session` after `after`.
pub(crate) fn events_request(session: &str, turn: &str, after: u64) -> Vec<u8> {
    let body = json!({"session": session, "turn": turn, "after": after});
    request("turn_events", body.to_string().as_bytes())
}

/// The page `turn_events` answers for `turn` of `session` after `after`.
pub(crate) fn turn_events(socket: &Path, session: &str, turn: &str, after: u64) -> TurnEventsPage {
    let response = exchange(socket, &events_request(session, turn, after));
    serde_json::from_slice(ok(&response)).unwrap()
}

/// The page of a `turn_events` answer read to the end of `stream`.
pub(crate) fn read_page(mut stream: UnixStream) -> TurnEventsPage {
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).unwrap();
    serde_json::from_slice(ok(&parse(&answer))).unwrap()
}
