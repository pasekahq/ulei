//! Scripted C6 engine on a Unix socket: records every call and answers queued replies. An
//! `inbox_turns` call is recorded apart, and one with no queued reply is held, as the engine
//! holds it while no inbox turn waits, until the fake is dropped.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// One scripted answer.
#[derive(Clone, Debug)]
pub(crate) enum Answer {
    /// HTTP status and JSON body.
    Json(u16, Value),
    /// Keep the connection open without answering.
    Silent,
    /// Close the connection without answering.
    Close,
    /// Hold the connection until the gate opens, then give its answer.
    Held(Gate),
}

/// The answer a held call waits for; clones share it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Gate(Arc<(Mutex<Option<Answer>>, Condvar)>);

impl Gate {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Releases the held call with `answer`.
    pub(crate) fn open(&self, answer: Answer) {
        let (slot, cvar) = &*self.0;
        *slot.lock().expect("gate") = Some(answer);
        cvar.notify_all();
    }

    fn wait(&self) -> Answer {
        let (slot, cvar) = &*self.0;
        let mut answer = slot.lock().expect("gate");
        loop {
            if let Some(answer) = answer.take() {
                return answer;
            }
            answer = cvar.wait(answer).expect("gate");
        }
    }
}

/// `200` with `body`.
pub(crate) fn ok(body: Value) -> Answer {
    Answer::Json(200, body)
}

/// A C6 `Error` with its status.
pub(crate) fn error(status: u16, code: &str, message: &str) -> Answer {
    Answer::Json(status, json!({ "code": code, "message": message }))
}

/// A `turn_events` page of `turn` in `state` with `events`.
pub(crate) fn page(turn: &str, state: &str, events: &[Value]) -> Answer {
    ok(json!({ "turn": turn, "state": state, "events": events }))
}

/// The `start` event, always the first of a turn.
pub(crate) fn start() -> Value {
    json!({ "seq": 1, "type": "start" })
}

/// Event `seq` of type `kind` (`interim` or `final`) with `text`.
pub(crate) fn text(seq: u64, kind: &str, text: &str) -> Value {
    json!({ "seq": seq, "type": kind, "text": text })
}

/// The `end` event `seq` with `outcome` and its `line`, if any.
pub(crate) fn end(seq: u64, outcome: &str, line: Option<&str>) -> Value {
    let mut event = json!({ "seq": seq, "type": "end", "outcome": outcome });
    if let Some(line) = line {
        event["line"] = json!(line);
    }
    event
}

/// A socket path of its own in the temporary directory, named after `prefix`, removed on drop.
pub(crate) struct Socket(pub(crate) PathBuf);

impl Socket {
    pub(crate) fn new(prefix: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        Self(std::env::temp_dir().join(format!("{prefix}-{}-{n}.sock", std::process::id())))
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

const INBOX_TURNS: &str = "inbox_turns";

#[derive(Default)]
struct State {
    scripts: HashMap<String, VecDeque<Answer>>,
    calls: Vec<(String, Value)>,
    holds: Vec<Value>,
}

/// Set once the fake is dropped; the unscripted `inbox_turns` calls wait for it.
type Dropped = Arc<(Mutex<bool>, Condvar)>;

/// The running fake; stops accepting when dropped.
pub(crate) struct FakeC6 {
    path: PathBuf,
    state: Arc<Mutex<State>>,
    closing: Arc<AtomicBool>,
    dropped: Dropped,
}

impl FakeC6 {
    /// Listens on `path`.
    pub(crate) fn start(path: &Path) -> Self {
        let listener = UnixListener::bind(path).expect("bind the C6 socket");
        let state = Arc::new(Mutex::new(State::default()));
        let closing = Arc::new(AtomicBool::new(false));
        let dropped = Dropped::default();
        let (shared, stop, parked) = (
            Arc::clone(&state),
            Arc::clone(&closing),
            Arc::clone(&dropped),
        );
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (shared, parked) = (Arc::clone(&shared), Arc::clone(&parked));
                thread::spawn(move || serve(stream, &shared, &parked));
            }
        });
        Self {
            path: path.to_owned(),
            state,
            closing,
            dropped,
        }
    }

    /// Queues `answer` for the next call of `method`.
    pub(crate) fn script(&self, method: &str, answer: Answer) {
        self.state
            .lock()
            .expect("fake state")
            .scripts
            .entry(method.to_owned())
            .or_default()
            .push_back(answer);
    }

    /// Every call so far but `inbox_turns`: method and request body.
    pub(crate) fn calls(&self) -> Vec<(String, Value)> {
        self.state.lock().expect("fake state").calls.clone()
    }

    /// The request bodies of the `inbox_turns` calls so far.
    pub(crate) fn holds(&self) -> Vec<Value> {
        self.state.lock().expect("fake state").holds.clone()
    }

    /// Waits up to 5 s until `count` `inbox_turns` calls arrived.
    pub(crate) fn wait_holds(&self, count: usize) {
        let until = Instant::now() + Duration::from_secs(5);
        while self.holds().len() < count {
            assert!(Instant::now() < until, "only {:?}", self.holds());
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// The method names of the calls so far.
    pub(crate) fn methods(&self) -> Vec<String> {
        self.calls().into_iter().map(|(m, _)| m).collect()
    }

    /// Waits up to 5 s until `count` calls arrived.
    pub(crate) fn wait_for(&self, count: usize) {
        let until = Instant::now() + Duration::from_secs(5);
        while self.calls().len() < count {
            assert!(Instant::now() < until, "only {:?}", self.methods());
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for FakeC6 {
    fn drop(&mut self) {
        let (flag, cvar) = &*self.dropped;
        *flag.lock().expect("dropped flag") = true;
        cvar.notify_all();
        self.closing.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path);
    }
}

fn serve(stream: UnixStream, shared: &Mutex<State>, dropped: &Dropped) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    // A client may connect and close without a request; that is no call.
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let method = line
        .split(' ')
        .nth(1)
        .and_then(|path| path.strip_prefix("/v1/"))
        .unwrap_or_default()
        .to_owned();
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header == "\r\n" || header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let answer = {
        let mut state = shared.lock().expect("fake state");
        let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if method == INBOX_TURNS {
            state.holds.push(body);
        } else {
            state.calls.push((method.clone(), body));
        }
        state.scripts.get_mut(&method).and_then(VecDeque::pop_front)
    };
    match answer {
        Some(answer) => reply(&mut writer, answer),
        None if method == INBOX_TURNS => {
            let (flag, cvar) = &**dropped;
            let mut gone = flag.lock().expect("dropped flag");
            while !*gone {
                gone = cvar.wait(gone).expect("dropped flag");
            }
        }
        None => reply(&mut writer, error(500, "internal", "unscripted call")),
    }
}

fn reply(writer: &mut UnixStream, answer: Answer) {
    match answer {
        Answer::Json(status, value) => {
            let body = value.to_string();
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = writer.write_all(head.as_bytes());
            let _ = writer.write_all(body.as_bytes());
        }
        Answer::Silent => thread::sleep(Duration::from_secs(5)),
        Answer::Close => {}
        Answer::Held(gate) => reply(writer, gate.wait()),
    }
}
