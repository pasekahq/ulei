//! `ulei-memory serve` and `ulei-memory delete` through the built binary and raw HTTP on the
//! daemon's socket; the peer check and the deadline through `ulei_memory::serve::Server` on a
//! socketpair. All items are synthetic.

#![allow(missing_docs, clippy::unwrap_used, clippy::needless_pass_by_value)]

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::{Value, json};
use ulei_memory::archive::Archive;
use ulei_memory::serve::{MAX_BODY, Server, TIMEOUT};
use ulei_memory::wire::c10;

const MEMORY: &str = env!("CARGO_BIN_EXE_ulei-memory");

/// Standard base64 of the bytes 0 to 255.
const EVERY_BYTE_B64: &str = concat!(
    "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+P0BBQkNE",
    "RUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6e3x9fn+AgYKDhIWGh4iJ",
    "iouMjY6PkJGSk5SVlpeYmZqbnJ2en6ChoqOkpaanqKmqq6ytrq+wsbKztLW2t7i5uru8vb6/wMHCw8TFxsfIycrLzM3O",
    "z9DR0tPU1dbX2Nna29zd3t/g4eLj5OXm5+jp6uvs7e7v8PHy8/T19vf4+fr7/P3+/w==",
);

/// Lowercase hex SHA-256 of the bytes 0 to 255.
const EVERY_BYTE_ID: &str = "40aff2e9d2d8922e47afd4648e6967497158785fbd1da870e7110266bf944880";

/// A fresh 0700 directory short enough for a socket path, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let temp = std::env::temp_dir();
        let base = if temp.as_os_str().len() <= 48 {
            temp
        } else {
            PathBuf::from("/tmp")
        };
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("ulei-memory-it-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self(dir)
    }

    fn archive(&self) -> PathBuf {
        self.0.join("data/memory.archive")
    }

    fn socket(&self) -> PathBuf {
        self.0.join("run/memory.sock")
    }

    /// `ulei.toml` naming the archive and the socket.
    fn config(&self) -> PathBuf {
        let path = self.0.join("ulei.toml");
        fs::write(
            &path,
            format!(
                "[memory]\narchive = \"{}\"\nsocket = \"{}\"\n",
                self.archive().display(),
                self.socket().display()
            ),
        )
        .unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A running daemon, killed on drop.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

enum Started {
    Ready(Daemon),
    Exited(Option<i32>, String),
}

/// `ulei-memory serve --config <config>` until its ready line or its exit.
fn launch(config: &Path) -> Started {
    let mut child = Command::new(MEMORY)
        .args(["serve", "--config"])
        .arg(config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    if line.is_empty() {
        let status = child.wait().unwrap();
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        return Started::Exited(status.code(), stderr);
    }
    assert!(line.starts_with("ulei-memory ready /"), "{line}");
    Started::Ready(Daemon(child))
}

fn start(scratch: &Scratch) -> Daemon {
    match launch(&scratch.config()) {
        Started::Ready(daemon) => daemon,
        Started::Exited(code, stderr) => panic!("daemon exited {code:?}: {stderr}"),
    }
}

/// Reads to the end of the connection; a reset after the answer also ends it.
fn read_all(stream: &mut UnixStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return out,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::ConnectionReset && !out.is_empty() => return out,
            Err(e) => panic!("read: {e}"),
        }
    }
}

/// The status and body of one answer.
fn parse(raw: &[u8]) -> (u16, Vec<u8>) {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = std::str::from_utf8(&raw[..end]).unwrap();
    assert!(head.contains("\r\nConnection: close"), "{head}");
    let status = head["HTTP/1.1 ".len().."HTTP/1.1 ".len() + 3]
        .parse()
        .unwrap();
    (status, raw[end + 4..].to_vec())
}

fn request(target: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "POST {target} HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// Sends `raw` on a new connection to `socket` and returns the answer.
fn exchange(socket: &Path, raw: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.write_all(raw).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    parse(&read_all(&mut stream))
}

fn post(socket: &Path, method: &str, body: &Value) -> (u16, Vec<u8>) {
    exchange(
        socket,
        &request(&format!("/v1/{method}"), body.to_string().as_bytes()),
    )
}

/// The `200` output of `method`.
fn ok(socket: &Path, method: &str, body: &Value) -> Vec<u8> {
    let (status, out) = post(socket, method, body);
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&out));
    out
}

/// The status and C10 `Error` code of a refused answer.
fn refused(answer: (u16, Vec<u8>)) -> (u16, c10::ErrorCode) {
    let error: c10::Error = serde_json::from_slice(&answer.1).unwrap();
    (answer.0, error.code)
}

fn put_body(external_id: &str, text: &str, bytes_b64: &str) -> Value {
    json!({
        "source": "notes",
        "external_id": external_id,
        "who": "owner",
        "at": 1_700_000_000.0,
        "kind": "text/plain",
        "text": text,
        "bytes_b64": bytes_b64,
    })
}

fn put(socket: &Path, external_id: &str, text: &str, bytes_b64: &str) -> c10::PutResponse {
    serde_json::from_slice(&ok(socket, "put", &put_body(external_id, text, bytes_b64))).unwrap()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[test]
fn put_search_read_sync_and_health_answer_per_c10_over_the_socket() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().unwrap()), 0o700);

    let health: c10::Health = serde_json::from_slice(&ok(&socket, "health", &json!({}))).unwrap();
    assert!(health.version.starts_with("ulei-memory "), "{health:?}");
    assert_eq!((health.items, health.last_sync_at), (0, None));
    assert_eq!(health.sync_error, None);

    let stored = put(&socket, "note-1", "orchard notes", EVERY_BYTE_B64);
    assert_eq!(
        (stored.id.as_str(), stored.status),
        (EVERY_BYTE_ID, c10::PutStatus::Stored)
    );
    let again = put(&socket, "note-1", "orchard notes", EVERY_BYTE_B64);
    assert_eq!(again.status, c10::PutStatus::Duplicate);
    for (n, b64) in ["", "QQ==", "QUI="].into_iter().enumerate() {
        let short = put(&socket, &format!("short-{n}"), "short", b64);
        assert_eq!(short.status, c10::PutStatus::Stored);
        let read: c10::ReadResponse =
            serde_json::from_slice(&ok(&socket, "read", &json!({"id": short.id}))).unwrap();
        assert_eq!((read.bytes_b64.as_str(), read.len), (b64, n as u64));
    }

    let found: c10::SearchResponse = serde_json::from_slice(&ok(
        &socket,
        "search",
        &json!({"q": "orchard", "who": "owner"}),
    ))
    .unwrap();
    assert!(found.synced);
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].id, EVERY_BYTE_ID);
    assert_eq!(found.hits[0].entries[0].external_id, "note-1");
    assert!(found.hits[0].snippet.contains(">>>orchard<<<"), "{found:?}");
    let missed: c10::SearchResponse = serde_json::from_slice(&ok(
        &socket,
        "search",
        &json!({"q": "orchard", "source": "mail"}),
    ))
    .unwrap();
    assert_eq!(missed.hits, []);

    let read: c10::ReadResponse =
        serde_json::from_slice(&ok(&socket, "read", &json!({"id": EVERY_BYTE_ID}))).unwrap();
    assert_eq!(read.bytes_b64, EVERY_BYTE_B64);
    assert_eq!((read.kind.as_str(), read.len), ("text/plain", 256));
    assert_eq!(read.entries.len(), 1);
    assert_eq!(read.entries[0].at, Some(1_700_000_000.0));

    let synced: c10::SyncResponse =
        serde_json::from_slice(&ok(&socket, "sync", &json!({}))).unwrap();
    assert_eq!((synced.put, synced.synced), (0, true));
    let health: c10::Health = serde_json::from_slice(&ok(&socket, "health", &json!({}))).unwrap();
    assert_eq!(health.items, 4);
}

#[test]
fn each_refusal_carries_its_c10_code_and_status() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();

    let invalid = (400, c10::ErrorCode::InvalidRequest);
    assert_eq!(refused(post(&socket, "search", &json!({"q": 5}))), invalid);
    assert_eq!(refused(post(&socket, "search", &json!({"q": ""}))), invalid);
    assert_eq!(
        refused(post(&socket, "search", &json!({"q": "x", "limit": 51}))),
        invalid
    );
    assert_eq!(
        refused(post(&socket, "sync", &json!({"now": true}))),
        invalid
    );
    assert_eq!(
        refused(post(&socket, "health", &json!({"verbose": true}))),
        invalid
    );
    assert_eq!(
        refused(post(&socket, "read", &json!({"id": "abc"}))),
        invalid
    );
    let mut bad_source = put_body("n", "t", "QQ==");
    bad_source["source"] = json!("Notes");
    assert_eq!(refused(post(&socket, "put", &bad_source)), invalid);
    for b64 in ["QQ=", "Q=Q=", "QQ==QQ==", "QQ-_", "===="] {
        assert_eq!(
            refused(post(&socket, "put", &put_body("n", "t", b64))),
            invalid,
            "{b64}"
        );
    }
    assert_eq!(
        refused(exchange(&socket, &request("/v1/put", b"{"))),
        invalid
    );
    assert_eq!(
        refused(exchange(&socket, b"POST /v1/health HTTP/1.1\r\n\r\n")),
        invalid
    );

    let unknown = (404, c10::ErrorCode::UnknownMethod);
    assert_eq!(refused(post(&socket, "forget", &json!({}))), unknown);
    assert_eq!(
        refused(exchange(&socket, &request("/health", b"{}"))),
        unknown
    );
    let get = b"GET /v1/health HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}";
    assert_eq!(refused(exchange(&socket, get)), unknown);

    let absent = "0".repeat(64);
    assert_eq!(
        refused(post(&socket, "read", &json!({"id": absent}))),
        (404, c10::ErrorCode::NotFound)
    );

    put(&socket, "note-1", "orchard", "QQ==");
    assert_eq!(
        refused(post(&socket, "put", &put_body("note-1", "orchard", "QUI="))),
        (409, c10::ErrorCode::Conflict)
    );

    let mut padded = b"{}".to_vec();
    padded.resize(MAX_BODY, b' ');
    let (status, _) = exchange(&socket, &request("/v1/health", &padded));
    assert_eq!(status, 200, "a body of exactly 1 MiB");
    padded.push(b' ');
    assert_eq!(
        refused(exchange(&socket, &request("/v1/health", &padded))),
        (413, c10::ErrorCode::TooLarge)
    );

    let id = put(&socket, "note-2", "meadow", EVERY_BYTE_B64).id;
    let db = Connection::open(scratch.archive()).unwrap();
    db.execute_batch("DROP TRIGGER blobs_immutable").unwrap();
    db.execute("UPDATE blobs SET body = zeroblob(256) WHERE id = ?1", [&id])
        .unwrap();
    assert_eq!(
        refused(post(&socket, "read", &json!({"id": id}))),
        (500, c10::ErrorCode::StoreFailed)
    );
}

#[test]
fn delete_is_not_served_on_the_socket() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let id = put(&socket, "note-1", "orchard", "QQ==").id;
    assert_eq!(
        refused(post(
            &socket,
            "delete",
            &json!({"id": id, "reason": "test"})
        )),
        (404, c10::ErrorCode::UnknownMethod)
    );
    let read: c10::ReadResponse =
        serde_json::from_slice(&ok(&socket, "read", &json!({"id": id}))).unwrap();
    assert_eq!(read.bytes_b64, "QQ==");
}

#[test]
fn an_external_id_over_512_utf8_bytes_answers_invalid_request() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    // 256 two-byte letters are 512 bytes; one more is 514 bytes in 257 characters.
    let longest = "ж".repeat(256);
    assert_eq!(
        put(&socket, &longest, "t", "QQ==").status,
        c10::PutStatus::Stored
    );
    let over = "ж".repeat(257);
    assert_eq!(
        refused(post(&socket, "put", &put_body(&over, "t", "QQ=="))),
        (400, c10::ErrorCode::InvalidRequest)
    );
}

/// What `Server::serve` answers for uid `uid` on a socketpair whose client sent `sent`.
fn serve_pair(archive: &Path, uid: u32, timeout: Duration, sent: &[u8]) -> Vec<u8> {
    let archive = Archive::open(archive).unwrap();
    let (mut client, server) = UnixStream::pair().unwrap();
    client.write_all(sent).unwrap();
    Server::new(archive, uid, timeout).serve(server);
    read_all(&mut client)
}

#[test]
fn peer_of_another_uid_is_forbidden() {
    let scratch = Scratch::new();
    let other = rustix::process::geteuid().as_raw().wrapping_add(1);
    let sent = request("/v1/health", b"{}");
    assert_eq!(
        refused(parse(&serve_pair(
            &scratch.archive(),
            other,
            TIMEOUT,
            &sent
        ))),
        (403, c10::ErrorCode::Forbidden)
    );
}

#[test]
fn a_stalled_client_is_cut_at_the_deadline_without_an_answer() {
    let scratch = Scratch::new();
    let own = rustix::process::geteuid().as_raw();
    let deadline = Duration::from_millis(300);
    let started = Instant::now();
    let answer = serve_pair(
        &scratch.archive(),
        own,
        deadline,
        b"POST /v1/health HTTP/1.1\r\nContent-Length: 2\r\n",
    );
    let waited = started.elapsed();
    assert!(answer.is_empty(), "{}", String::from_utf8_lossy(&answer));
    assert!(waited >= deadline, "{waited:?}");
    assert!(waited < deadline * 10, "{waited:?}");
}

#[test]
fn a_store_directory_or_file_open_to_other_users_refuses_the_start() {
    let wide_dir = Scratch::new();
    fs::DirBuilder::new()
        .mode(0o755)
        .create(wide_dir.archive().parent().unwrap())
        .unwrap();
    fs::set_permissions(
        wide_dir.archive().parent().unwrap(),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let wide_file = Scratch::new();
    drop(Archive::open(&wide_file.archive()).unwrap());
    fs::set_permissions(wide_file.archive(), fs::Permissions::from_mode(0o644)).unwrap();

    for (scratch, shown) in [(&wide_dir, "mode 0755"), (&wide_file, "mode 0644")] {
        let Started::Exited(code, stderr) = launch(&scratch.config()) else {
            panic!("daemon started with {shown}");
        };
        assert_eq!(code, Some(1), "{stderr}");
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        assert!(stderr.contains(shown), "{stderr}");
        assert!(!scratch.socket().exists());
    }
    assert!(!wide_dir.archive().exists());
}

fn delete(scratch: &Scratch, id: &str, reason: &str) -> (Output, u32) {
    let child = Command::new(MEMORY)
        .args(["delete", id, "--reason", reason, "--config"])
        .arg(scratch.config())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    (child.wait_with_output().unwrap(), pid)
}

#[test]
fn the_delete_command_removes_the_item_and_leaves_its_audit_row_and_one_stderr_line() {
    let scratch = Scratch::new();
    let daemon = start(&scratch);
    let socket = scratch.socket();
    let id = put(&socket, "note-1", "orchard", EVERY_BYTE_B64).id;
    put(&socket, "note-2", "meadow", "QQ==");

    let before = now();
    let (out, pid) = delete(&scratch, &id, "synthetic cleanup");
    let after = now();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{stderr}");
    assert_eq!(out.stdout, b"");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains(&id), "{stderr}");

    assert_eq!(
        refused(post(&socket, "read", &json!({"id": id}))),
        (404, c10::ErrorCode::NotFound)
    );
    let found: c10::SearchResponse =
        serde_json::from_slice(&ok(&socket, "search", &json!({"q": "orchard"}))).unwrap();
    assert_eq!(found.hits, []);
    let health: c10::Health = serde_json::from_slice(&ok(&socket, "health", &json!({}))).unwrap();
    assert_eq!(health.items, 1);
    drop(daemon);

    let deletions = Archive::open(&scratch.archive())
        .unwrap()
        .deletions()
        .unwrap();
    assert_eq!(deletions.len(), 1);
    let row = &deletions[0];
    assert_eq!((row.id.as_str(), row.len), (id.as_str(), 256));
    assert_eq!(row.reason, "synthetic cleanup");
    assert_eq!(row.pid, u64::from(pid));
    assert_eq!(row.uid, u64::from(rustix::process::getuid().as_raw()));
    assert!((before..=after).contains(&row.deleted_at), "{row:?}");
    assert_eq!(row.entries.len(), 1);
    assert_eq!(row.entries[0].external_id, "note-1");
}

#[test]
fn deleting_an_unknown_id_fails_with_one_line_and_writes_nothing() {
    let scratch = Scratch::new();
    let absent = "0".repeat(64);

    let (out, _) = delete(&scratch, &absent, "synthetic cleanup");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains("memory.archive"), "{stderr}");
    assert!(
        !scratch.archive().exists(),
        "a missing archive is not created"
    );

    let mut archive = Archive::open(&scratch.archive()).unwrap();
    let kept = archive
        .put(&ulei_memory::archive::Item {
            source: "notes",
            external_id: "note-1",
            who: None,
            at: None,
            kind: "text/plain",
            text: "orchard",
            bytes: b"A",
        })
        .unwrap();
    drop(archive);
    let (out, _) = delete(&scratch, &absent, "synthetic cleanup");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains(&format!("no item {absent}")), "{stderr}");

    let archive = Archive::open(&scratch.archive()).unwrap();
    assert_eq!(archive.deletions().unwrap(), []);
    assert_eq!(archive.items().unwrap(), 1);
    assert_eq!(archive.read(&kept.id).unwrap().bytes, b"A");
}
