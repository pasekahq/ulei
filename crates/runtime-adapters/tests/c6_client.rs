//! The C6 client against a fake adapter on a Unix socket, driven by the C6 fixtures.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c6::{ErrorCode, IdleStatus, InjectRequest, InjectResponse};
use ulei_runtime_adapters::client::{Client, Error, MAX_BODY};

const TIMEOUT: Duration = Duration::from_secs(2);

/// A fresh 0700 directory for one socket, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let temp = std::env::temp_dir();
        // A socket path holds at most 107 bytes.
        let base = if temp.as_os_str().len() <= 48 {
            temp
        } else {
            PathBuf::from("/tmp")
        };
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("ulei-c6-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .expect("scratch dir");
        Self(dir)
    }

    fn socket(&self) -> PathBuf {
        self.0.join("c6.sock")
    }

    fn uid(&self) -> u32 {
        fs::metadata(&self.0).expect("scratch metadata").uid()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A fake adapter that accepts one connection, reads one request, writes `answer` and
/// returns the request bytes it read.
fn serve_once(socket: &Path, answer: Vec<u8>) -> JoinHandle<Vec<u8>> {
    let listener = UnixListener::bind(socket).expect("bind");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream.set_read_timeout(Some(TIMEOUT)).expect("timeout");
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        while !whole_request(&request) {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => request.extend_from_slice(&chunk[..n]),
            }
        }
        let _ = stream.write_all(&answer);
        request
    })
}

fn whole_request(request: &[u8]) -> bool {
    let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&request[..end]);
    let length = head
        .split("\r\n")
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    request.len() >= end + 4 + length
}

fn response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// Calls the client against a fake adapter answering `answer`; returns the result and the
/// request the adapter read.
fn exchange<T>(answer: Vec<u8>, call: impl FnOnce(&Client) -> T) -> (T, Vec<u8>) {
    let scratch = Scratch::new();
    let server = serve_once(&scratch.socket(), answer);
    let client = Client::new(scratch.socket(), TIMEOUT).expect("client");
    let out = call(&client);
    (out, server.join().expect("fake adapter"))
}

/// `(file name, bytes)` of every fixture of `def` under `contracts/fixtures/c6/<sub>`.
fn fixtures(sub: &str, def: &str) -> Vec<(String, Vec<u8>)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/c6")
        .join(sub);
    let mut out: Vec<_> = fs::read_dir(&dir)
        .expect("fixture dir")
        .map(|e| e.expect("fixture entry").path())
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?.to_string();
            name.starts_with(&format!("{def}."))
                .then(|| (name, fs::read(&p).expect("fixture")))
        })
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no {sub} fixtures of {def}");
    out
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("fixture is JSON")
}

/// Compact JSON with keys sorted at every level.
fn canonical(value: &Value) -> Vec<u8> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            Value::Object(fields) => {
                let mut entries: Vec<_> = fields.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                Value::Object(
                    entries
                        .into_iter()
                        .map(|(k, v)| (k.clone(), sorted(v)))
                        .collect::<Map<_, _>>(),
                )
            }
            other => other.clone(),
        }
    }
    sorted(value).to_string().into_bytes()
}

fn expected_request(method: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "POST /v1/{method} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

#[test]
fn inject_sends_each_valid_request_fixture_as_canonical_json() {
    for (name, bytes) in fixtures("valid", "InjectRequest") {
        let fixture = json_of(&bytes);
        let request: InjectRequest = serde_json::from_value(fixture.clone()).expect(&name);
        let (out, sent) = exchange(response("200 OK", br#"{"accepted":true}"#), |c| {
            c.inject(&request)
        });
        assert!(out.is_ok(), "{name}: {out:?}");
        assert_eq!(
            String::from_utf8_lossy(&sent),
            String::from_utf8_lossy(&expected_request("inject", &canonical(&fixture))),
            "{name}"
        );
    }
}

#[test]
fn is_idle_sends_the_empty_fixture() {
    for (name, bytes) in fixtures("valid", "Empty") {
        let (out, sent) = exchange(response("200 OK", br#"{"idle":true}"#), Client::is_idle);
        assert!(out.is_ok(), "{name}: {out:?}");
        assert_eq!(
            sent,
            expected_request("is_idle", &canonical(&json_of(&bytes))),
            "{name}"
        );
    }
}

fn some_request() -> InjectRequest {
    InjectRequest {
        event_id: "e1".into(),
        kind: "task_result".into(),
        payload: "x".into(),
        lease: "0123abcd".into(),
        attempt: 1,
    }
}

#[test]
fn each_valid_response_fixture_parses_to_the_same_value() {
    for (name, bytes) in fixtures("valid", "InjectResponse") {
        let want: InjectResponse = serde_json::from_slice(&bytes).expect(&name);
        let (out, _) = exchange(response("200 OK", &bytes), |c| c.inject(&some_request()));
        assert_eq!(out.expect(&name), want, "{name}");
    }
    for (name, bytes) in fixtures("valid", "IdleStatus") {
        let want: IdleStatus = serde_json::from_slice(&bytes).expect(&name);
        let (out, _) = exchange(response("200 OK", &bytes), Client::is_idle);
        assert_eq!(out.expect(&name), want, "{name}");
    }
}

#[test]
fn each_invalid_response_fixture_is_refused_as_invalid() {
    for (name, bytes) in fixtures("invalid", "InjectResponse") {
        let (out, _) = exchange(response("200 OK", &bytes), |c| c.inject(&some_request()));
        assert!(matches!(out, Err(Error::Invalid(_))), "{name}: {out:?}");
    }
    for (name, bytes) in fixtures("invalid", "IdleStatus") {
        let (out, _) = exchange(response("200 OK", &bytes), Client::is_idle);
        assert!(matches!(out, Err(Error::Invalid(_))), "{name}: {out:?}");
    }
    for (name, bytes) in fixtures("invalid", "Error") {
        let (out, _) = exchange(
            response("500 Internal Server Error", &bytes),
            Client::is_idle,
        );
        assert!(matches!(out, Err(Error::Invalid(_))), "{name}: {out:?}");
    }
}

#[test]
fn an_error_answer_carries_its_code() {
    for (name, bytes) in fixtures("valid", "Error") {
        let (out, _) = exchange(response("404 Not Found", &bytes), Client::is_idle);
        let Err(Error::Remote { code, message }) = out else {
            panic!("{name}: {out:?}");
        };
        let want: Value = json_of(&bytes);
        assert_eq!(json!(code.as_str()), want["code"], "{name}");
        assert_eq!(json!(message), want["message"], "{name}");
    }
    let body = br#"{"code":"too_large","message":"body over 512 KiB"}"#;
    let (out, _) = exchange(response("413 Content Too Large", body), |c| {
        c.inject(&some_request())
    });
    assert!(
        matches!(
            out,
            Err(Error::Remote {
                code: ErrorCode::TooLarge,
                ..
            })
        ),
        "{out:?}"
    );
}

#[test]
fn an_error_whose_status_differs_from_its_code_is_invalid() {
    let body = br#"{"code":"not_found","message":"no such method or path"}"#;
    let (out, _) = exchange(response("500 Internal Server Error", body), Client::is_idle);
    assert!(matches!(out, Err(Error::Invalid(_))), "{out:?}");
}

#[test]
fn framing_failures_are_malformed() {
    let cases: [(&str, Vec<u8>); 5] = [
        (
            "no Content-Length",
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"idle\":true}".to_vec(),
        ),
        (
            "oversize body",
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY + 1
            )
            .into_bytes(),
        ),
        ("non-JSON body", response("200 OK", b"idle")),
        (
            "non-JSON error body",
            response("500 Internal Server Error", b"<html>oops</html>"),
        ),
        (
            "chunked body",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_vec(),
        ),
    ];
    for (what, answer) in cases {
        let (out, _) = exchange(answer, Client::is_idle);
        assert!(matches!(out, Err(Error::Malformed(_))), "{what}: {out:?}");
    }
}

#[test]
fn an_adapter_that_never_answers_times_out() {
    let scratch = Scratch::new();
    let _listener = UnixListener::bind(scratch.socket()).expect("bind");
    let client = Client::new(scratch.socket(), Duration::from_millis(50)).expect("client");
    let started = Instant::now();
    let out = client.is_idle();
    assert!(matches!(out, Err(Error::Timeout)), "{out:?}");
    assert!(started.elapsed() < Duration::from_millis(400));
}

#[test]
fn an_adapter_of_another_uid_is_refused_before_any_byte() {
    let scratch = Scratch::new();
    let server = serve_once(&scratch.socket(), response("200 OK", br#"{"idle":true}"#));
    let own = scratch.uid();
    let client = Client::with_uid(scratch.socket(), TIMEOUT, own.wrapping_add(1));
    let out = client.inject(&some_request());
    let Err(Error::PeerUid { expected, actual }) = out else {
        panic!("{out:?}");
    };
    assert_eq!((expected, actual), (own.wrapping_add(1), own));
    assert_eq!(server.join().expect("fake adapter"), b"");
}

#[test]
fn a_request_over_the_body_limit_is_refused_before_connecting() {
    let scratch = Scratch::new();
    let request = InjectRequest {
        payload: "x".repeat(MAX_BODY),
        ..some_request()
    };
    let client = Client::new(scratch.socket(), TIMEOUT).expect("client");
    let out = client.inject(&request);
    assert!(matches!(out, Err(Error::RequestTooLarge(_))), "{out:?}");
}

#[test]
fn a_missing_socket_is_an_io_error() {
    let scratch = Scratch::new();
    let client = Client::new(scratch.socket(), TIMEOUT).expect("client");
    let out = client.is_idle();
    assert!(matches!(out, Err(Error::Io(_))), "{out:?}");
}
