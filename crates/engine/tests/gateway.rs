//! The C4G gateway client against the scripted fake gateway, driven by the C4G fixtures.

mod fake_gateway;

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use fake_gateway::{Answer, FakeGateway, STREAM_HEAD, chunk, json_bytes, line};
use serde_json::Value;
use ulei_contracts::generated::c4g::{
    CancelAnswer, Error as GatewayError, ErrorClass, EventType, RoutesAnswer, StopReason,
    StreamEvent, TurnRequest,
};
use ulei_engine::gateway::{Client, Error, MAX_HEAD, MAX_LINE, Malformed};

const SLOW: Duration = Duration::from_secs(2);
const SHORT: Duration = Duration::from_millis(30);

fn fixture(sub: &str, name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/c4g")
        .join(sub)
        .join(format!("{name}.json"));
    fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn event(name: &str) -> StreamEvent {
    serde_json::from_slice(&fixture("valid", &format!("StreamEvent.{name}"))).expect(name)
}

fn request(name: &str) -> TurnRequest {
    serde_json::from_slice(&fixture("valid", &format!("TurnRequest.{name}"))).expect(name)
}

fn client(fake: &FakeGateway) -> Client {
    Client::new(fake.socket(), SLOW, SLOW)
}

/// Runs one turn of `request("minimal")` against `answer`; every item of the stream.
fn run(answer: Answer) -> Vec<Result<StreamEvent, Error>> {
    let fake = FakeGateway::start(vec![answer]);
    client(&fake)
        .turn(&request("minimal"))
        .expect("turn")
        .collect()
}

fn events(items: Vec<Result<StreamEvent, Error>>) -> Vec<StreamEvent> {
    items.into_iter().map(|item| item.expect("event")).collect()
}

/// The single error of a turn that yields `ok` events before it.
fn failure(answer: Answer, ok: usize) -> Error {
    let mut items = run(answer);
    assert_eq!(items.len(), ok + 1, "{items:?}");
    match items.pop() {
        Some(Err(e)) => e,
        other => panic!("expected an error, got {other:?}"),
    }
}

fn expected_turn_head(body_len: usize) -> Vec<u8> {
    format!(
        "POST /v1/model/turn HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

fn stop(reason: StopReason) -> StreamEvent {
    StreamEvent {
        reason: Some(reason),
        ..event("stop")
    }
}

#[test]
fn a_text_turn_posts_each_request_fixture_and_streams_its_events() {
    let script = vec![
        event("route"),
        event("text_delta"),
        event("thinking"),
        event("usage"),
        stop(StopReason::End),
    ];
    for name in ["minimal", "all_blocks"] {
        let fake = FakeGateway::start(vec![Answer::Events(script.clone())]);
        let got = events(client(&fake).turn(&request(name)).expect(name).collect());
        assert_eq!(got, script, "{name}");

        let sent = fake.requests();
        assert_eq!(sent.len(), 1, "{name}");
        assert_eq!(
            String::from_utf8_lossy(&sent[0].head),
            String::from_utf8_lossy(&expected_turn_head(sent[0].body.len())),
            "{name}"
        );
        let body: Value = serde_json::from_slice(&sent[0].body).expect("body is JSON");
        let want: Value =
            serde_json::from_slice(&fixture("valid", &format!("TurnRequest.{name}"))).expect(name);
        assert_eq!(body, want, "{name}");
    }
}

#[test]
fn a_tool_call_turn_yields_the_call_and_stops_for_tool_use() {
    let script = vec![
        event("route"),
        event("tool_call"),
        stop(StopReason::ToolUse),
    ];
    let got = events(run(Answer::Events(script.clone())));
    assert_eq!(got, script);
    let call = got[1].tool_call.as_ref().expect("tool_call payload");
    assert_eq!((call.id.as_str(), call.name.as_str()), ("call_02", "read"));
    assert_eq!(call.input.get("path"), Some(&Value::from("notes.md")));
}

#[test]
fn a_terminal_error_event_of_each_class_ends_the_turn_intact() {
    for (name, class) in [
        ("error_quota_exhausted", ErrorClass::QuotaExhausted),
        ("error_auth", ErrorClass::Auth),
        ("error_unavailable", ErrorClass::Unavailable),
        ("error_overloaded", ErrorClass::Overloaded),
        ("error_bad_request", ErrorClass::BadRequest),
        ("error_context_too_long", ErrorClass::ContextTooLong),
        ("error_no_route", ErrorClass::NoRoute),
        ("error_cancelled", ErrorClass::Cancelled),
    ] {
        // Events after the terminal one are never yielded.
        let script = vec![event("route"), event(name), event("text_delta")];
        let got = events(run(Answer::Events(script)));
        assert_eq!(got, vec![event("route"), event(name)], "{name}");
        let error = got[1].error.as_ref().expect("error payload");
        assert_eq!(got[1].r#type, EventType::Error, "{name}");
        assert_eq!(error.class, class, "{name}");
        match class {
            ErrorClass::QuotaExhausted => assert_eq!(error.resets_at, Some(1_795_000_000)),
            ErrorClass::Overloaded => assert_eq!(error.retry_after, Some(30)),
            _ => {}
        }
    }
}

#[test]
fn a_pre_stream_rejection_is_typed_and_ends_the_turn() {
    let error: GatewayError =
        serde_json::from_slice(&fixture("valid", "Error.rejected")).expect("Error fixture");
    let items = run(Answer::Reject(400, error.clone()));
    assert_eq!(items.len(), 1, "{items:?}");
    match &items[0] {
        Err(Error::Rejected { status, error: got }) => {
            assert_eq!(*status, 400);
            assert_eq!(*got, error);
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// The fixture `sub/name` as one compact line, with every field it holds.
fn fixture_line(sub: &str, name: &str) -> Vec<u8> {
    let value: Value = serde_json::from_slice(&fixture(sub, name)).expect(name);
    let mut out = value.to_string().into_bytes();
    out.push(b'\n');
    out
}

#[test]
fn answers_with_an_unknown_optional_field_parse() {
    let items = run(Answer::Chunks(vec![
        fixture_line("valid", "StreamEvent.extra_route_field"),
        fixture_line("valid", "StreamEvent.extra_usage_field"),
        line(&stop(StopReason::End)),
    ]));
    let got = events(items);
    let route = got[0].route.as_ref().expect("route payload");
    assert_eq!(route.lease, "l-17");
    let usage = got[1].usage.as_ref().expect("usage payload");
    assert_eq!((usage.input, usage.cache_read), (120, 2048));
    assert_eq!(got[2], stop(StopReason::End));

    let rejected = json_bytes(429, &fixture("valid", "Error.extra_field"));
    match failure(Answer::Raw(rejected), 0) {
        Error::Rejected { status, error } => {
            assert_eq!(status, 429);
            assert_eq!(
                (error.class, error.retry_after),
                (ErrorClass::Overloaded, Some(1))
            );
        }
        other => panic!("expected a rejection, got {other:?}"),
    }

    let fake = FakeGateway::start(vec![Answer::Hang(vec![event("route")])]);
    fake.answer_once(
        "GET /v1/model/routes",
        json_bytes(200, &fixture("valid", "RoutesAnswer.extra_field")),
    );
    fake.answer_once(
        "POST /v1/model/cancel",
        json_bytes(200, &fixture("valid", "CancelAnswer.extra_field")),
    );
    let routes = client(&fake).routes().expect("routes");
    assert_eq!(routes.routes[0].alias, "chat");
    let mut turn = client(&fake).turn(&request("minimal")).expect("turn");
    assert_eq!(turn.next().expect("route").expect("route"), event("route"));
    let answer = turn.cancel_handle().cancel().expect("cancel");
    assert_eq!(answer, CancelAnswer { cancelled: true });
}

#[test]
fn lines_are_reassembled_across_arbitrary_chunk_boundaries() {
    let script = [event("route"), event("text_delta"), stop(StopReason::End)];
    let wire: Vec<u8> = script.iter().flat_map(line).collect();
    for size in [1, 2, 7, 64, wire.len()] {
        let chunks = wire.chunks(size).map(<[u8]>::to_vec).collect();
        assert_eq!(
            events(run(Answer::Chunks(chunks))),
            script,
            "chunks of {size}"
        );
    }
}

#[test]
fn a_stream_without_a_terminal_event_is_cut() {
    let open = vec![event("route"), event("text_delta")];
    assert!(matches!(failure(Answer::Events(open), 2), Error::Cut));

    let mut raw = STREAM_HEAD.to_vec();
    raw.extend_from_slice(&chunk(&line(&event("route"))));
    raw.extend_from_slice(b"40\r\n{\"type\":");
    assert!(matches!(failure(Answer::Raw(raw), 1), Error::Cut));
}

#[test]
fn a_silent_gateway_ends_the_turn_with_a_timeout_within_the_idle_bound() {
    let fake = FakeGateway::start(vec![Answer::Hang(vec![event("route")])]);
    let mut turn = Client::new(fake.socket(), SLOW, SHORT)
        .turn(&request("minimal"))
        .expect("turn");
    assert_eq!(turn.next().expect("route").expect("route"), event("route"));
    let start = Instant::now();
    assert!(matches!(turn.next(), Some(Err(Error::Timeout))));
    let waited = start.elapsed();
    assert!(waited >= SHORT && waited < SHORT * 10, "{waited:?}");
    assert!(turn.next().is_none());
}

#[test]
fn cancel_from_another_thread_ends_the_stream_the_gateway_keeps_open() {
    let fake = FakeGateway::start(vec![Answer::Hang(vec![event("route")])]);
    let turn_request = request("minimal");
    let mut turn = client(&fake).turn(&turn_request).expect("turn");
    assert_eq!(turn.next().expect("route").expect("route"), event("route"));

    let handle = turn.cancel_handle();
    let canceller = thread::spawn(move || {
        thread::sleep(SHORT);
        handle.cancel()
    });
    let start = Instant::now();
    assert!(matches!(turn.next(), Some(Err(Error::Cancelled))));
    assert!(start.elapsed() < SLOW / 2, "{:?}", start.elapsed());
    assert!(turn.next().is_none());

    let answer = canceller.join().expect("canceller").expect("cancel");
    assert_eq!(answer, CancelAnswer { cancelled: true });
    let sent = fake.requests();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].target(), "POST /v1/model/cancel");
    let body: Value = serde_json::from_slice(&sent[1].body).expect("cancel body");
    assert_eq!(
        body,
        serde_json::json!({ "request_id": turn_request.request_id })
    );
}

#[test]
fn malformed_streams_are_distinct_typed_errors() {
    let malformed = |answer, ok| match failure(answer, ok) {
        Error::Malformed(kind) => kind,
        other => panic!("expected Malformed, got {other:?}"),
    };
    let route = chunk(&line(&event("route")));
    let raw = |tail: &[u8]| {
        let mut out = STREAM_HEAD.to_vec();
        out.extend_from_slice(&route);
        out.extend_from_slice(tail);
        Answer::Raw(out)
    };

    assert_eq!(malformed(raw(b"zz\r\n"), 1), Malformed::ChunkSize);
    assert_eq!(malformed(raw(&[b'1'; 80]), 1), Malformed::ChunkSize);
    assert_eq!(malformed(raw(b"1\r\n{xx"), 1), Malformed::ChunkEnd);
    assert_eq!(
        malformed(Answer::Chunks(vec![b"not json\n".to_vec()]), 0),
        Malformed::NotJson
    );
    for name in [
        "unknown_type",
        "unknown_error_class",
        "stop_unknown_reason",
        "tool_call_without_id",
    ] {
        assert_eq!(
            malformed(
                Answer::Chunks(vec![fixture_line(
                    "invalid",
                    &format!("StreamEvent.{name}")
                )]),
                0
            ),
            Malformed::UnknownEvent,
            "{name}"
        );
    }
    for name in ["payload_of_other_type", "two_payloads"] {
        assert_eq!(
            malformed(
                Answer::Chunks(vec![fixture_line(
                    "invalid",
                    &format!("StreamEvent.{name}")
                )]),
                0
            ),
            Malformed::WrongPayload,
            "{name}"
        );
    }
    assert_eq!(
        malformed(Answer::Chunks(vec![vec![b' '; MAX_LINE + 1]]), 0),
        Malformed::LineTooLarge
    );

    let mut big_head = b"HTTP/1.1 200 OK\r\nX-Pad: ".to_vec();
    big_head.resize(MAX_HEAD + 16, b'a');
    big_head.extend_from_slice(b"\r\n\r\n");
    assert_eq!(malformed(Answer::Raw(big_head), 0), Malformed::HeadTooLarge);
    assert!(matches!(
        malformed(
            Answer::Raw(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec()),
            0
        ),
        Malformed::Head(_)
    ));
    assert!(matches!(
        malformed(
            Answer::Raw(b"HTTP/1.1 503 Busy\r\nContent-Length: 2\r\n\r\n{}".to_vec()),
            0
        ),
        Malformed::Body(_)
    ));
}

#[test]
fn routes_gets_the_routes_answer() {
    let routes: RoutesAnswer =
        serde_json::from_slice(&fixture("valid", "RoutesAnswer.basic")).expect("routes");
    let fake = FakeGateway::with_routes(Vec::new(), routes.clone());
    assert_eq!(client(&fake).routes().expect("routes"), routes);
    let sent = fake.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        String::from_utf8_lossy(&sent[0].head),
        "GET /v1/model/routes HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(sent[0].body, b"");
}

#[test]
fn a_full_listen_backlog_ends_the_connect_with_a_timeout() {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};

    let fake = FakeGateway::start(Vec::new());
    let socket = fake.socket().with_file_name("full.sock");
    let listener =
        rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).expect("socket");
    rustix::net::bind(
        &listener,
        &SocketAddrUnix::new(socket.as_path()).expect("addr"),
    )
    .expect("bind");
    rustix::net::listen(&listener, 0).expect("listen");
    let _queued = UnixStream::connect(&socket).expect("first connect fills the backlog");

    let start = Instant::now();
    let out = Client::new(&socket, SHORT, SLOW).routes();
    let waited = start.elapsed();
    assert!(matches!(out, Err(Error::Timeout)), "{out:?}");
    assert!(waited >= SHORT && waited < SLOW / 2, "{waited:?}");
}

#[test]
fn a_missing_socket_is_an_io_error() {
    let fake = FakeGateway::start(Vec::new());
    let out = Client::new(fake.socket().with_file_name("absent.sock"), SLOW, SLOW)
        .turn(&request("minimal"));
    assert!(matches!(out, Err(Error::Io(_))));
}

/// `request("minimal")` with a text of 8 MiB, far over a socket's send buffer.
fn large_request() -> TurnRequest {
    let mut request = request("minimal");
    request.messages[0].content[0].text = Some("x".repeat(8 << 20));
    request
}

/// The first item of a large turn whose head the fake answers with `answer` and then closes on.
fn answered_from_the_head(answer: Vec<u8>) -> Result<StreamEvent, Error> {
    let fake = FakeGateway::start(Vec::new());
    fake.answer_after_head(answer);
    let mut turn = client(&fake).turn(&large_request()).expect("turn");
    let first = turn.next().expect("an item");
    assert!(turn.next().is_none());
    let sent = fake.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].target(), "POST /v1/model/turn");
    first
}

#[test]
fn an_answer_sent_before_the_body_was_read_is_classified_as_after_a_full_write() {
    let overloaded = GatewayError {
        retry_after: Some(7),
        ..error(ErrorClass::Overloaded)
    };
    let body = serde_json::to_vec(&overloaded).expect("error JSON");
    match answered_from_the_head(json_bytes(429, &body)) {
        Err(Error::Rejected { status, error }) => {
            assert_eq!(status, 429);
            assert_eq!(error, overloaded);
        }
        other => panic!("expected an overloaded rejection, got {other:?}"),
    }

    let too_large = error(ErrorClass::BadRequest);
    let body = serde_json::to_vec(&too_large).expect("error JSON");
    match answered_from_the_head(json_bytes(413, &body)) {
        Err(Error::Rejected { status, error }) => {
            assert_eq!(status, 413);
            assert_eq!(error, too_large);
        }
        other => panic!("expected a 413 rejection, got {other:?}"),
    }
}

#[test]
fn a_gateway_that_closes_on_the_head_without_an_answer_is_an_io_error() {
    let out = answered_from_the_head(Vec::new());
    assert!(matches!(out, Err(Error::Io(_))), "{out:?}");
}

fn error(class: ErrorClass) -> GatewayError {
    GatewayError {
        class,
        retryable: false,
        retry_after: None,
        resets_at: None,
        tried: None,
        message: Some("refused from the head".into()),
    }
}
