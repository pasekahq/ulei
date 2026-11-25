//! `ulei-memory serve` and `ulei-memory delete` through the built binary and raw HTTP on the
//! daemon's socket, the dialogue pulled from a fake engine; the peer check and the deadline
//! through `ulei_memory::serve::Server` on a socketpair. All items are synthetic.

#![allow(missing_docs, clippy::unwrap_used, clippy::needless_pass_by_value)]

mod fake_engine;

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fake_engine::{FakeEngine, c4g, message, skipped};
use rusqlite::Connection;
use serde_json::{Value, json};
use ulei_memory::archive::{Archive, HIT_TEXT_CHARS};
use ulei_memory::pull::{Engine, KIND, SOURCE};
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

    /// The engine's C6 socket; nothing listens there unless a test starts a [`FakeEngine`].
    fn engine(&self) -> PathBuf {
        self.0.join("engine.sock")
    }

    /// `ulei.toml` naming the archive, the socket and the engine.
    fn config(&self) -> PathBuf {
        let path = self.0.join("ulei.toml");
        fs::write(
            &path,
            format!(
                "[memory]\narchive = \"{}\"\nsocket = \"{}\"\n\n[engine]\nc6_socket = \"{}\"\n",
                self.archive().display(),
                self.socket().display(),
                self.engine().display()
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

fn own() -> u32 {
    rustix::process::geteuid().as_raw()
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
    let _engine = FakeEngine::start(&scratch.engine(), 100, vec![]);
    let before = now();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().unwrap()), 0o700);

    let health: c10::Health = serde_json::from_slice(&ok(&socket, "health", &json!({}))).unwrap();
    assert!(health.version.starts_with("ulei-memory "), "{health:?}");
    assert_eq!(health.items, 0);
    assert!(
        health.last_sync_at.is_some_and(|at| at >= before),
        "{health:?}"
    );
    assert_eq!((health.sync_error, health.skipped), (None, None));

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
fn each_hit_carries_its_text_up_to_the_character_bound_and_says_when_it_is_cut() {
    let scratch = Scratch::new();
    let _engine = FakeEngine::start(&scratch.engine(), 100, vec![]);
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let bound = usize::try_from(HIT_TEXT_CHARS).unwrap();
    let short = "orchard ladder by the shed";
    let exact = format!("orchard {}", "ö".repeat(bound - 8));
    let long = format!("orchard {}", "ö".repeat(bound));
    let ids = [
        put(&socket, "short", short, "QQ==").id,
        put(&socket, "exact", &exact, "QUI=").id,
        put(&socket, "long", &long, "QUJD").id,
    ];

    let found = search(&socket, "orchard");
    let hit = |id: &str| found.hits.iter().find(|h| h.id == id).unwrap();
    let texts: Vec<_> = ids
        .iter()
        .map(|id| (hit(id).text.clone().unwrap(), hit(id).text_truncated))
        .collect();
    let head: String = long.chars().take(bound).collect();
    assert_eq!(
        texts,
        [
            (short.to_owned(), Some(false)),
            (exact, Some(false)),
            (head, Some(true)),
        ]
    );
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
fn serve_pair(path: &Path, uid: u32, timeout: Duration, sent: &[u8]) -> Vec<u8> {
    let archive = Archive::open(path, own()).unwrap();
    let (mut client, server) = UnixStream::pair().unwrap();
    client.write_all(sent).unwrap();
    let engine = Engine::new(path.with_file_name("engine.sock"), TIMEOUT);
    Server::new(archive, engine, uid, timeout).serve(server);
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
    drop(Archive::open(&wide_file.archive(), own()).unwrap());
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

    let deletions = Archive::open(&scratch.archive(), own())
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

    let mut archive = Archive::open(&scratch.archive(), own()).unwrap();
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

    let archive = Archive::open(&scratch.archive(), own()).unwrap();
    assert_eq!(archive.deletions().unwrap(), []);
    assert_eq!(archive.items().unwrap(), 1);
    assert_eq!(archive.read(&kept.id).unwrap().bytes, b"A");
}

/// The dialogue rows of a synthetic journal: two sessions, three turns.
fn journal() -> Vec<fake_engine::Row> {
    vec![
        message("owner", 1, 0, "user", "Plant the tulips?"),
        message("owner", 1, 1, "assistant", "In October."),
        message("side", 2, 0, "user", "Fence paint colour?"),
        message("side", 2, 1, "assistant", "Dark green."),
        message("owner", 1, 3, "user", "And the bulbs?"),
    ]
}

fn search(socket: &Path, q: &str) -> c10::SearchResponse {
    serde_json::from_slice(&ok(socket, "search", &json!({"q": q}))).unwrap()
}

fn sync(socket: &Path) -> c10::SyncResponse {
    serde_json::from_slice(&ok(socket, "sync", &json!({}))).unwrap()
}

fn health(socket: &Path) -> c10::Health {
    serde_json::from_slice(&ok(socket, "health", &json!({}))).unwrap()
}

#[test]
fn the_start_archives_every_served_message_with_its_bytes_source_and_external_id() {
    let scratch = Scratch::new();
    let _engine = FakeEngine::start(&scratch.engine(), 2, journal());
    let _daemon = start(&scratch);
    let socket = scratch.socket();

    assert_eq!(health(&socket).items, 5);
    let cases = [
        (
            "tulips",
            "owner/1/0",
            "user",
            "Plant the tulips?",
            1_700_000_100.0,
        ),
        (
            "October",
            "owner/1/1",
            "assistant",
            "In October.",
            1_700_000_101.0,
        ),
        (
            "Fence",
            "side/2/0",
            "user",
            "Fence paint colour?",
            1_700_000_200.0,
        ),
        (
            "green",
            "side/2/1",
            "assistant",
            "Dark green.",
            1_700_000_201.0,
        ),
        (
            "bulbs",
            "owner/1/3",
            "user",
            "And the bulbs?",
            1_700_000_103.0,
        ),
    ];
    for (q, external_id, who, text, at) in cases {
        let found = search(&socket, q);
        assert!(found.synced);
        assert_eq!(found.hits.len(), 1, "{q}");
        let entry = &found.hits[0].entries[0];
        assert_eq!(
            (entry.source.as_str(), entry.external_id.as_str()),
            (SOURCE, external_id)
        );
        assert_eq!((entry.who.as_deref(), entry.at), (Some(who), Some(at)));
        let original = Archive::open(&scratch.archive(), own())
            .unwrap()
            .read(&found.hits[0].id)
            .unwrap();
        assert_eq!(original.kind, KIND);
        assert_eq!(original.bytes, c4g(who, text).into_bytes());
    }
}

#[test]
fn a_repeated_sync_adds_nothing_and_a_later_message_comes_with_the_next() {
    let scratch = Scratch::new();
    let engine = FakeEngine::start(&scratch.engine(), 2, journal());
    let _daemon = start(&scratch);
    let socket = scratch.socket();

    let again = sync(&socket);
    assert_eq!((again.put, again.synced), (0, true));
    assert_eq!(health(&socket).items, 5);

    engine.append(vec![message(
        "owner",
        1,
        4,
        "assistant",
        "Water them weekly.",
    )]);
    let next = sync(&socket);
    assert_eq!((next.put, next.synced), (1, true));
    assert_eq!(health(&socket).items, 6);
    assert_eq!(search(&socket, "weekly").hits.len(), 1);
}

#[test]
fn a_pull_cut_between_pages_resumes_at_the_stored_cursor_without_duplicates() {
    let scratch = Scratch::new();
    let engine = FakeEngine::start(&scratch.engine(), 2, journal());
    engine.fail(2);
    let _daemon = start(&scratch);
    let socket = scratch.socket();

    let cut = health(&socket);
    assert_eq!(cut.items, 2, "the first page is stored");
    assert!(cut.sync_error.unwrap().contains("synthetic failure"));
    assert_eq!(cut.last_sync_at, None);

    let resumed = sync(&socket);
    assert_eq!((resumed.put, resumed.synced), (3, true));
    let after = health(&socket);
    assert_eq!((after.items, after.sync_error), (5, None));
    assert_eq!(
        engine.cursors(),
        [None, Some("2"), Some("2"), Some("4")].map(|c| c.map(str::to_owned)),
        "the retry starts after the stored page; the last page has no cursor of its own"
    );
}

#[test]
fn with_the_engine_down_search_answers_from_the_store_unsynced_and_health_names_the_error() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();

    let down = health(&socket);
    assert_eq!(down.last_sync_at, None);
    let error = down.sync_error.unwrap();
    assert!(error.contains("engine.c6_socket"), "{error}");
    put(&socket, "note-1", "orchard notes", "QQ==");
    let found = search(&socket, "orchard");
    assert_eq!((found.hits.len(), found.synced), (1, false));
    let failed = sync(&socket);
    assert_eq!((failed.put, failed.synced), (0, false));

    let _engine = FakeEngine::start(&scratch.engine(), 100, journal());
    let back = search(&socket, "orchard");
    assert_eq!((back.hits.len(), back.synced), (1, true));
    let health = health(&socket);
    assert_eq!((health.items, health.sync_error), (6, None));
    assert!(health.last_sync_at.is_some());
}

#[test]
fn a_message_the_engine_skips_keeps_its_reference_and_the_pull_goes_on() {
    let scratch = Scratch::new();
    let mut rows = journal();
    rows.insert(2, skipped("owner", 1, 2, 900_000));
    rows.push(skipped("side", 2, 2, 700_000));
    rows.push(message("side", 2, 3, "user", "Which brush?"));
    let engine = FakeEngine::start(&scratch.engine(), 2, rows);
    let daemon = start(&scratch);
    let socket = scratch.socket();

    let first = health(&socket);
    assert_eq!(first.items, 6);
    let refs = |h: &c10::Health| -> Vec<(String, String, u64, Option<f64>)> {
        h.skipped
            .iter()
            .flatten()
            .map(|s| (s.source.clone(), s.external_id.clone(), s.len, s.at))
            .collect()
    };
    let expected = vec![
        (
            SOURCE.to_owned(),
            "owner/1/2".to_owned(),
            900_000,
            Some(1_700_000_500.0),
        ),
        (
            SOURCE.to_owned(),
            "side/2/2".to_owned(),
            700_000,
            Some(1_700_000_500.0),
        ),
    ];
    assert_eq!(refs(&first), expected);
    assert_eq!(search(&socket, "brush").hits.len(), 1);

    engine.append(vec![skipped("side", 2, 4, 800_000)]);
    assert_eq!(sync(&socket).put, 0);
    drop(daemon);
    let _daemon = start(&scratch);
    let mut every = expected;
    every.push((
        SOURCE.to_owned(),
        "side/2/4".to_owned(),
        800_000,
        Some(1_700_000_500.0),
    ));
    assert_eq!(
        refs(&health(&socket)),
        every,
        "kept across a restart, none twice"
    );
}

#[test]
fn a_refused_config_exits_2_with_one_line_so_the_unit_is_not_restarted() {
    let scratch = Scratch::new();
    let config = scratch.0.join("ulei.toml");
    fs::write(&config, "[memory]\nsocket = \"/run/example/memory.sock\"\n").unwrap();
    match launch(&config) {
        Started::Exited(code, stderr) => {
            assert_eq!(code, Some(2), "{stderr}");
            assert_eq!(stderr.lines().count(), 1, "{stderr}");
            assert!(stderr.contains("memory.archive is missing"), "{stderr}");
        }
        Started::Ready(_) => panic!("the daemon started on a refused config"),
    }
}

fn b64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ABC[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unb64(text: &str) -> Vec<u8> {
    let sextet = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let mut out = Vec::new();
    for group in text.as_bytes().chunks(4) {
        let pad = usize::from(group[3] == b'=') + usize::from(group[2] == b'=');
        let n = group[..4 - pad]
            .iter()
            .fold(0u32, |n, &c| n << 6 | u32::from(sextet(c)))
            << (6 * pad);
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    out
}

fn sha(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// `len` bytes deflate cannot shorten.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[0]
        })
        .collect()
}

fn begin(socket: &Path, external_id: &str, original: &[u8], id: &str) -> (u16, Vec<u8>) {
    post(
        socket,
        "put_begin",
        &json!({
            "source": "library",
            "external_id": external_id,
            "kind": "application/octet-stream",
            "text": "lantern manual",
            "len": original.len(),
            "id": id,
        }),
    )
}

/// The original of `id` read back by ranges of at most 512 KiB.
fn read_back(socket: &Path, id: &str) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let range: c10::ReadResponse = serde_json::from_slice(&ok(
            socket,
            "read",
            &json!({"id": id, "offset": out.len(), "length": 512 * 1024}),
        ))
        .unwrap();
        assert_eq!(range.offset, Some(out.len() as u64));
        let bytes = unb64(&range.bytes_b64);
        if bytes.is_empty() {
            assert_eq!(out.len() as u64, range.len);
            return out;
        }
        out.extend_from_slice(&bytes);
    }
}

#[test]
fn an_original_over_the_body_cap_goes_in_parts_and_reads_back_by_ranges() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let part = 512 * 1024;
    let original = noise(3 * part + 1000, 7);
    let id = sha(&original);

    // An upload abandoned after its first part does not hold up the next one.
    let (status, body) = begin(&socket, "lib/lantern.bin", &original, &id);
    assert_eq!(status, 200);
    let abandoned = serde_json::from_slice::<c10::PutBeginResponse>(&body)
        .unwrap()
        .upload
        .unwrap();
    ok(
        &socket,
        "put_part",
        &json!({"upload": abandoned, "index": 0, "bytes_b64": b64(&original[..part])}),
    );
    let (status, body) = begin(&socket, "lib/lantern.bin", &original, &id);
    assert_eq!(status, 200);
    let begun: c10::PutBeginResponse = serde_json::from_slice(&body).unwrap();
    let upload = begun.upload.unwrap();
    assert_eq!((begun.id.as_str(), begun.status), (id.as_str(), None));
    for (index, chunk) in original.chunks(part).enumerate() {
        let got: c10::PutPartResponse = serde_json::from_slice(&ok(
            &socket,
            "put_part",
            &json!({"upload": upload, "index": index, "bytes_b64": b64(chunk)}),
        ))
        .unwrap();
        assert_eq!(got.parts, index as u64 + 1);
    }
    let health: c10::Health = serde_json::from_slice(&ok(&socket, "health", &json!({}))).unwrap();
    assert_eq!(health.items, 0, "nothing is stored before put_end");
    let ended: c10::PutResponse =
        serde_json::from_slice(&ok(&socket, "put_end", &json!({"upload": upload}))).unwrap();
    assert_eq!(
        (ended.id.as_str(), ended.status),
        (id.as_str(), c10::PutStatus::Stored)
    );

    assert_eq!(
        refused(post(&socket, "read", &json!({"id": id}))),
        (413, c10::ErrorCode::TooLarge)
    );
    assert_eq!(sha(&read_back(&socket, &id)), id);
    let found = search(&socket, "lantern");
    assert_eq!(found.hits[0].id, id);

    // Again: settled at put_begin without a part.
    let again: c10::PutBeginResponse = serde_json::from_slice(&{
        let (status, body) = begin(&socket, "lib/lantern.bin", &original, &id);
        assert_eq!(status, 200);
        body
    })
    .unwrap();
    assert_eq!(
        (again.upload, again.status),
        (None, Some(c10::PutStatus::Duplicate))
    );

    // A small item reads whole as before and by range.
    let small = put(&socket, "note-1", "orchard", EVERY_BYTE_B64);
    let range: c10::ReadResponse = serde_json::from_slice(&ok(
        &socket,
        "read",
        &json!({"id": small.id, "offset": 255}),
    ))
    .unwrap();
    assert_eq!((range.bytes_b64.as_str(), range.len), ("/w==", 256));
}

#[test]
fn parts_of_another_sha256_or_missing_bytes_are_refused_and_store_nothing() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let original = noise(700 * 1024, 11);
    let id = sha(&original);
    let mut wrong = original.clone();
    wrong[600 * 1024] ^= 0x80;
    let (status, body) = begin(&socket, "lib/wrong.bin", &original, &id);
    assert_eq!(status, 200);
    let upload = serde_json::from_slice::<c10::PutBeginResponse>(&body)
        .unwrap()
        .upload
        .unwrap();
    for (index, chunk) in wrong.chunks(512 * 1024).enumerate() {
        ok(
            &socket,
            "put_part",
            &json!({"upload": upload, "index": index, "bytes_b64": b64(chunk)}),
        );
    }
    assert_eq!(
        refused(post(&socket, "put_end", &json!({"upload": upload}))),
        (400, c10::ErrorCode::InvalidRequest)
    );
    assert_eq!(
        refused(post(&socket, "put_end", &json!({"upload": upload}))),
        (404, c10::ErrorCode::NotFound)
    );

    let (_, body) = begin(&socket, "lib/wrong.bin", &original, &id);
    let upload = serde_json::from_slice::<c10::PutBeginResponse>(&body)
        .unwrap()
        .upload
        .unwrap();
    assert_eq!(
        refused(post(
            &socket,
            "put_part",
            &json!({"upload": upload, "index": 0, "bytes_b64": "!!!!"})
        )),
        (400, c10::ErrorCode::InvalidRequest)
    );
    assert_eq!(
        refused(post(
            &socket,
            "put_part",
            &json!({"upload": upload, "index": 1, "bytes_b64": "QQ=="})
        )),
        (400, c10::ErrorCode::InvalidRequest)
    );
    ok(
        &socket,
        "put_part",
        &json!({"upload": upload, "index": 0, "bytes_b64": b64(&original[..1024])}),
    );
    assert_eq!(
        refused(post(&socket, "put_end", &json!({"upload": upload}))),
        (400, c10::ErrorCode::InvalidRequest)
    );
    assert_eq!(
        refused(post(
            &socket,
            "put_part",
            &json!({"upload": 999, "index": 0, "bytes_b64": "QQ=="})
        )),
        (404, c10::ErrorCode::NotFound)
    );
    assert_eq!(health(&socket).items, 0);
    let db = Connection::open(scratch.archive()).unwrap();
    for table in ["blobs", "parts", "chunked"] {
        let n: i64 = db
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }
}
