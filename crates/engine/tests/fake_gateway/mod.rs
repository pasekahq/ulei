//! A scripted C4G gateway on a Unix socket for the engine's tests. Each turn is answered by the
//! next [`Answer`] of the script; routes and cancel are served; every request is recorded byte
//! for byte. A test crate uses it with `mod fake_gateway;`.

#![allow(dead_code, reason = "each test crate uses its own part of the fake")]

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ulei_contracts::generated::c4g::{
    CancelAnswer, CancelRequest, Error as GatewayError, ErrorClass, RoutesAnswer, StreamEvent,
    TurnRequest,
};

/// Bounds every read and write of the fake, so a broken test fails instead of hanging.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// How the fake answers one `POST /v1/model/turn`.
pub(crate) enum Answer {
    /// `200`, each event as one chunk, then the last chunk.
    Events(Vec<StreamEvent>),
    /// `200`, each event as one chunk, then the stream stays open and silent until the fake
    /// drops, cancelled or not.
    Hang(Vec<StreamEvent>),
    /// The status with the `Error` as its JSON body.
    Reject(u16, GatewayError),
    /// `200`, each element as one chunk, then the last chunk.
    Chunks(Vec<Vec<u8>>),
    /// These bytes verbatim as the whole answer.
    Raw(Vec<u8>),
}

/// One request as the fake read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    /// Head bytes up to and including the empty line.
    pub(crate) head: Vec<u8>,
    /// Exactly `Content-Length` body bytes.
    pub(crate) body: Vec<u8>,
}

impl Request {
    /// `METHOD /path` of the request line.
    pub(crate) fn target(&self) -> String {
        let line = self.head.split(|&b| b == b'\r').next().unwrap_or_default();
        let line = String::from_utf8_lossy(line);
        line.rsplit_once(' ')
            .map_or(String::new(), |(t, _)| t.to_owned())
    }
}

struct Shared {
    script: Mutex<VecDeque<Answer>>,
    routes: RoutesAnswer,
    requests: Mutex<Vec<Request>>,
    hanging: Mutex<HashSet<String>>,
    stopping: Mutex<bool>,
    wake: Condvar,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

/// The running fake; dropping it stops every connection and removes its directory.
pub(crate) struct FakeGateway {
    dir: PathBuf,
    socket: PathBuf,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

impl FakeGateway {
    /// A fake answering turns from `script` in order, with no routes.
    pub(crate) fn start(script: Vec<Answer>) -> Self {
        Self::with_routes(script, RoutesAnswer { routes: Vec::new() })
    }

    /// A fake answering turns from `script` in order and `GET /v1/model/routes` with `routes`.
    pub(crate) fn with_routes(script: Vec<Answer>, routes: RoutesAnswer) -> Self {
        let dir = scratch_dir();
        let socket = dir.join("gw.sock");
        let listener = UnixListener::bind(&socket).expect("bind the fake gateway");
        let shared = Arc::new(Shared {
            script: Mutex::new(script.into()),
            routes,
            requests: Mutex::new(Vec::new()),
            hanging: Mutex::new(HashSet::new()),
            stopping: Mutex::new(false),
            wake: Condvar::new(),
            workers: Mutex::new(Vec::new()),
        });
        let accept = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || accept_loop(&listener, &shared))
        };
        Self {
            dir,
            socket,
            shared,
            accept: Some(accept),
        }
    }

    /// Path of the listening socket.
    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    /// Every request received so far, in arrival order.
    pub(crate) fn requests(&self) -> Vec<Request> {
        self.shared.requests.lock().expect("requests").clone()
    }
}

impl Drop for FakeGateway {
    fn drop(&mut self) {
        *self.shared.stopping.lock().expect("stopping") = true;
        self.shared.wake.notify_all();
        // Wakes the accept loop so it sees `stopping`.
        drop(UnixStream::connect(&self.socket));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let workers = std::mem::take(&mut *self.shared.workers.lock().expect("workers"));
        for worker in workers {
            let _ = worker.join();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A fresh 0700 directory under the temp dir, short enough for a 107-byte socket path.
fn scratch_dir() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let temp = std::env::temp_dir();
    let base = if temp.as_os_str().len() <= 48 {
        temp
    } else {
        PathBuf::from("/tmp")
    };
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("ulei-gw-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .expect("scratch dir");
    dir
}

fn accept_loop(listener: &UnixListener, shared: &Arc<Shared>) {
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

fn serve(mut stream: UnixStream, shared: &Shared) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    shared
        .requests
        .lock()
        .expect("requests")
        .push(request.clone());
    let answer = match request.target().as_str() {
        "GET /v1/model/routes" => json(200, serde_json::to_vec(&shared.routes)),
        "POST /v1/model/cancel" => match serde_json::from_slice::<CancelRequest>(&request.body) {
            Ok(cancel) => {
                let cancelled = shared
                    .hanging
                    .lock()
                    .expect("hanging")
                    .contains(&cancel.request_id);
                json(200, serde_json::to_vec(&CancelAnswer { cancelled }))
            }
            Err(_) => json(400, serde_json::to_vec(&bad_request("not a CancelRequest"))),
        },
        "POST /v1/model/turn" => match serde_json::from_slice::<TurnRequest>(&request.body) {
            Ok(turn) => {
                let next = shared.script.lock().expect("script").pop_front();
                match next {
                    Some(answer) => return play(stream, shared, &turn.request_id, answer),
                    None => json(400, serde_json::to_vec(&bad_request("script exhausted"))),
                }
            }
            Err(_) => json(400, serde_json::to_vec(&bad_request("not a TurnRequest"))),
        },
        _ => json(404, serde_json::to_vec(&bad_request("unknown path"))),
    };
    let _ = stream.write_all(&answer);
}

fn play(mut stream: UnixStream, shared: &Shared, request_id: &str, answer: Answer) {
    match answer {
        Answer::Events(events) => {
            let _ =
                stream_events(&mut stream, &events).and_then(|()| stream.write_all(b"0\r\n\r\n"));
        }
        Answer::Hang(events) => {
            shared
                .hanging
                .lock()
                .expect("hanging")
                .insert(request_id.to_owned());
            let _ = stream_events(&mut stream, &events);
            let mut stopping = shared.stopping.lock().expect("stopping");
            while !*stopping {
                stopping = shared.wake.wait(stopping).expect("stopping");
            }
        }
        Answer::Reject(status, error) => {
            let _ = stream.write_all(&json(status, serde_json::to_vec(&error)));
        }
        Answer::Chunks(chunks) => {
            let _ = stream.write_all(STREAM_HEAD);
            for data in chunks {
                if stream.write_all(&chunk(&data)).is_err() {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        Answer::Raw(bytes) => {
            let _ = stream.write_all(&bytes);
        }
    }
}

/// Head of a `200` turn stream.
pub(crate) const STREAM_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";

/// Writes the stream head and one chunk per event line.
fn stream_events(stream: &mut UnixStream, events: &[StreamEvent]) -> std::io::Result<()> {
    stream.write_all(STREAM_HEAD)?;
    for event in events {
        stream.write_all(&chunk(&line(event)))?;
    }
    Ok(())
}

/// One NDJSON line of `event`.
pub(crate) fn line(event: &StreamEvent) -> Vec<u8> {
    let mut out = serde_json::to_vec(event).expect("event JSON");
    out.push(b'\n');
    out
}

/// `data` framed as one HTTP chunk.
pub(crate) fn chunk(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

fn bad_request(message: &str) -> GatewayError {
    GatewayError {
        class: ErrorClass::BadRequest,
        retryable: false,
        retry_after: None,
        resets_at: None,
        tried: None,
        message: Some(message.to_owned()),
    }
}

/// A `Content-Length` answer with the serialized `body`.
fn json(status: u16, body: serde_json::Result<Vec<u8>>) -> Vec<u8> {
    let body = body.expect("answer JSON");
    let mut out = format!(
        "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(&body);
    out
}

/// Reads one request: the head and exactly `Content-Length` body bytes.
fn read_request(stream: &mut UnixStream) -> Option<Request> {
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
    let length = String::from_utf8_lossy(&buf[..end])
        .split("\r\n")
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .map_or(Some(0), |v| v.parse::<usize>().ok())?;
    while buf.len() < end + length {
        match stream.read(&mut piece) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&piece[..n]),
        }
    }
    let body = buf.split_off(end);
    Some(Request { head: buf, body })
}
