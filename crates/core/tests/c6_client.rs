//! The core's C6 client against a fake engine on a Unix socket.

mod common;

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::Scratch;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use serde_json::{Value, json};
use ulei_contracts::generated::c6::{
    ErrorCode, IdleStatus, InjectRequest, InjectResponse, SessionStatus, StatusRequest,
};
use ulei_core::c6::{Client, Error, MAX_BODY};
use ulei_core::inbox::{EventRow, canonical};
use ulei_core::prompt::model_text;

const TIMEOUT: Duration = Duration::from_secs(2);

/// A fake engine that accepts one connection, reads one request, writes `answer` and returns
/// the request bytes it read.
fn serve_once(socket: &Path, answer: Vec<u8>) -> JoinHandle<Vec<u8>> {
    let listener = UnixListener::bind(socket).expect("bind");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream.set_read_timeout(Some(TIMEOUT)).expect("timeout");
        let mut request = Vec::new();
        let mut chunk = [0u8; 8192];
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
    request.len() >= end + 4 + content_length(&request[..end])
}

fn content_length(head: &[u8]) -> usize {
    String::from_utf8_lossy(head)
        .split("\r\n")
        .find_map(|l| l.strip_prefix("Content-Length: ")?.parse().ok())
        .unwrap_or(0)
}

/// An HTTP/1.1 answer with `status` and the JSON `body`, closing the connection.
fn response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn own_uid(scratch: &Scratch) -> u32 {
    fs::metadata(&scratch.0).expect("scratch metadata").uid()
}

fn client(scratch: &Scratch) -> Client {
    Client::with_limits(scratch.socket(), TIMEOUT, TIMEOUT, own_uid(scratch))
}

/// Calls the client against a fake engine answering `answer`; returns the result and the
/// request the engine read.
fn exchange<T>(answer: Vec<u8>, call: impl FnOnce(&Client) -> T) -> (T, Vec<u8>) {
    let scratch = Scratch::new();
    let server = serve_once(&scratch.socket(), answer);
    let out = call(&client(&scratch));
    (out, server.join().expect("fake engine"))
}

fn ok(body: &Value) -> Vec<u8> {
    response("200 OK", body.to_string().as_bytes())
}

/// The bytes of C6 fixture `name` under `contracts/fixtures/c6/<sub>`.
fn fixture(sub: &str, name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/c6")
        .join(sub)
        .join(name);
    fs::read(path).expect("fixture")
}

fn some_request() -> InjectRequest {
    serde_json::from_slice(&fixture("valid", "InjectRequest.basic.json")).expect("InjectRequest")
}

fn status_of_turn() -> StatusRequest {
    serde_json::from_slice(&fixture("valid", "StatusRequest.turn.json")).expect("StatusRequest")
}

fn sent_body(request: &[u8]) -> Value {
    let end = request
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("request head");
    serde_json::from_slice(&request[end + 4..]).expect("request body")
}

#[test]
fn is_idle_posts_an_empty_object_and_returns_the_status() {
    let bytes = fixture("valid", "IdleStatus.busy.json");
    let (out, sent) = exchange(response("200 OK", &bytes), Client::is_idle);
    let want: IdleStatus = serde_json::from_slice(&bytes).expect("IdleStatus");
    assert_eq!(out.expect("is_idle"), want);
    assert!(sent.starts_with(b"POST /v1/is_idle HTTP/1.1\r\n"));
    assert!(sent.ends_with(b"\r\n\r\n{}"));
}

#[test]
fn inject_posts_the_request_as_canonical_json_and_returns_the_accept() {
    let bytes = fixture("valid", "InjectResponse.accepted.json");
    let (out, sent) = exchange(response("200 OK", &bytes), |c| c.inject(&some_request()));
    let want: InjectResponse = serde_json::from_slice(&bytes).expect("InjectResponse");
    assert_eq!(out.expect("inject"), want);
    let body = canonical(&serde_json::to_value(some_request()).expect("JSON")).expect("canonical");
    let mut expected = format!(
        "POST /v1/inject HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    expected.extend_from_slice(&body);
    assert_eq!(
        String::from_utf8_lossy(&sent),
        String::from_utf8_lossy(&expected)
    );
}

#[test]
fn status_with_a_turn_returns_the_report_of_that_turn() {
    let answer = json!({
        "session": "owner", "running": false, "since": 1, "queue_depth": 0, "route": "chat",
        "turn": {"turn": "owner:7", "state": "ended", "outcome": "completed", "ended": 2,
                 "delivery": "sent"},
    });
    let (out, sent) = exchange(ok(&answer), |c| c.status(&status_of_turn()));
    let want: SessionStatus = serde_json::from_value(answer).expect("SessionStatus");
    assert_eq!(out.expect("status"), want);
    assert_eq!(
        sent_body(&sent),
        json!({"session": "owner", "turn": "owner:7"})
    );
}

#[test]
fn an_engine_of_another_uid_is_refused_before_any_byte() {
    let scratch = Scratch::new();
    let server = serve_once(&scratch.socket(), ok(&json!({"idle": true})));
    let own = own_uid(&scratch);
    let client = Client::with_limits(scratch.socket(), TIMEOUT, TIMEOUT, own.wrapping_add(1));
    let out = client.inject(&some_request());
    let Err(Error::PeerUid { expected, actual }) = out else {
        panic!("{out:?}");
    };
    assert_eq!((expected, actual), (own.wrapping_add(1), own));
    assert_eq!(server.join().expect("fake engine"), b"");
}

#[test]
fn a_request_over_the_body_limit_is_refused_before_connecting() {
    let scratch = Scratch::new();
    let request = InjectRequest {
        prompt: "x".repeat(MAX_BODY),
        ..some_request()
    };
    let out = client(&scratch).inject(&request);
    assert!(matches!(out, Err(Error::RequestTooLarge(_))), "{out:?}");
}

#[test]
fn a_missing_socket_is_an_io_error_of_the_connect() {
    let scratch = Scratch::new();
    let out = client(&scratch).is_idle();
    assert!(
        matches!(
            &out,
            Err(Error::Io { connected: false, error }) if error.kind() == ErrorKind::NotFound
        ),
        "{out:?}"
    );
}

#[test]
fn an_engine_that_closes_while_the_request_is_written_is_an_io_error_after_the_connect() {
    let scratch = Scratch::new();
    let listener = UnixListener::bind(scratch.socket()).expect("bind");
    let server = thread::spawn(move || drop(listener.accept().expect("accept")));
    // Larger than the socket buffer, so the write is under way when the engine closes.
    let request = InjectRequest {
        prompt: "x".repeat(MAX_BODY - 1024),
        ..some_request()
    };
    let out = client(&scratch).inject(&request);
    server.join().expect("fake engine");
    assert!(
        matches!(
            out,
            Err(Error::Io {
                connected: true,
                ..
            })
        ),
        "{out:?}"
    );
}

#[test]
fn an_engine_that_never_answers_times_out_within_the_call_timeout() {
    let scratch = Scratch::new();
    let _listener = UnixListener::bind(scratch.socket()).expect("bind");
    let call = Duration::from_millis(50);
    let client = Client::with_limits(scratch.socket(), TIMEOUT, call, own_uid(&scratch));
    let started = Instant::now();
    let out = client.is_idle();
    assert!(matches!(out, Err(Error::Timeout)), "{out:?}");
    assert!(started.elapsed() < call + Duration::from_millis(350));
}

#[test]
fn a_listener_whose_backlog_stays_full_times_out_within_the_connect_timeout() {
    let scratch = Scratch::new();
    let socket = scratch.socket();
    let listener =
        rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).expect("socket");
    let address = SocketAddrUnix::new(socket.as_path()).expect("address");
    rustix::net::bind(&listener, &address).expect("bind");
    rustix::net::listen(&listener, 0).expect("listen");
    let mut queued = Vec::new();
    loop {
        let peer = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK,
            None,
        )
        .expect("peer");
        match rustix::net::connect(&peer, &address) {
            Ok(()) => queued.push(peer),
            Err(rustix::io::Errno::AGAIN) => break,
            Err(e) => panic!("connect: {e}"),
        }
        assert!(queued.len() < 1024, "the backlog never fills");
    }
    let connect = Duration::from_millis(100);
    let client = Client::with_limits(&socket, connect, TIMEOUT, own_uid(&scratch));
    let started = Instant::now();
    let out = client.is_idle();
    let elapsed = started.elapsed();
    assert!(matches!(out, Err(Error::Timeout)), "{out:?}");
    assert!(
        elapsed < connect + Duration::from_millis(300),
        "{elapsed:?}"
    );
}

#[test]
fn framing_failures_are_malformed() {
    let cases: [(&str, Vec<u8>); 6] = [
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
            "chunked body",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_vec(),
        ),
        ("not HTTP/1.1", b"HTTP/1.0 200 OK\r\n\r\n".to_vec()),
        (
            "end of stream inside the body",
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{\"idle\":".to_vec(),
        ),
    ];
    for (what, answer) in cases {
        let (out, _) = exchange(answer, Client::is_idle);
        assert!(matches!(out, Err(Error::Malformed(_))), "{what}: {out:?}");
    }
}

#[test]
fn answers_invalid_for_their_definition_are_invalid() {
    let (out, _) = exchange(
        response(
            "200 OK",
            &fixture("invalid", "IdleStatus.negative_since.json"),
        ),
        Client::is_idle,
    );
    assert!(matches!(out, Err(Error::Invalid(_))), "is_idle: {out:?}");
    let busy_with_turn = json!({"accepted": false, "retry_after_s": 15, "turn": "owner:7"});
    let (out, _) = exchange(ok(&busy_with_turn), |c| c.inject(&some_request()));
    assert!(matches!(out, Err(Error::Invalid(_))), "inject: {out:?}");
    let base = json!({"session": "owner", "running": false, "since": 1, "queue_depth": 0, "route": "chat"});
    let mut running_with_delivery: Value =
        serde_json::from_slice(&fixture("invalid", "TurnReport.running_with_delivery.json"))
            .expect("TurnReport");
    running_with_delivery["turn"] = json!("owner:7");
    let reports = [
        (
            "another turn",
            json!({"turn": "owner:8", "state": "unknown"}),
        ),
        ("delivery on a running turn", running_with_delivery),
        (
            "delivery on a dropped turn",
            json!({"turn": "owner:7", "state": "dropped", "ended": 2, "delivery": "sent"}),
        ),
        (
            "ended without an outcome",
            json!({"turn": "owner:7", "state": "ended", "ended": 2}),
        ),
    ];
    for (what, report) in reports {
        let mut answer = base.clone();
        answer["turn"] = report;
        let (out, _) = exchange(ok(&answer), |c| c.status(&status_of_turn()));
        assert!(matches!(out, Err(Error::Invalid(_))), "{what}: {out:?}");
    }
    let body = br#"{"code":"not_found","message":"no such method or path"}"#;
    let (out, _) = exchange(response("500 Internal Server Error", body), Client::is_idle);
    assert!(
        matches!(out, Err(Error::Invalid(_))),
        "status off its code: {out:?}"
    );
}

#[test]
fn an_error_answer_is_remote_with_its_code_and_message() {
    let body = br#"{"code":"bad_request","message":"session is not the owner session"}"#;
    let (out, _) = exchange(response("400 Bad Request", body), |c| {
        c.status(&status_of_turn())
    });
    let Err(Error::Remote { code, message }) = out else {
        panic!("{out:?}");
    };
    assert_eq!(code, ErrorCode::BadRequest);
    assert_eq!(message, "session is not the owner session");
}

#[test]
fn a_full_payload_of_control_characters_makes_an_inject_body_under_the_c6_limit() {
    let payload = "\u{1}".repeat(65_536);
    let packet = json!({
        "id": "ev-1", "kind": "task_result", "mac": "ab".repeat(32), "payload": payload,
        "source_ts": 1_767_225_600.25, "trust": "untrusted-source-data", "version": 1,
    });
    let row = EventRow {
        id: "ev-1".into(),
        packet: canonical(&packet).expect("canonical"),
        digest: "cd".repeat(32),
        status: "leased".into(),
        attempts: 1,
        lease: Some("0123456789abcdef0123456789abcdef".into()),
        lease_until: Some(1_767_225_720.0),
        available: 0.0,
        session_id: None,
        outcome: None,
        created: 1_767_225_600.0,
    };
    let request = InjectRequest {
        event_id: row.id.clone(),
        kind: "task_result".into(),
        prompt: model_text(&row).expect("model text"),
        lease: row.lease.clone().expect("lease"),
        attempt: 1,
    };
    let accepted = fixture("valid", "InjectResponse.accepted.json");
    let (out, sent) = exchange(response("200 OK", &accepted), |c| c.inject(&request));
    assert!(out.is_ok(), "{out:?}");
    let end = sent
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("request head");
    let body = sent.len() - end - 4;
    assert_eq!(body, content_length(&sent[..end]));
    assert!(body > 7 * 65_536 && body < MAX_BODY, "{body}");
}
