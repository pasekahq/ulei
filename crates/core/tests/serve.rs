//! `ulei-inbox serve` and `ulei inbox` through the built binaries and raw bytes on the socket;
//! the peer check and the read timeout through `ulei_core::serve::Server` on a socketpair.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::daemon::{
    Daemon, Started, c1, call, exchange, launch, parse, post_raw, read_all, status_field,
};
use common::{Scratch, golden, hex, sign, unhex};
use serde_json::{Value, json};
use ulei_contracts::generated::c1::{
    Applied, ClaimResponse, Error as WireError, ErrorCode, GetResponse, Health, PublishResponse,
    Row, Status,
};
use ulei_core::inbox::{Inbox, canonical, own_uid};
use ulei_core::serve::{READ_TIMEOUT, Server};

const INBOX: &str = env!("CARGO_BIN_EXE_ulei-inbox");
const ULEI: &str = env!("CARGO_BIN_EXE_ulei");

fn start(scratch: &Scratch, root: &Path) -> Daemon {
    match launch(INBOX, &[], root, &scratch.socket(), None) {
        Started::Ready(daemon) => daemon,
        Started::Exited(status, stderr) => panic!("daemon exited {status}: {stderr}"),
    }
}

fn refused(root: &Path, socket: &Path) -> String {
    match launch(INBOX, &[], root, socket, None) {
        Started::Ready(_) => panic!("daemon started"),
        Started::Exited(status, stderr) => {
            assert!(!status.success(), "{stderr}");
            assert!(stderr.starts_with("ulei-inbox: "), "{stderr}");
            stderr
        }
    }
}

fn error_code(socket: &Path, op: &str, body: &Value, status: u16) -> ErrorCode {
    call::<WireError>(socket, op, body, status).code
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The patterns of C1 `Row` that serde does not check.
fn check_row(row: &Row) {
    assert!(is_hex(&row.digest, 64), "digest");
    assert!(row.lease.as_deref().is_none_or(|l| is_hex(l, 32)), "lease");
    assert!(is_hex(&row.packet.mac, 64), "mac");
    assert_eq!(row.id, row.packet.id);
}

fn golden_vectors() -> (Vec<u8>, Vec<Value>) {
    let file = fs::read(golden("vectors.json")).expect("vectors");
    let golden: Value = serde_json::from_slice(&file).expect("json");
    let key = unhex(golden["key_hex"].as_str().expect("key_hex"));
    (key, golden["vectors"].as_array().expect("vectors").clone())
}

/// The golden vector `name` as a packet with its `mac`.
fn vector(vectors: &[Value], name: &str) -> Value {
    let v = vectors
        .iter()
        .find(|v| v["name"] == name)
        .expect("vector name");
    let mut packet = v["packet"].clone();
    packet["mac"] = v["mac"].clone();
    packet
}

fn claimed(socket: &Path, id: &str) -> Row {
    let row = call::<ClaimResponse>(socket, "claim", &json!({ "ttl": 60 }), 200)
        .row
        .expect("claimed row");
    check_row(&row);
    assert_eq!((row.id.as_str(), row.status), (id, Status::Leased));
    row
}

fn get(socket: &Path, id: &str) -> Row {
    let row = call::<GetResponse>(socket, "get", &json!({ "id": id }), 200)
        .row
        .expect("stored row");
    check_row(&row);
    row
}

fn ok(socket: &Path, op: &str, body: &Value) -> bool {
    call::<Applied>(socket, op, body, 200).ok
}

#[test]
fn every_operation_maps_to_the_store_end_to_end() {
    let scratch = Scratch::new();
    let (key, vectors) = golden_vectors();
    let daemon = start(&scratch, &scratch.root_with_key(&key));
    let s = daemon.socket.as_path();

    let health: Health = call(s, "health", &json!({}), 200);
    assert!(health.ok);
    assert_eq!(
        health.version.as_deref(),
        Some(format!("ulei-inbox {}", ulei_core::VERSION).as_str())
    );

    let first = vector(&vectors, "int_source_ts");
    let request = json!({ "packet": first });
    for _ in 0..2 {
        let stored: PublishResponse = call(s, "publish", &request, 200);
        assert_eq!(stored.id, "golden:int");
    }
    let mut other = first.clone();
    other["payload"] = json!("Other content.");
    other.as_object_mut().expect("packet").remove("mac");
    let other = json!({ "packet": sign(&key, &other) });
    assert_eq!(error_code(s, "publish", &other, 409), ErrorCode::Conflict);

    let row = get(s, "golden:int");
    assert_eq!(
        (row.status, row.attempts, row.lease.as_ref()),
        (Status::Pending, 0, None)
    );
    let get_body = post_raw(s, "get", br#"{"id":"golden:int"}"#).body;
    let packet = &serde_json::from_slice::<Value>(&get_body).expect("json")["row"]["packet"];
    let stored_hex = hex(&canonical(packet).expect("canonical"));
    let golden = vectors.iter().find(|v| v["name"] == "int_source_ts");
    assert_eq!(stored_hex, golden.expect("vector")["stored_hex"]);
    let unknown: GetResponse = call(s, "get", &json!({ "id": "golden:none" }), 200);
    assert_eq!(unknown.row, None);

    let row = claimed(s, "golden:int");
    let lease = row.lease.clone().expect("lease");
    let held = json!({ "id": "golden:int", "lease": lease });
    let mut start = held.clone();
    start["session_id"] = json!("session:e2e");
    assert!(ok(s, "start", &start));
    assert!(ok(s, "renew", &held));
    let mut ack = held.clone();
    ack["outcome"] = json!("success");
    assert!(ok(s, "ack", &ack));
    assert!(!ok(s, "ack", &ack), "settled event acked twice");
    let row = get(s, "golden:int");
    assert_eq!(
        (row.status, row.session_id.as_deref()),
        (Status::Done, Some("session:e2e"))
    );
    let empty: ClaimResponse = call(s, "claim", &json!({}), 200);
    assert_eq!(empty.row, None);

    let second = json!({ "packet": vector(&vectors, "float_fraction") });
    let stored: PublishResponse = call(s, "publish", &second, 200);
    let lease = claimed(s, &stored.id).lease.expect("lease");
    let held = json!({ "id": stored.id, "lease": lease, "delay": 0 });
    assert!(ok(s, "retry", &held));
    assert_eq!(get(s, &stored.id).status, Status::Pending);
    let row = claimed(s, &stored.id);
    assert_eq!(row.attempts, 2);
    let held = json!({ "id": stored.id, "lease": row.lease.expect("lease") });
    let mut start = held.clone();
    start["session_id"] = json!("session:e2e-2");
    assert!(ok(s, "start", &start));
    let mut ack = held;
    ack["outcome"] = json!("deferred");
    assert!(ok(s, "ack", &ack));
    let row = get(s, &stored.id);
    assert_eq!(row.status, Status::Deferred);
    assert!(row.available >= row.created + 3600.0);

    let mut forged = vector(&vectors, "slash");
    forged["mac"] = json!("0".repeat(64));
    let forged = json!({ "packet": forged });
    assert_eq!(
        error_code(s, "publish", &forged, 401),
        ErrorCode::Unauthenticated
    );
    let exponent = json!({ "packet": vector(&vectors, "float_exponent") });
    assert_eq!(
        error_code(s, "publish", &exponent, 422),
        ErrorCode::InvalidPacket
    );
    let none: GetResponse = call(s, "get", &json!({ "id": "golden:float-1e-5" }), 200);
    assert_eq!(none.row, None);
}

#[test]
fn framing_and_limits_answer_with_a_c1_error() {
    let scratch = Scratch::new();
    let daemon = start(&scratch, &scratch.root());
    let s = daemon.socket.as_path();
    let long_header = format!(
        "POST /v1/health HTTP/1.1\r\nX-Pad: {}\r\nContent-Length: 2\r\n\r\n{{}}",
        "a".repeat(8 * 1024)
    );
    let cases: Vec<(&str, Vec<u8>, u16, ErrorCode)> = vec![
        ("one-token request line", b"POST\r\n\r\n".to_vec(), 400, ErrorCode::BadRequest),
        (
            "HTTP/1.0",
            b"POST /v1/health HTTP/1.0\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            400,
            ErrorCode::BadRequest,
        ),
        (
            "header without colon",
            b"POST /v1/health HTTP/1.1\r\nbroken\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            400,
            ErrorCode::BadRequest,
        ),
        (
            "missing Content-Length",
            b"POST /v1/health HTTP/1.1\r\nHost: inbox\r\n\r\n".to_vec(),
            400,
            ErrorCode::BadRequest,
        ),
        (
            "chunked body",
            b"POST /v1/health HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n"
                .to_vec(),
            400,
            ErrorCode::BadRequest,
        ),
        (
            "Content-Length and Transfer-Encoding",
            b"POST /v1/health HTTP/1.1\r\nContent-Length: 2\r\nTransfer-Encoding: identity\r\n\r\n{}"
                .to_vec(),
            400,
            ErrorCode::BadRequest,
        ),
        ("headers over 8 KiB", long_header.into_bytes(), 413, ErrorCode::TooLarge),
        (
            "body over 512 KiB",
            b"POST /v1/health HTTP/1.1\r\nContent-Length: 524289\r\n\r\n".to_vec(),
            413,
            ErrorCode::TooLarge,
        ),
        (
            "unknown path",
            b"POST /v1/nope HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            404,
            ErrorCode::NotFound,
        ),
        (
            "path outside /v1",
            b"POST /health HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            404,
            ErrorCode::NotFound,
        ),
        (
            "GET method",
            b"GET /v1/health HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
            404,
            ErrorCode::NotFound,
        ),
    ];
    for (name, raw, status, code) in cases {
        let response = exchange(s, &raw);
        assert_eq!(response.status, status, "{name}");
        assert_eq!(c1::<WireError>(&response.body).code, code, "{name}");
    }
}

#[test]
fn a_client_writing_a_body_over_512_kib_reads_the_413_then_a_clean_end() {
    let scratch = Scratch::new();
    let daemon = start(&scratch, &scratch.root());
    let body = vec![b' '; 600 * 1024];
    let mut raw = format!(
        "POST /v1/health HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(&body);
    let mut stream = UnixStream::connect(&daemon.socket).expect("connect");
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("read timeout");
    stream.write_all(&raw).expect("the whole body is taken");
    let mut answer = Vec::new();
    stream
        .read_to_end(&mut answer)
        .expect("the answer ends without a reset");
    let response = parse(&answer);
    assert_eq!(response.status, 413);
    assert_eq!(c1::<WireError>(&response.body).code, ErrorCode::TooLarge);
}

#[test]
fn body_not_json_or_not_valid_for_the_input_is_400() {
    let scratch = Scratch::new();
    let (key, vectors) = golden_vectors();
    let daemon = start(&scratch, &scratch.root_with_key(&key));
    let s = daemon.socket.as_path();
    let packet = json!({ "packet": vector(&vectors, "int_source_ts") });
    let id = call::<PublishResponse>(s, "publish", &packet, 200).id;
    let bodies: [(&str, &str, &[u8]); 10] = [
        ("not JSON", "health", b"{"),
        ("JSON array", "claim", b"[60]"),
        ("health argument", "health", br#"{"x":1}"#),
        ("publish without packet", "publish", b"{}"),
        ("publish extra field", "publish", br#"{"packet":{},"x":1}"#),
        ("zero ttl", "claim", br#"{"ttl":0}"#),
        ("ttl past fixed notation", "claim", br#"{"ttl":1e16}"#),
        ("bad id", "get", br#"{"id":"no spaces"}"#),
        (
            "uppercase lease",
            "renew",
            br#"{"id":"a","lease":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}"#,
        ),
        (
            "unknown outcome",
            "ack",
            br#"{"id":"a","lease":"00000000000000000000000000000000","outcome":"done"}"#,
        ),
    ];
    let refused = |name: &str, op: &str, body: &[u8]| {
        let response = post_raw(s, op, body);
        assert_eq!(response.status, 400, "{name}");
        assert_eq!(
            c1::<WireError>(&response.body).code,
            ErrorCode::BadRequest,
            "{name}"
        );
    };
    for (name, op, body) in bodies {
        refused(name, op, body);
    }
    let pending = get(s, &id);
    assert_eq!(
        (pending.status, pending.lease.as_ref()),
        (Status::Pending, None)
    );
    let leased = claimed(s, &id);
    let lease = leased.lease.clone().expect("lease");
    for (name, op, field) in [
        ("renew ttl past fixed notation", "renew", "ttl"),
        ("retry delay past fixed notation", "retry", "delay"),
    ] {
        let body = json!({ "id": id, "lease": lease, field: 1e16 });
        refused(name, op, &serde_json::to_vec(&body).expect("json"));
    }
    assert_eq!(get(s, &id), leased, "refused bodies changed the row");
    let mut padded = b"{}".to_vec();
    padded.resize(512 * 1024, b' ');
    let response = post_raw(s, "health", &padded);
    assert_eq!(response.status, 200, "body of exactly 512 KiB");
    assert!(c1::<Health>(&response.body).ok);
}

/// `Server` over a fresh store with peers of `uid`, serving the far end of a socketpair.
fn serve_pair(scratch: &Scratch, uid: u32, timeout: Duration, sent: &[u8]) -> (Vec<u8>, Duration) {
    let inbox = Inbox::open(scratch.root()).expect("open");
    let (mut client, server) = UnixStream::pair().expect("socketpair");
    client.write_all(sent).expect("write");
    let started = Instant::now();
    Server::new(inbox, uid, timeout).serve(server);
    let elapsed = started.elapsed();
    (read_all(&mut client), elapsed)
}

#[test]
fn peer_of_another_uid_gets_403_before_its_request_is_read() {
    let scratch = Scratch::new();
    let other = own_uid().expect("uid").wrapping_add(1);
    // Nothing is sent: a server that read first would wait out the timeout instead.
    let (raw, elapsed) = serve_pair(&scratch, other, READ_TIMEOUT, b"");
    let response = parse(&raw);
    assert_eq!(response.status, 403);
    assert_eq!(c1::<WireError>(&response.body).code, ErrorCode::Forbidden);
    assert!(elapsed < READ_TIMEOUT, "{elapsed:?}");
}

#[test]
fn peer_of_the_same_uid_is_served() {
    let scratch = Scratch::new();
    let request = b"POST /v1/health HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}";
    let (raw, _) = serve_pair(&scratch, own_uid().expect("uid"), READ_TIMEOUT, request);
    let response = parse(&raw);
    assert_eq!(response.status, 200);
    assert!(c1::<Health>(&response.body).ok);
}

#[test]
fn connection_silent_for_the_read_timeout_is_closed_without_an_answer() {
    let uid = own_uid().expect("uid");
    let timeout = Duration::from_millis(50);
    for sent in [
        &b""[..],
        b"POST /v1/health HTTP/1.1\r\nContent-Length: 2\r\n\r\n{",
    ] {
        let scratch = Scratch::new();
        let (raw, elapsed) = serve_pair(&scratch, uid, timeout, sent);
        assert!(raw.is_empty(), "{}", String::from_utf8_lossy(&raw));
        assert!(
            elapsed >= timeout && elapsed < Duration::from_millis(400),
            "{elapsed:?}"
        );
    }
}

#[test]
fn daemon_is_one_thread_with_an_owner_only_socket() {
    let scratch = Scratch::new();
    let daemon = start(&scratch, &scratch.root());
    let meta = fs::symlink_metadata(&daemon.socket).expect("socket");
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(post_raw(&daemon.socket, "health", b"{}").status, 200);
    assert_eq!(status_field(daemon.child.id(), "Threads:"), "1");
}

#[test]
fn socket_path_is_limited_to_107_bytes() {
    let scratch = Scratch::new();
    let prefix = scratch.0.as_os_str().len() + 1;
    let longest = scratch.0.join("s".repeat(107 - prefix));
    assert_eq!(longest.as_os_str().len(), 107);
    match launch(INBOX, &[], &scratch.root(), &longest, None) {
        Started::Ready(daemon) => assert_eq!(post_raw(&daemon.socket, "health", b"{}").status, 200),
        Started::Exited(status, stderr) => panic!("107 bytes refused {status}: {stderr}"),
    }
    let too_long = scratch.0.join("t".repeat(108 - prefix));
    let stderr = refused(&scratch.root(), &too_long);
    assert!(stderr.contains("108 bytes, at most 107"), "{stderr}");
    assert!(!too_long.exists());
}

#[test]
fn stale_socket_is_replaced() {
    let scratch = Scratch::new();
    drop(UnixListener::bind(scratch.socket()).expect("bind"));
    // A child another test is spawning holds a copy of the listener until it execs.
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(scratch.socket()).is_ok() {
        assert!(Instant::now() < deadline, "nobody listens");
        std::thread::sleep(Duration::from_millis(1));
    }
    let daemon = start(&scratch, &scratch.root());
    assert_eq!(post_raw(&daemon.socket, "health", b"{}").status, 200);
}

#[test]
fn live_socket_or_other_file_at_the_socket_path_is_refused() {
    let scratch = Scratch::new();
    let live = UnixListener::bind(scratch.socket()).expect("bind");
    let stderr = refused(&scratch.root(), &scratch.socket());
    assert!(stderr.contains("not a stale socket"), "{stderr}");
    drop(live);

    let scratch = Scratch::new();
    fs::write(scratch.socket(), b"keep").expect("file");
    let stderr = refused(&scratch.root(), &scratch.socket());
    assert!(stderr.contains("not a stale socket"), "{stderr}");
    assert_eq!(fs::read(scratch.socket()).expect("file"), b"keep");
}

#[test]
fn root_that_is_not_owner_only_is_refused() {
    let scratch = Scratch::new();
    let root = scratch.root();
    fs::DirBuilder::new()
        .mode(0o755)
        .create(&root)
        .expect("root");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("chmod");
    let stderr = refused(&root, &scratch.socket());
    assert!(
        stderr.contains("Root must have no group or other mode bits"),
        "{stderr}"
    );
    assert!(!scratch.socket().exists());
}

#[test]
fn ulei_inbox_execs_the_sibling_daemon() {
    let scratch = Scratch::new();
    let daemon = match launch(ULEI, &["inbox"], &scratch.root(), &scratch.socket(), None) {
        Started::Ready(daemon) => daemon,
        Started::Exited(status, stderr) => panic!("forward exited {status}: {stderr}"),
    };
    assert_eq!(post_raw(&daemon.socket, "health", b"{}").status, 200);
    let comm = fs::read_to_string(format!("/proc/{}/comm", daemon.child.id())).expect("comm");
    assert_eq!(comm, "ulei-inbox\n");
}

#[test]
fn ulei_inbox_never_runs_ulei_inbox_from_path() {
    let scratch = Scratch::under(Path::new(env!("CARGO_TARGET_TMPDIR")));
    let bin = scratch.0.join("bin");
    let decoys = scratch.0.join("decoys");
    for dir in [&bin, &decoys] {
        fs::DirBuilder::new().mode(0o700).create(dir).expect("dir");
    }
    fs::copy(ULEI, bin.join("ulei")).expect("copy ulei");
    let marker = scratch.0.join("decoy-ran");
    let decoy = decoys.join("ulei-inbox");
    fs::write(&decoy, format!("#!/bin/sh\n: > '{}'\n", marker.display())).expect("decoy");
    fs::set_permissions(&decoy, fs::Permissions::from_mode(0o700)).expect("chmod");

    let out = Command::new(bin.join("ulei"))
        .args(["inbox", "serve", "--root", "r", "--socket", "s"])
        .env("PATH", &decoys)
        .output()
        .expect("run ulei");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    let expected = fs::canonicalize(&bin).expect("bin").join("ulei-inbox");
    assert!(stderr.contains(&expected.display().to_string()), "{stderr}");
    assert!(!marker.exists(), "decoy ran");
}
