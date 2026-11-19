//! Scripted C6 engine on a Unix socket: records every call and answers queued replies.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
}

/// `200` with `body`.
pub(crate) fn ok(body: Value) -> Answer {
    Answer::Json(200, body)
}

/// A C6 `Error` with its status.
pub(crate) fn error(status: u16, code: &str, message: &str) -> Answer {
    Answer::Json(status, json!({ "code": code, "message": message }))
}

#[derive(Default)]
struct State {
    scripts: HashMap<String, VecDeque<Answer>>,
    calls: Vec<(String, Value)>,
}

/// The running fake; stops accepting when dropped.
pub(crate) struct FakeC6 {
    path: PathBuf,
    state: Arc<Mutex<State>>,
    closing: Arc<AtomicBool>,
}

impl FakeC6 {
    /// Listens on `path`.
    pub(crate) fn start(path: &Path) -> Self {
        let listener = UnixListener::bind(path).expect("bind the C6 socket");
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
            path: path.to_owned(),
            state,
            closing,
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

    /// Every call so far: method and request body.
    pub(crate) fn calls(&self) -> Vec<(String, Value)> {
        self.state.lock().expect("fake state").calls.clone()
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
        self.closing.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path);
    }
}

fn serve(stream: UnixStream, shared: &Mutex<State>) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
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
        state.calls.push((method.clone(), body));
        state
            .scripts
            .get_mut(&method)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| error(500, "internal", "unscripted call"))
    };
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
    }
}
