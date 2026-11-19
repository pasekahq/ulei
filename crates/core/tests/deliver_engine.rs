//! `ulei-inbox serve --deliver` against the workspace-built `ulei-engine serve` on a fake C4G
//! gateway. The test plays the channel over raw C6: it holds `inbox_turns` for the owner session
//! and reports each handed-over turn's delivery. `cargo test --workspace` builds the engine.

mod common;

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::daemon::{Daemon, Started, call, launch};
use common::{Scratch, sign, write_mode};
use serde_json::{Value, json};
use ulei_contracts::generated::c1::PublishResponse;
use ulei_core::deliver::ENABLED;
use ulei_core::inbox::{EventRow, Inbox};
use ulei_core::prompt::model_text;

const INBOX: &str = env!("CARGO_BIN_EXE_ulei-inbox");
const KEY: [u8; 32] = [9; 32];
const OWNER_SESSION: &str = "agent:main:telegram:dm:42";
/// Bounds every wait of the test.
const WAIT: Duration = Duration::from_secs(15);

/// The `ulei-engine` binary of this build: in the target directory that holds this test.
fn engine_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable");
    exe.ancestors()
        .map(|dir| dir.join("ulei-engine"))
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "no ulei-engine above {}; build the workspace (cargo test --workspace)",
                exe.display()
            )
        })
}

/// A C4G gateway that answers every turn with one fixed text and records each request body.
struct FakeGateway {
    turns: Arc<Mutex<Vec<Value>>>,
}

impl FakeGateway {
    fn start(socket: &Path) -> Self {
        let listener = UnixListener::bind(socket).expect("bind the fake gateway");
        let turns = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&turns);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let recorded = Arc::clone(&recorded);
                thread::spawn(move || gateway_answer(stream, &recorded));
            }
        });
        Self { turns }
    }

    /// The text of the last user message of each turn request so far.
    fn prompts(&self) -> Vec<String> {
        let turns = self.turns.lock().expect("gateway");
        turns
            .iter()
            .map(|turn| {
                let last = turn["messages"]
                    .as_array()
                    .expect("messages")
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "user")
                    .expect("a user message");
                last["content"]
                    .as_array()
                    .expect("content")
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect()
            })
            .collect()
    }
}

fn gateway_answer(mut stream: UnixStream, turns: &Mutex<Vec<Value>>) {
    let _ = stream.set_read_timeout(Some(WAIT));
    let Some((target, body)) = read_request(&mut stream) else {
        return;
    };
    let bytes = match target.as_str() {
        "POST /v1/model/turn" => {
            turns
                .lock()
                .expect("gateway")
                .push(serde_json::from_slice(&body).expect("turn request"));
            let route = json!({"vendor": "vendor-a", "model": "model-a", "login_label": "login-a", "lease": "lease-1"});
            let events = [
                json!({"type": "route", "route": route}),
                json!({"type": "text_delta", "text": "The backup is done."}),
                json!({"type": "usage", "usage": {"input": 10, "output": 3, "cache_read": 0, "cache_write": 0}}),
                json!({"type": "stop", "reason": "end"}),
            ];
            let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
            for event in events {
                let line = format!("{event}\n");
                out.extend_from_slice(format!("{:x}\r\n{line}\r\n", line.len()).as_bytes());
            }
            out.extend_from_slice(b"0\r\n\r\n");
            out
        }
        "POST /v1/model/cancel" => json_answer(&json!({"cancelled": false})),
        _ => json_answer(&json!({"routes": []})),
    };
    let _ = stream.write_all(&bytes);
}

fn json_answer(body: &Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// `METHOD /path` and the `Content-Length` body of one request.
fn read_request(stream: &mut UnixStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut piece = [0u8; 4096];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match stream.read(&mut piece) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&piece[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let length = head
        .split("\r\n")
        .find_map(|l| {
            let (name, value) = l.split_once(": ")?;
            name.eq_ignore_ascii_case("content-length").then_some(value)
        })
        .map_or(Some(0), |v| v.parse::<usize>().ok())?;
    while buf.len() < end + length {
        match stream.read(&mut piece) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&piece[..n]),
        }
    }
    let target = head.split("\r\n").next()?.rsplit_once(' ')?.0.to_owned();
    Some((target, buf.split_off(end)))
}

/// `ulei-engine serve` on the gateway at `gateway` with C6 at `c6`, killed on drop.
struct Engine(Child);

impl Engine {
    fn start(dir: &Path, gateway: &Path, c6: &Path) -> Self {
        let brain = dir.join("brain");
        fs::create_dir(&brain).expect("brain");
        fs::write(brain.join("SOUL.md"), "You are the test assistant.\n").expect("soul");
        let config = dir.join("ulei.toml");
        let text = format!(
            "[qb]\nsocket = \"{gateway}\"\n\n\
             [engine]\njournal = \"{journal}\"\nterminal_output = \"{terminal}\"\n\
             soul_file = \"{brain}/SOUL.md\"\nindex_file = \"{brain}/INDEX.md\"\n\
             memory_file = \"{brain}/MEMORY.md\"\nuser_file = \"{brain}/USER.md\"\n\
             skills_dir = \"{brain}/skills\"\nc6_socket = \"{c6}\"\n\
             owner_session = \"{OWNER_SESSION}\"\ndefault_route = \"main\"\n",
            gateway = gateway.display(),
            journal = dir.join("journal.sqlite3").display(),
            terminal = dir.join("terminal-output").display(),
            brain = brain.display(),
            c6 = c6.display(),
        );
        fs::write(&config, text).expect("config");
        let log = File::create(dir.join("engine.log")).expect("engine log");
        let mut child = Command::new(engine_binary())
            .args(["serve", "--config"])
            .arg(&config)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .expect("spawn the engine");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("ready line");
        assert!(
            line.starts_with("ulei-engine ready "),
            "no ready line: {line:?}, {}",
            fs::read_to_string(dir.join("engine.log")).unwrap_or_default()
        );
        Self(child)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The `200` body of C6 `method` with `body` on the engine at `socket`.
fn c6_call(socket: &Path, method: &str, body: &Value) -> Value {
    let body = body.to_string();
    let mut stream = UnixStream::connect(socket).expect("connect to the engine");
    write!(
        stream,
        "POST /v1/{method} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("request");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("answer");
    let (head, body) = answer.split_once("\r\n\r\n").expect("head");
    assert!(head.starts_with("HTTP/1.1 200 "), "{method}: {answer}");
    serde_json::from_str(body).expect("JSON body")
}

/// A held `inbox_turns` for the owner session; joins to the turn the engine hands over.
fn hold_inbox_turns(socket: &Path) -> JoinHandle<String> {
    let socket = socket.to_owned();
    thread::spawn(move || {
        let answer = c6_call(&socket, "inbox_turns", &json!({"session": OWNER_SESSION}));
        answer["turn"].as_str().expect("turn").to_owned()
    })
}

/// Waits until the engine reports `turn` ended, then reports its `delivery` as the channel.
fn report_when_ended(socket: &Path, turn: &str, delivery: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let status = c6_call(
            socket,
            "status",
            &json!({"session": OWNER_SESSION, "turn": turn}),
        );
        if status["turn"]["state"] == "ended" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "turn {turn} never ended: {status}"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let ack = c6_call(
        socket,
        "report_delivery",
        &json!({"session": OWNER_SESSION, "turn": turn, "delivery": delivery}),
    );
    assert_eq!(ack, json!({"ok": true}));
}

fn publish(daemon: &Daemon, id: &str, kind: &str, payload: &str) {
    let packet = sign(
        &KEY,
        &json!({
            "version": 1, "kind": kind, "id": id, "payload": payload,
            "source_ts": 1_763_290_000, "trust": "untrusted-source-data",
        }),
    );
    let out: PublishResponse = call(&daemon.socket, "publish", &json!({"packet": packet}), 200);
    assert_eq!(out.id, id);
}

fn wait_settled(inbox: &Inbox, id: &str) -> EventRow {
    let deadline = Instant::now() + WAIT;
    loop {
        let row = inbox.get(id).expect("get").expect("row");
        if !matches!(row.status.as_str(), "pending" | "leased" | "processing") {
            return row;
        }
        assert!(Instant::now() < deadline, "row {id} stayed {row:?}");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn each_published_event_runs_one_engine_turn_with_its_model_text_and_settles_from_the_delivery() {
    let scratch = Scratch::new();
    let gateway = FakeGateway::start(&scratch.0.join("gw.sock"));
    let c6 = scratch.0.join("c6.sock");
    let _engine = Engine::start(&scratch.0, &scratch.0.join("gw.sock"), &c6);
    c6_call(
        &c6,
        "set_route",
        &json!({"session": OWNER_SESSION, "route": "main"}),
    );
    let root = scratch.root_with_key(&KEY);
    write_mode(&root.join(ENABLED), b"", 0o600);
    let daemon = match launch(INBOX, &[], &root, &scratch.0.join("i"), Some(&c6)) {
        Started::Ready(daemon) => daemon,
        Started::Exited(status, stderr) => panic!("daemon exited {status}: {stderr}"),
    };
    let inbox = Inbox::open(&root).expect("inbox");

    let held = hold_inbox_turns(&c6);
    publish(
        &daemon,
        "evt:1",
        "task_result",
        "The nightly backup finished.",
    );
    let turn = held.join().expect("inbox_turns");
    report_when_ended(&c6, &turn, "sent");
    let first = wait_settled(&inbox, "evt:1");
    assert_eq!(
        (
            first.status.as_str(),
            first.outcome.as_deref(),
            first.attempts
        ),
        ("done", Some("success"), 1)
    );
    assert_eq!(gateway.prompts(), [model_text(&first).expect("model text")]);

    let held = hold_inbox_turns(&c6);
    publish(
        &daemon,
        "evt:2",
        "task_result",
        "A saved note needs no answer.",
    );
    let turn = held.join().expect("inbox_turns");
    report_when_ended(&c6, &turn, "silent_done");
    let second = wait_settled(&inbox, "evt:2");
    assert_eq!(
        (
            second.status.as_str(),
            second.outcome.as_deref(),
            second.attempts
        ),
        ("done", Some("silent"), 1)
    );
    assert_eq!(
        gateway.prompts(),
        [
            model_text(&first).expect("model text"),
            model_text(&second).expect("model text"),
        ]
    );
    assert_eq!(daemon.stderr(), "");
}
