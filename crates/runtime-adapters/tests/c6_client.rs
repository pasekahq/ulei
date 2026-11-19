//! The C6 client against a fake adapter on a Unix socket, driven by the C6 fixtures.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::{Scratch, response};
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use serde_json::{Map, Value, json};
use ulei_contracts::generated::c6::{
    ErrorCode, IdleStatus, InjectRequest, InjectResponse, InterruptResponse, RouteSet,
    SessionStatus, StatusRequest, SubmitTurnResponse,
};
use ulei_runtime_adapters::client::{Client, Error, MAX_BODY};

const TIMEOUT: Duration = Duration::from_secs(2);

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
        let accepted = br#"{"accepted":true,"session_id":"owner","turn":"owner:7"}"#;
        let (out, sent) = exchange(response("200 OK", accepted), |c| c.inject(&request));
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
        prompt: "x".into(),
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
        prompt: "x".repeat(MAX_BODY),
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

/// `(method, input definition, output definition)` of the session methods.
const SESSION_METHODS: [(&str, &str, &str); 4] = [
    ("submit_turn", "SubmitTurnRequest", "SubmitTurnResponse"),
    ("interrupt", "InterruptRequest", "InterruptResponse"),
    ("set_route", "SetRouteRequest", "RouteSet"),
    ("status", "StatusRequest", "SessionStatus"),
];

/// Calls the typed client method `method` with `input` parsed into its request; the answer as
/// JSON.
fn call_typed(client: &Client, method: &str, input: Value) -> Result<Value, Error> {
    let json = |answer: serde_json::Result<Value>| answer.expect("answer JSON");
    match method {
        "inject" => client
            .inject(&serde_json::from_value(input).expect("InjectRequest"))
            .map(|a| json(serde_json::to_value(a))),
        "submit_turn" => client
            .submit_turn(&serde_json::from_value(input).expect("SubmitTurnRequest"))
            .map(|a| json(serde_json::to_value(a))),
        "interrupt" => client
            .interrupt(&serde_json::from_value(input).expect("InterruptRequest"))
            .map(|a| json(serde_json::to_value(a))),
        "set_route" => client
            .set_route(&serde_json::from_value(input).expect("SetRouteRequest"))
            .map(|a| json(serde_json::to_value(a))),
        "status" => client
            .status(&serde_json::from_value(input).expect("StatusRequest"))
            .map(|a| json(serde_json::to_value(a))),
        other => panic!("no typed method {other}"),
    }
}

/// `bytes` of output definition `def` parsed into its type and back to JSON.
fn typed_value(def: &str, bytes: &[u8]) -> Value {
    let value = match def {
        "SubmitTurnResponse" => {
            serde_json::to_value(serde_json::from_slice::<SubmitTurnResponse>(bytes).expect(def))
        }
        "InterruptResponse" => {
            serde_json::to_value(serde_json::from_slice::<InterruptResponse>(bytes).expect(def))
        }
        "RouteSet" => serde_json::to_value(serde_json::from_slice::<RouteSet>(bytes).expect(def)),
        "SessionStatus" => {
            serde_json::to_value(serde_json::from_slice::<SessionStatus>(bytes).expect(def))
        }
        other => panic!("no output definition {other}"),
    };
    value.expect("JSON")
}

/// A valid answer of `method` to `input`: a `status` answer reports the turn `input` names.
fn answer_to(method: &str, output: &str, input: &Value) -> Vec<u8> {
    if method != "status" {
        return fixtures("valid", output).remove(0).1;
    }
    let mut status = json!({
        "session": input["session"],
        "running": false,
        "since": 0,
        "queue_depth": 0,
        "route": "chat",
    });
    if let Some(turn) = input.get("turn") {
        status["turn"] = json!({"turn": turn, "state": "unknown"});
    }
    status.to_string().into_bytes()
}

/// A request of `method` that a valid answer fixture `answer` answers.
fn request_for(method: &str, input: &str, answer: &Value) -> Value {
    if method != "status" {
        return json_of(&fixtures("valid", input).remove(0).1);
    }
    let mut request = json!({"session": answer["session"]});
    if let Some(report) = answer.get("turn") {
        request["turn"] = report["turn"].clone();
    }
    request
}

#[test]
fn session_methods_send_each_valid_request_fixture_as_canonical_json() {
    for (method, input, output) in SESSION_METHODS {
        for (name, bytes) in fixtures("valid", input) {
            let fixture = json_of(&bytes);
            let answer = answer_to(method, output, &fixture);
            let (out, sent) = exchange(response("200 OK", &answer), |c| {
                call_typed(c, method, fixture.clone())
            });
            assert!(out.is_ok(), "{name}: {out:?}");
            assert_eq!(
                String::from_utf8_lossy(&sent),
                String::from_utf8_lossy(&expected_request(method, &canonical(&fixture))),
                "{name}"
            );
        }
    }
}

#[test]
fn each_valid_session_answer_fixture_parses_to_the_same_value() {
    for (method, input, output) in SESSION_METHODS {
        for (name, bytes) in fixtures("valid", output) {
            let request = request_for(method, input, &json_of(&bytes));
            let (out, _) = exchange(response("200 OK", &bytes), |c| {
                call_typed(c, method, request)
            });
            assert_eq!(out.expect(&name), typed_value(output, &bytes), "{name}");
        }
    }
}

#[test]
fn each_invalid_session_answer_fixture_is_refused_as_invalid() {
    for (method, input, output) in SESSION_METHODS {
        for (name, bytes) in fixtures("invalid", output) {
            let request = request_for(method, input, &json!({"session": "owner"}));
            let (out, _) = exchange(response("200 OK", &bytes), |c| {
                call_typed(c, method, request)
            });
            assert!(matches!(out, Err(Error::Invalid(_))), "{name}: {out:?}");
        }
    }
}

/// Puts a value into a `SessionStatus` answer.
type Place = fn(&mut Value, Value);

#[test]
fn an_invalid_definition_fixture_inside_a_session_status_is_refused_as_invalid() {
    let base = json_of(&fixtures("valid", "SessionStatus.fresh").remove(0).1);
    let ended = json!({"turn": "owner:7", "state": "ended", "outcome": "completed", "ended": 1});
    let places: [(&str, Place); 7] = [
        ("SessionKey", |s, v| s["session"] = v),
        ("RouteAlias", |s, v| s["route"] = v),
        ("TurnReport", |s, v| s["last_turn"] = v),
        ("TurnId", |s, v| s["last_turn"]["turn"] = v),
        ("TurnState", |s, v| s["last_turn"]["state"] = v),
        ("TurnOutcome", |s, v| s["last_turn"]["outcome"] = v),
        ("Delivery", |s, v| s["last_turn"]["delivery"] = v),
    ];
    for (def, place) in places {
        for (name, bytes) in fixtures("invalid", def) {
            let mut status = base.clone();
            status["last_turn"] = ended.clone();
            place(&mut status, json_of(&bytes));
            let (out, _) = exchange(response("200 OK", status.to_string().as_bytes()), |c| {
                c.status(&StatusRequest {
                    session: "owner".into(),
                    turn: None,
                })
            });
            assert!(matches!(out, Err(Error::Invalid(_))), "{name}: {out:?}");
        }
    }
}

#[test]
fn each_valid_turn_report_fixture_inside_a_session_status_parses_to_the_same_value() {
    let base = json_of(&fixtures("valid", "SessionStatus.fresh").remove(0).1);
    for (name, bytes) in fixtures("valid", "TurnReport") {
        let report = json_of(&bytes);
        let mut status = base.clone();
        status["turn"] = report.clone();
        let (out, _) = exchange(response("200 OK", status.to_string().as_bytes()), |c| {
            c.status(&StatusRequest {
                session: "owner".into(),
                turn: report["turn"].as_str().map(str::to_string),
            })
        });
        let got = serde_json::to_value(out.expect(&name).turn).expect("JSON");
        assert_eq!(got, report, "{name}");
    }
}

/// Asserts that `method` refuses each `(what, answer)` to `request` as invalid.
fn refused(method: &str, request: &Value, answers: Vec<(&str, Value)>) {
    for (what, answer) in answers {
        let (out, _) = exchange(response("200 OK", answer.to_string().as_bytes()), |c| {
            call_typed(c, method, request.clone())
        });
        assert!(matches!(out, Err(Error::Invalid(_))), "{what}: {out:?}");
    }
}

#[test]
fn turn_and_route_answers_that_break_a_rule_the_types_do_not_encode_are_invalid() {
    let owner = json!({"session": "owner"});
    refused(
        "submit_turn",
        &json!({"session": "owner", "prompt": "p"}),
        vec![
            (
                "accepted without a turn",
                json!({"accepted": true, "position": 0, "queue_depth": 0}),
            ),
            (
                "refused with a turn",
                json!({"accepted": false, "turn": "t1", "queue_depth": 32}),
            ),
            (
                "position over the cap",
                json!({"accepted": true, "turn": "t1", "position": 33, "queue_depth": 0}),
            ),
            (
                "turn id with a slash",
                json!({"accepted": true, "turn": "t/1", "position": 0, "queue_depth": 0}),
            ),
        ],
    );
    refused(
        "interrupt",
        &owner,
        vec![
            (
                "dropped without an interrupt",
                json!({"interrupted": false, "dropped": "t2", "queue_depth": 0}),
            ),
            (
                "interrupted without a turn",
                json!({"interrupted": true, "queue_depth": 0}),
            ),
            (
                "queue over the cap",
                json!({"interrupted": false, "queue_depth": 33}),
            ),
        ],
    );
    refused(
        "set_route",
        &json!({"session": "owner", "route": "deep"}),
        vec![
            (
                "route with a slash",
                json!({"session": "owner", "route": "deep/model"}),
            ),
            ("empty session", json!({"session": "", "route": "deep"})),
        ],
    );
    refused(
        "inject",
        &serde_json::to_value(some_request()).expect("InjectRequest"),
        vec![
            (
                "turn id with a space",
                json!({"accepted": true, "session_id": "owner", "turn": "owner 7"}),
            ),
            (
                "accepted without a session",
                json!({"accepted": true, "turn": "owner:7"}),
            ),
            (
                "accepted without a turn",
                json!({"accepted": true, "session_id": "owner"}),
            ),
            (
                "accepted with a retry",
                json!({"accepted": true, "session_id": "owner", "turn": "owner:7", "retry_after_s": 30}),
            ),
            ("busy without a retry", json!({"accepted": false})),
            (
                "busy with a turn",
                json!({"accepted": false, "retry_after_s": 30, "turn": "owner:7"}),
            ),
        ],
    );
}

#[test]
fn status_answers_that_break_a_rule_the_types_do_not_encode_are_invalid() {
    let status = |turn: Option<Value>, since: i64| {
        let mut status = json!({"session": "owner", "running": false, "since": since, "queue_depth": 0, "route": "chat"});
        if let Some(turn) = turn {
            status["turn"] = turn;
        }
        status
    };
    let named = |report: Value| status(Some(report), 1);
    refused(
        "status",
        &json!({"session": "owner", "turn": "t1"}),
        vec![
            ("no turn when asked", status(None, 1)),
            (
                "another turn",
                named(json!({"turn": "t2", "state": "unknown"})),
            ),
            (
                "ended without an outcome",
                named(json!({"turn": "t1", "state": "ended", "ended": 2})),
            ),
            (
                "ended without its end",
                named(json!({"turn": "t1", "state": "ended", "outcome": "completed"})),
            ),
            (
                "dropped without its end",
                named(json!({"turn": "t1", "state": "dropped"})),
            ),
            (
                "running with an outcome",
                named(json!({"turn": "t1", "state": "running", "outcome": "completed"})),
            ),
            (
                "a line on a completed turn",
                named(
                    json!({"turn": "t1", "state": "ended", "outcome": "completed", "ended": 2, "line": "x"}),
                ),
            ),
            (
                "a negative end",
                named(json!({"turn": "t1", "state": "dropped", "ended": -1})),
            ),
        ],
    );
    refused(
        "status",
        &json!({"session": "owner"}),
        vec![
            (
                "turn when not asked",
                named(json!({"turn": "t1", "state": "unknown"})),
            ),
            ("a negative since", status(None, -1)),
        ],
    );
}

#[test]
fn a_last_turn_read_back_from_the_journal_has_no_outcome_and_no_end() {
    let answer = json!({
        "session": "owner", "running": false, "since": 1, "queue_depth": 0, "route": "chat",
        "last_turn": {"turn": "s1-t3", "state": "ended"},
    });
    let (out, _) = exchange(response("200 OK", answer.to_string().as_bytes()), |c| {
        c.status(&StatusRequest {
            session: "owner".into(),
            turn: None,
        })
    });
    let last = out.expect("status").last_turn.expect("last turn");
    assert_eq!((last.outcome, last.ended), (None, None));
}

#[test]
fn bytes_that_follow_the_body_in_a_later_write_are_malformed() {
    let scratch = Scratch::new();
    let listener = UnixListener::bind(scratch.socket()).expect("bind");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        while !whole_request(&request) {
            let n = stream.read(&mut chunk).expect("request");
            request.extend_from_slice(&chunk[..n]);
        }
        stream
            .write_all(&response("200 OK", br#"{"idle":true}"#))
            .expect("answer");
        thread::sleep(Duration::from_millis(50));
        let _ = stream.write_all(b"{}");
    });
    let client = Client::new(scratch.socket(), TIMEOUT).expect("client");
    let out = client.is_idle();
    assert!(matches!(out, Err(Error::Malformed(_))), "{out:?}");
    server.join().expect("fake adapter");
}

#[test]
fn a_listener_whose_backlog_stays_full_times_out_within_the_call_timeout() {
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
    let timeout = Duration::from_millis(100);
    let client = Client::new(&socket, timeout).expect("client");
    let started = Instant::now();
    let out = client.is_idle();
    let elapsed = started.elapsed();
    assert!(matches!(out, Err(Error::Timeout)), "{out:?}");
    assert!(
        elapsed < timeout + Duration::from_millis(300),
        "{elapsed:?}"
    );
}

#[test]
fn a_method_name_outside_the_c6_names_is_refused_before_connecting() {
    let scratch = Scratch::new();
    let client = Client::new(scratch.socket(), TIMEOUT).expect("client");
    for method in ["", "is idle", "is_idle\r\nX: y", "Status", "../inject"] {
        let out = client.call(method, &json!({}));
        assert!(
            matches!(&out, Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::InvalidInput),
            "{method:?}: {out:?}"
        );
    }
}
