//! Scripted C4G gateway on a Unix socket: each `POST /v1/model/turn` takes the next queued
//! answer and is recorded; routes and cancel are served.

use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use ulei_contracts::generated::c4g::{
    CancelAnswer, CancelRequest, Error as GatewayError, ErrorClass, EventType, RouteInfo,
    RoutesAnswer, StopReason, StreamEvent, ToolCall, Usage,
};

/// Bounds every wait of the fake and of the tests on it.
const WAIT: Duration = Duration::from_secs(5);

/// How the fake answers one turn request.
#[derive(Clone, Debug)]
pub(crate) enum Answer {
    /// The text as the whole answer, then the end of the turn.
    Text(String),
    /// Nothing until [`FakeGateway::release`], then as [`Answer::Text`].
    Held(String),
    /// One call of tool `name` with `input`, then a stop for the tool results.
    Call(String, Value),
    /// The HTTP status with the gateway `Error` as its JSON body.
    Reject(u16, GatewayError),
}

#[derive(Default)]
struct State {
    script: VecDeque<Answer>,
    /// Bodies of the turn requests, in arrival order.
    turns: Vec<Value>,
    /// Held answers waiting now.
    held: usize,
    /// `request_id`s of the held turns, which a cancel names.
    running: HashSet<String>,
    /// Releases not yet taken by a held answer.
    released: usize,
    stopping: bool,
}

/// The running fake; stops when dropped.
pub(crate) struct FakeGateway {
    socket: PathBuf,
    state: Arc<(Mutex<State>, Condvar)>,
}

impl FakeGateway {
    /// Listens on `socket`.
    pub(crate) fn start(socket: &Path) -> Self {
        let listener = UnixListener::bind(socket).expect("bind the fake gateway");
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let shared = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming() {
                if shared.0.lock().expect("gateway").stopping {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let shared = Arc::clone(&shared);
                thread::spawn(move || serve(stream, &shared));
            }
        });
        Self {
            socket: socket.to_owned(),
            state,
        }
    }

    /// Queues `answer` for the next turn request.
    pub(crate) fn script(&self, answer: Answer) {
        self.state
            .0
            .lock()
            .expect("gateway")
            .script
            .push_back(answer);
    }

    /// Lets one held answer, waiting or later, go out.
    pub(crate) fn release(&self) {
        self.state.0.lock().expect("gateway").released += 1;
        self.state.1.notify_all();
    }

    /// Waits until `n` turn requests arrived, at most [`WAIT`].
    pub(crate) fn wait_turns(&self, n: usize) {
        self.wait(&format!("{n} turn requests"), |s| s.turns.len() >= n);
    }

    /// Waits until an answer is held, at most [`WAIT`].
    pub(crate) fn wait_held(&self) {
        self.wait("a held answer", |s| s.held > 0);
    }

    /// Bodies of the turn requests so far.
    pub(crate) fn turns(&self) -> Vec<Value> {
        self.state.0.lock().expect("gateway").turns.clone()
    }

    /// The text of the last user message of each turn request so far.
    pub(crate) fn prompts(&self) -> Vec<String> {
        self.turns().iter().map(last_user_text).collect()
    }

    fn wait(&self, what: &str, done: impl Fn(&State) -> bool) {
        let (state, changed) = &*self.state;
        let deadline = Instant::now() + WAIT;
        let mut state = state.lock().expect("gateway");
        while !done(&state) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the gateway never saw {what}");
            state = changed.wait_timeout(state, left).expect("gateway").0;
        }
    }
}

impl Drop for FakeGateway {
    fn drop(&mut self) {
        self.state.0.lock().expect("gateway").stopping = true;
        self.state.1.notify_all();
        drop(UnixStream::connect(&self.socket));
    }
}

fn serve(mut stream: UnixStream, shared: &(Mutex<State>, Condvar)) {
    let _ = stream.set_read_timeout(Some(WAIT));
    let _ = stream.set_write_timeout(Some(WAIT));
    let Some((target, body)) = read_request(&mut stream) else {
        return;
    };
    let bytes = match target.as_str() {
        "POST /v1/model/turn" => {
            let turn: Value = serde_json::from_slice(&body).expect("turn request");
            let id = turn["request_id"].as_str().expect("request_id").to_owned();
            let next = {
                let (state, changed) = shared;
                let mut state = state.lock().expect("gateway");
                state.turns.push(turn);
                changed.notify_all();
                state.script.pop_front()
            };
            match next {
                Some(Answer::Text(text)) => stream_answer(&text_events(&text)),
                Some(Answer::Held(text)) if held(shared, &id) => stream_answer(&text_events(&text)),
                Some(Answer::Held(_)) => return,
                Some(Answer::Call(name, input)) => {
                    let Value::Object(input) = input else {
                        panic!("tool input is not an object")
                    };
                    stream_answer(&[
                        route_event(),
                        StreamEvent {
                            tool_call: Some(ToolCall {
                                id: "c1".into(),
                                name,
                                input,
                            }),
                            ..event(EventType::ToolCall)
                        },
                        stop_event(StopReason::ToolUse),
                    ])
                }
                Some(Answer::Reject(status, error)) => json_answer(status, &error),
                None => json_answer(400, &bad_request("script exhausted")),
            }
        }
        "POST /v1/model/cancel" => {
            let cancel: CancelRequest = serde_json::from_slice(&body).expect("cancel request");
            if shared
                .0
                .lock()
                .expect("gateway")
                .running
                .contains(&cancel.request_id)
            {
                json_answer(200, &CancelAnswer { cancelled: true })
            } else {
                json_answer(404, &bad_request("no such running turn"))
            }
        }
        "GET /v1/model/routes" => json_answer(200, &RoutesAnswer { routes: Vec::new() }),
        _ => json_answer(404, &bad_request("no such endpoint")),
    };
    let _ = stream.write_all(&bytes);
}

/// Waits, with `id` running, for a release, true, or for the fake to stop, false.
fn held(shared: &(Mutex<State>, Condvar), id: &str) -> bool {
    let (state, changed) = shared;
    let mut state = state.lock().expect("gateway");
    state.held += 1;
    state.running.insert(id.to_owned());
    changed.notify_all();
    while state.released == 0 && !state.stopping {
        state = changed.wait(state).expect("gateway");
    }
    state.held -= 1;
    state.running.remove(id);
    if state.stopping {
        return false;
    }
    state.released -= 1;
    true
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

/// An event of `kind` without its payload.
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

fn stop_event(reason: StopReason) -> StreamEvent {
    StreamEvent {
        reason: Some(reason),
        ..event(EventType::Stop)
    }
}

fn route_event() -> StreamEvent {
    StreamEvent {
        route: Some(RouteInfo {
            vendor: "vendor-a".into(),
            model: "model-a".into(),
            login_label: "login-a".into(),
            lease: "lease-1".into(),
        }),
        ..event(EventType::Route)
    }
}

/// The events of a step that answers `text` and ends the turn.
fn text_events(text: &str) -> [StreamEvent; 4] {
    [
        route_event(),
        StreamEvent {
            text: Some(text.into()),
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
        stop_event(StopReason::End),
    ]
}

/// A `200` turn stream of `events`.
fn stream_answer(events: &[StreamEvent]) -> Vec<u8> {
    let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for event in events {
        let mut line = serde_json::to_vec(event).expect("event JSON");
        line.push(b'\n');
        out.extend_from_slice(format!("{:x}\r\n", line.len()).as_bytes());
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

fn json_answer(status: u16, body: &impl Serialize) -> Vec<u8> {
    let body = serde_json::to_string(body).expect("answer JSON");
    format!(
        "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// The text blocks of the last user message of a turn request.
fn last_user_text(turn: &Value) -> String {
    let messages = turn["messages"].as_array().expect("messages");
    let last = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .expect("a user message");
    message_text(last)
}

/// The text of each user message of a turn request that carries text, in order.
pub(crate) fn user_texts(turn: &Value) -> Vec<String> {
    turn["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] == "user")
        .map(message_text)
        .filter(|text| !text.is_empty())
        .collect()
}

fn message_text(message: &Value) -> String {
    message["content"]
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
