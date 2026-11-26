//! The C6 client against the engine's C6 server: the `ulei-engine` binary of the same workspace
//! build serves on a socket, its turns on a fake C4G gateway, and every call goes through
//! [`Client`]. The binary is reached only over C6; `cargo test --workspace` builds it before
//! any test runs.

mod common;

use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::{Scratch, response};
use serde_json::{Value, json};
use ulei_contracts::generated::c4g::{
    CancelAnswer, CancelRequest, Error as GatewayError, ErrorClass, EventType, RouteInfo,
    RoutesAnswer, StopReason, StreamEvent, Usage,
};
use ulei_contracts::generated::c6::{
    ErrorCode, InjectRequest, InterruptRequest, InterruptResponse, SetRouteRequest, StatusRequest,
    SubmitTurnRequest, TurnOutcome, TurnReport, TurnState,
};
use ulei_runtime_adapters::client::{Client, Error};

const OWNER: &str = "owner";
/// A prompt that contains this holds its turn at the gateway until released or cancelled.
const HOLD: &str = "HOLD";
const WAIT: Duration = Duration::from_secs(5);

/// What the fake gateway saw and the turns it holds.
#[derive(Default)]
struct Seen {
    /// `(route, last user text)` of every turn request, in arrival order.
    turns: Vec<(String, String)>,
    /// Held turns waiting now.
    held: usize,
    /// `request_id`s of the held turns, which a cancel names.
    running: HashSet<String>,
    /// Releases not yet taken by a held turn.
    released: usize,
    stopping: bool,
}

/// A C4G gateway on a Unix socket: a turn answers `done` and ends, a turn whose last user text
/// contains [`HOLD`] waits for [`Gateway::release`] or for the engine to close it.
struct Gateway {
    socket: PathBuf,
    seen: Arc<(Mutex<Seen>, Condvar)>,
}

impl Gateway {
    fn start(dir: &Scratch) -> Self {
        let socket = dir.path("gw.sock");
        let listener = UnixListener::bind(&socket).expect("bind the gateway");
        let seen = Arc::new((Mutex::new(Seen::default()), Condvar::new()));
        let shared = Arc::clone(&seen);
        thread::spawn(move || {
            for stream in listener.incoming() {
                if shared.0.lock().expect("seen").stopping {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let shared = Arc::clone(&shared);
                thread::spawn(move || serve_gateway(stream, &shared));
            }
        });
        Self { socket, seen }
    }

    /// Waits until `n` turns are held at once.
    fn wait_held(&self, n: usize) {
        let (seen, changed) = &*self.seen;
        let (seen, _) = changed
            .wait_timeout_while(seen.lock().expect("seen"), WAIT, |s| s.held < n)
            .expect("seen");
        assert!(seen.held >= n, "{} turns held, want {n}", seen.held);
    }

    /// Lets one held turn, waiting or later, answer.
    fn release(&self) {
        let (seen, changed) = &*self.seen;
        seen.lock().expect("seen").released += 1;
        changed.notify_all();
    }

    fn turns(&self) -> Vec<(String, String)> {
        self.seen.0.lock().expect("seen").turns.clone()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.seen.0.lock().expect("seen").stopping = true;
        self.seen.1.notify_all();
        drop(UnixStream::connect(&self.socket));
    }
}

fn serve_gateway(mut stream: UnixStream, shared: &(Mutex<Seen>, Condvar)) {
    let _ = stream.set_read_timeout(Some(WAIT));
    let Some((target, body)) = read_request(&mut stream) else {
        return;
    };
    let answer = match target.as_str() {
        "POST /v1/model/turn" => {
            let turn: Value = serde_json::from_slice(&body).expect("turn request");
            let route = turn["route"].as_str().expect("route").to_owned();
            let text = last_user_text(&turn);
            let id = turn["request_id"].as_str().expect("request_id").to_owned();
            let hold = text.contains(HOLD);
            shared.0.lock().expect("seen").turns.push((route, text));
            if hold && !held(&mut stream, shared, &id) {
                return;
            }
            let _ = stream.write_all(&stream_answer());
            return;
        }
        "POST /v1/model/cancel" => {
            let cancel: CancelRequest = serde_json::from_slice(&body).expect("cancel request");
            if shared
                .0
                .lock()
                .expect("seen")
                .running
                .contains(&cancel.request_id)
            {
                (
                    "200 OK",
                    serde_json::to_vec(&CancelAnswer { cancelled: true }),
                )
            } else {
                (
                    "404 Not Found",
                    serde_json::to_vec(&bad_request("no such running turn")),
                )
            }
        }
        "GET /v1/model/routes" => (
            "200 OK",
            serde_json::to_vec(&RoutesAnswer { routes: Vec::new() }),
        ),
        _ => (
            "404 Not Found",
            serde_json::to_vec(&bad_request("no such endpoint")),
        ),
    };
    let (status, body) = answer;
    let _ = stream.write_all(&response(status, &body.expect("answer JSON")));
}

fn bad_request(message: &str) -> GatewayError {
    GatewayError {
        class: ErrorClass::BadRequest,
        retryable: false,
        retry_after: None,
        resets_at: None,
        tried: None,
        message: Some(message.into()),
    }
}

/// Holds the turn `id` until a release, true, or until the engine closes the stream or the fake
/// stops, false.
fn held(stream: &mut UnixStream, shared: &(Mutex<Seen>, Condvar), id: &str) -> bool {
    let (seen, changed) = shared;
    {
        let mut seen = seen.lock().expect("seen");
        seen.held += 1;
        seen.running.insert(id.to_owned());
    }
    changed.notify_all();
    let _ = stream.set_read_timeout(Some(Duration::from_millis(10)));
    let released = loop {
        {
            let mut seen = seen.lock().expect("seen");
            if seen.stopping {
                break false;
            }
            if seen.released > 0 {
                seen.released -= 1;
                break true;
            }
        }
        match stream.read(&mut [0u8; 64]) {
            Ok(0) => break false,
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break false,
        }
    };
    {
        let mut seen = seen.lock().expect("seen");
        seen.held -= 1;
        seen.running.remove(id);
    }
    changed.notify_all();
    released
}

/// A `200` C4G turn stream that answers `done` and stops at the end of the turn.
fn stream_answer() -> Vec<u8> {
    let events = [
        StreamEvent {
            route: Some(RouteInfo {
                vendor: "vendor-a".into(),
                model: "model-a".into(),
                login_label: "login-a".into(),
                lease: "lease-1".into(),
            }),
            ..event(EventType::Route)
        },
        StreamEvent {
            text: Some("done".into()),
            ..event(EventType::TextDelta)
        },
        StreamEvent {
            usage: Some(Usage {
                input: 10,
                output: 3,
                cache_read: 0,
                cache_write: 0,
            }),
            ..event(EventType::Usage)
        },
        StreamEvent {
            reason: Some(StopReason::End),
            ..event(EventType::Stop)
        },
    ];
    let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for event in events {
        let mut line = serde_json::to_vec(&event).expect("event JSON");
        line.push(b'\n');
        out.extend_from_slice(format!("{:x}\r\n", line.len()).as_bytes());
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

/// A gateway event of `kind` without its payload.
fn event(kind: EventType) -> StreamEvent {
    StreamEvent {
        r#type: kind,
        route: None,
        text: None,
        thinking: None,
        tool_call: None,
        usage: None,
        reason: None,
        error: None,
    }
}

/// The text blocks of the last user message of a C4G turn request.
fn last_user_text(turn: &Value) -> String {
    let messages = turn["messages"].as_array().expect("messages");
    let last = messages
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
}

/// `METHOD /path` and exactly `Content-Length` body bytes of one request.
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

/// A running `ulei-engine serve` on a fake gateway with a client of its socket; the engine is
/// killed on drop.
struct Engine {
    child: Child,
    socket: PathBuf,
    client: Client,
    gateway: Gateway,
    _dir: Scratch,
}

impl Engine {
    fn start() -> Self {
        let dir = Scratch::new();
        let gateway = Gateway::start(&dir);
        let brain = dir.path("brain");
        fs::create_dir_all(&brain).expect("brain");
        fs::write(brain.join("SOUL.md"), "You are the test assistant.\n").expect("soul");
        let config = format!(
            "[qb]\nsocket = \"{}\"\n\n[engine]\njournal = \"{}\"\nterminal_output = \"{}\"\nsoul_file = \"{}\"\nindex_file = \"{}\"\nmemory_file = \"{}\"\nuser_file = \"{}\"\nskills_dir = \"{}\"\nc6_socket = \"{}\"\nowner_session = \"{OWNER}\"\ndefault_route = \"main\"\n",
            gateway.socket.display(),
            dir.path("journal.sqlite3").display(),
            dir.path("terminal-output").display(),
            brain.join("SOUL.md").display(),
            brain.join("INDEX.md").display(),
            brain.join("MEMORY.md").display(),
            brain.join("USER.md").display(),
            brain.join("skills").display(),
            dir.path("c6.sock").display(),
        );
        let config_path = dir.path("ulei.toml");
        fs::write(&config_path, config).expect("config");
        let mut child = Command::new(engine_binary())
            .args(["serve", "--config"])
            .arg(&config_path)
            .current_dir(&dir.0)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &dir.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the engine");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("ready line");
        let socket = PathBuf::from(
            line.strip_prefix("ulei-engine ready ")
                .unwrap_or_else(|| panic!("no ready line: {line:?}"))
                .trim_end(),
        );
        let client = Client::new(&socket, WAIT).expect("client");
        Self {
            child,
            socket,
            client,
            gateway,
            _dir: dir,
        }
    }

    /// Holds `inbox_turns` for the owner session on a raw connection, as the channel does; the
    /// engine registers the hold before it serves a later call.
    fn hold_inbox(&self) -> UnixStream {
        let body = json!({"session": OWNER}).to_string();
        let mut stream = UnixStream::connect(&self.socket).expect("connect");
        let head = format!(
            "POST /v1/inbox_turns HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream
            .write_all([head.as_bytes(), body.as_bytes()].concat().as_slice())
            .expect("inbox_turns");
        stream
    }

    fn submit(&self, session: &str, prompt: &str) -> (String, u64) {
        let answer = self
            .client
            .submit_turn(&SubmitTurnRequest {
                session: session.into(),
                prompt: prompt.into(),
                channel: None,
                new: None,
                context: None,
                images: None,
                message_id: None,
            })
            .expect("submit_turn");
        assert!(answer.accepted, "{answer:?}");
        (
            answer.turn.expect("turn"),
            answer.position.expect("position"),
        )
    }

    /// The report of `turn` once it has left the queue and stopped running.
    fn settled(&self, session: &str, turn: &str) -> TurnReport {
        let deadline = Instant::now() + WAIT;
        loop {
            let status = self
                .client
                .status(&StatusRequest {
                    session: session.into(),
                    turn: Some(turn.into()),
                })
                .expect("status");
            let report = status.turn.expect("the turn asked for");
            if !matches!(report.state, TurnState::Queued | TurnState::Running) {
                return report;
            }
            assert!(Instant::now() < deadline, "{turn} still {report:?}");
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_second_turn_queues_and_status_follows_both_to_their_outcome() {
    let engine = Engine::start();
    let (first, at) = engine.submit("chat", &format!("{HOLD} first"));
    assert_eq!(at, 0);
    engine.gateway.wait_held(1);
    let (second, at) = engine.submit("chat", "second");
    assert_eq!(at, 1);
    let running = engine
        .client
        .status(&StatusRequest {
            session: "chat".into(),
            turn: Some(second.clone()),
        })
        .expect("status");
    assert!(running.running);
    assert_eq!(running.queue_depth, 1);
    assert_eq!(running.turn.expect("turn").state, TurnState::Queued);

    engine.gateway.release();
    for turn in [&first, &second] {
        let report = engine.settled("chat", turn);
        assert_eq!(report.state, TurnState::Ended, "{report:?}");
        assert_eq!(report.outcome, Some(TurnOutcome::Completed), "{report:?}");
        assert!(report.ended.is_some(), "{report:?}");
    }
    let texts: Vec<String> = engine.gateway.turns().into_iter().map(|t| t.1).collect();
    assert_eq!(texts, [format!("{HOLD} first"), "second".into()]);
}

#[test]
fn interrupt_stops_the_running_turn_and_drops_the_queue_head() {
    let engine = Engine::start();
    let (running, _) = engine.submit("chat", &format!("{HOLD} long"));
    engine.gateway.wait_held(1);
    let (head, _) = engine.submit("chat", "head");
    let (tail, _) = engine.submit("chat", "tail");
    let answer = engine
        .client
        .interrupt(&InterruptRequest {
            session: "chat".into(),
        })
        .expect("interrupt");
    assert_eq!(
        answer,
        InterruptResponse {
            interrupted: true,
            turn: Some(running.clone()),
            dropped: Some(head.clone()),
            queue_depth: 1,
        }
    );
    let report = engine.settled("chat", &running);
    assert_eq!(report.outcome, Some(TurnOutcome::Interrupted), "{report:?}");
    assert_eq!(engine.settled("chat", &head).state, TurnState::Dropped);
    let report = engine.settled("chat", &tail);
    assert_eq!(report.outcome, Some(TurnOutcome::Completed), "{report:?}");
    let texts: Vec<String> = engine.gateway.turns().into_iter().map(|t| t.1).collect();
    assert_eq!(texts, [format!("{HOLD} long"), "tail".into()]);
}

#[test]
fn set_route_runs_the_next_turn_on_the_stored_alias() {
    let engine = Engine::start();
    let (before, _) = engine.submit("chat", "before");
    engine.settled("chat", &before);
    let stored = engine
        .client
        .set_route(&SetRouteRequest {
            session: "chat".into(),
            route: "fast".into(),
        })
        .expect("set_route");
    assert_eq!(
        (stored.session.as_str(), stored.route.as_str()),
        ("chat", "fast")
    );
    let status = engine
        .client
        .status(&StatusRequest {
            session: "chat".into(),
            turn: None,
        })
        .expect("status");
    assert_eq!(status.route, "fast");
    let (after, _) = engine.submit("chat", "after");
    let report = engine.settled("chat", &after);
    assert_eq!(report.outcome, Some(TurnOutcome::Completed), "{report:?}");
    let routes: Vec<String> = engine.gateway.turns().into_iter().map(|t| t.0).collect();
    assert_eq!(routes, ["main", "fast"]);
}

#[test]
fn an_inject_replay_answers_the_same_turn_and_the_owner_session_is_idle_after_it() {
    let engine = Engine::start();
    engine
        .client
        .set_route(&SetRouteRequest {
            session: OWNER.into(),
            route: "main".into(),
        })
        .expect("set_route");
    let _hold = engine.hold_inbox();
    let event = InjectRequest {
        event_id: "hive:h_0001:done".into(),
        kind: "task_result".into(),
        prompt: format!("{HOLD} RESULT: done."),
        lease: "0123abcd".into(),
        attempt: 1,
    };
    let first = engine.client.inject(&event).expect("inject");
    assert!(first.accepted, "{first:?}");
    assert_eq!(first.session_id.as_deref(), Some(OWNER));
    let turn = first.turn.clone().expect("turn");
    engine.gateway.wait_held(1);
    assert!(!engine.client.is_idle().expect("is_idle").idle);

    let replay = engine
        .client
        .inject(&InjectRequest {
            lease: "89abcdef".into(),
            ..event
        })
        .expect("replay");
    assert_eq!(replay, first);

    engine.gateway.release();
    let report = engine.settled(OWNER, &turn);
    assert_eq!(report.outcome, Some(TurnOutcome::Completed), "{report:?}");
    let _again = engine.hold_inbox();
    let idle = engine.client.is_idle().expect("is_idle");
    assert!(idle.idle, "{idle:?}");
    assert!(
        idle.since
            .is_some_and(|since| since >= report.ended.unwrap_or(f64::MAX))
    );
    assert_eq!(engine.gateway.turns().len(), 1);
}

#[test]
fn an_unknown_method_is_a_not_found_error() {
    let engine = Engine::start();
    let out = engine.client.call("no_such_method", &json!({}));
    assert!(
        matches!(
            out,
            Err(Error::Remote {
                code: ErrorCode::NotFound,
                ..
            })
        ),
        "{out:?}"
    );
}
