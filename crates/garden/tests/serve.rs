//! `ulei-garden serve` through the built binary and raw HTTP on its socket; the peer check and
//! a store failure through `ulei_garden::serve::Server` on a socketpair. All people, places and
//! texts are fictional.

#![allow(missing_docs, clippy::unwrap_used, clippy::needless_pass_by_value)]

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use ulei_garden::serve::{MAX_BODY, Server, TIMEOUT, open};
use ulei_garden::wire::c7;

const GARDEN: &str = env!("CARGO_BIN_EXE_ulei-garden");

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
        let dir = base.join(format!("ulei-garden-it-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self(dir)
    }

    fn store(&self) -> PathBuf {
        self.0.join("garden.sqlite3")
    }

    fn socket(&self) -> PathBuf {
        self.0.join("run/garden.sock")
    }

    /// `ulei.toml` holding `text`.
    fn config(&self, text: &str) -> PathBuf {
        let path = self.0.join("ulei.toml");
        fs::write(&path, text).unwrap();
        path
    }

    /// `ulei.toml` naming the store and the socket.
    fn full_config(&self) -> PathBuf {
        self.config(&format!(
            "[garden]\nstore = \"{}\"\nsocket = \"{}\"\n",
            self.store().display(),
            self.socket().display()
        ))
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

/// `ulei-garden serve --config <config>` until its ready line or its exit.
fn launch(config: &Path) -> Started {
    let mut child = Command::new(GARDEN)
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
    assert!(line.starts_with("ulei-garden ready /"), "{line}");
    Started::Ready(Daemon(child))
}

fn start(scratch: &Scratch) -> Daemon {
    match launch(&scratch.full_config()) {
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

/// The status and C7 `Error` of a refused answer.
fn refused(answer: (u16, Vec<u8>)) -> (u16, c7::ErrorCode) {
    let error: c7::Error = serde_json::from_slice(&answer.1).unwrap();
    (answer.0, error.code)
}

fn owner_fact(entity: &str, fact: &str) -> Value {
    json!({
        "fact": fact,
        "entity": entity,
        "source_ref": "msg:7",
        "kind": "person",
        "domain": "doctors",
        "valid_from": 1_700_000_000,
        "source_kind": "owner",
    })
}

/// An internet candidate that conflicts with an owner address and so waits in the digest.
fn weak_address() -> Value {
    json!({
        "fact": "Ada lives at 7 Sample Lane.",
        "entity": "Ada Example",
        "source_ref": "https://example.invalid/directory",
        "attribute": "address",
        "valid_from": 1_700_000_500,
        "source_kind": "internet",
        "ttl_kind": "place",
    })
}

fn queue_weak_address(socket: &Path) -> String {
    let mut first = owner_fact("Ada Example", "Ada lives at 3 Old Road.");
    first["attribute"] = json!("address");
    ok(socket, "propose", &first);
    let queued: c7::ProposeResponse =
        serde_json::from_slice(&ok(socket, "propose", &weak_address())).unwrap();
    assert_eq!(queued.status, c7::ProposalStatus::Queued);
    queued.proposal_id
}

#[test]
fn every_method_round_trips_over_the_socket_and_the_store_outlives_a_restart() {
    let scratch = Scratch::new();
    let daemon = start(&scratch);
    let socket = scratch.socket();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().unwrap()), 0o700);

    // `doctors` is a starting domain: the propose is accepted only when the seed ran.
    let proposed: c7::ProposeResponse = serde_json::from_slice(&ok(
        &socket,
        "propose",
        &owner_fact("Bram Sample", "Bram Sample sees the dentist on Fridays."),
    ))
    .unwrap();
    assert_eq!(proposed.status, c7::ProposalStatus::Accepted);
    let fact_id = proposed.fact_id.unwrap();

    let found: c7::SearchResponse =
        serde_json::from_slice(&ok(&socket, "search", &json!({"q": "dentist"}))).unwrap();
    assert_eq!(found.facts.len(), 1);
    assert_eq!(found.facts[0].fact_id.as_deref(), Some(fact_id.as_str()));
    assert_eq!(
        found.facts[0].fact,
        "Bram Sample sees the dentist on Fridays."
    );

    let pointed: c7::PointersResponse =
        serde_json::from_slice(&ok(&socket, "pointers", &json!({"q": "dentist"}))).unwrap();
    assert_eq!(pointed.pointers.len(), 1);
    assert_eq!(pointed.pointers[0].fact_id, fact_id);
    assert_eq!(pointed.pointers[0].entity, "Bram Sample");

    let proposal_id = queue_weak_address(&socket);
    let digest: c7::DigestResponse =
        serde_json::from_slice(&ok(&socket, "digest", &json!({}))).unwrap();
    assert_eq!(digest.items.len(), 1);
    assert_eq!(digest.items[0].proposal_id, proposal_id);
    let resolved: c7::ProposeResponse = serde_json::from_slice(&ok(
        &socket,
        "resolve",
        &json!({"proposal_id": proposal_id, "action": "accept"}),
    ))
    .unwrap();
    assert_eq!(resolved.status, c7::ProposalStatus::Accepted);

    let health: c7::Health = serde_json::from_slice(&ok(&socket, "health", &json!({}))).unwrap();
    assert_eq!(
        health,
        c7::Health {
            version: format!("ulei-garden {}", env!("CARGO_PKG_VERSION")),
            dense: c7::DenseState::Off,
            dense_error: None,
            failed_proposes: 0,
            unpublished_misses: 0,
        }
    );

    // The killed daemon leaves a stale socket, which the next start replaces.
    drop(daemon);
    let _daemon = start(&scratch);
    let digest: c7::DigestResponse =
        serde_json::from_slice(&ok(&socket, "digest", &json!({"limit": 5}))).unwrap();
    assert_eq!(digest.items.len(), 0);
    let found: c7::SearchResponse =
        serde_json::from_slice(&ok(&socket, "search", &json!({"q": "dentist"}))).unwrap();
    assert_eq!(found.facts[0].fact_id.as_deref(), Some(fact_id.as_str()));
}

#[test]
fn refusals_carry_their_c7_code_and_status() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let invalid = (400, c7::ErrorCode::InvalidRequest);
    assert_eq!(refused(post(&socket, "search", &json!({"q": 5}))), invalid);
    assert_eq!(
        refused(post(&socket, "health", &json!({"verbose": true}))),
        invalid
    );
    assert_eq!(
        refused(exchange(&socket, &request("/v1/search", b"{"))),
        invalid
    );
    assert_eq!(
        refused(exchange(&socket, b"POST /v1/health HTTP/1.1\r\n\r\n")),
        invalid
    );

    let unknown = (404, c7::ErrorCode::UnknownMethod);
    assert_eq!(refused(post(&socket, "forget", &json!({}))), unknown);
    assert_eq!(
        refused(exchange(&socket, &request("/search", b"{}"))),
        unknown
    );
    let get = b"GET /v1/health HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}";
    assert_eq!(refused(exchange(&socket, get)), unknown);

    assert_eq!(
        refused(post(
            &socket,
            "resolve",
            &json!({"proposal_id": "prop:404", "action": "accept"})
        )),
        (404, c7::ErrorCode::NotFound)
    );

    let proposal_id = queue_weak_address(&socket);
    let mut live = weak_address();
    live["ttl_kind"] = json!("never_cache");
    let replace = json!({"proposal_id": proposal_id, "action": "replace", "replacement": live});
    assert_eq!(
        refused(post(&socket, "resolve", &replace)),
        (409, c7::ErrorCode::ReplacementRefused)
    );
    let digest: c7::DigestResponse =
        serde_json::from_slice(&ok(&socket, "digest", &json!({}))).unwrap();
    assert_eq!(
        digest.items.len(),
        1,
        "a refused replacement leaves the item open"
    );
    let accept = json!({"proposal_id": proposal_id, "action": "accept"});
    ok(&socket, "resolve", &accept);
    assert_eq!(
        refused(post(&socket, "resolve", &accept)),
        (404, c7::ErrorCode::NotFound),
        "a closed item"
    );

    let mut padded = b"{}".to_vec();
    padded.resize(MAX_BODY, b' ');
    let (status, _) = exchange(&socket, &request("/v1/health", &padded));
    assert_eq!(status, 200, "a body of exactly 1 MiB");
    padded.push(b' ');
    assert_eq!(
        refused(exchange(&socket, &request("/v1/health", &padded))),
        (413, c7::ErrorCode::TooLarge)
    );
}

/// What `Server::serve` answers for uid `uid` on a socketpair whose client sent `sent`.
fn serve_pair(
    store: &Path,
    uid: u32,
    setup: impl FnOnce(&ulei_garden::api::Garden),
    sent: &[u8],
) -> (u16, Vec<u8>) {
    let garden = open(store).unwrap();
    setup(&garden);
    let (mut client, server) = UnixStream::pair().unwrap();
    client.write_all(sent).unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    Server::new(garden, uid, TIMEOUT).serve(server);
    parse(&read_all(&mut client))
}

#[test]
fn peer_of_another_uid_is_forbidden() {
    let scratch = Scratch::new();
    let other = rustix::process::geteuid().as_raw().wrapping_add(1);
    let sent = request("/v1/health", b"{}");
    assert_eq!(
        refused(serve_pair(&scratch.store(), other, |_| {}, &sent)),
        (403, c7::ErrorCode::Forbidden)
    );
}

#[test]
fn failing_store_answers_store_failed() {
    let scratch = Scratch::new();
    let own = rustix::process::geteuid().as_raw();
    let body = owner_fact("Cleo Placeholder", "Cleo Placeholder grows tomatoes.").to_string();
    let read_only = |g: &ulei_garden::api::Garden| {
        g.store()
            .conn()
            .execute_batch("PRAGMA query_only = 1")
            .unwrap();
    };
    assert_eq!(
        refused(serve_pair(
            &scratch.store(),
            own,
            read_only,
            &request("/v1/propose", body.as_bytes())
        )),
        (500, c7::ErrorCode::StoreFailed)
    );
}

#[test]
fn unusable_config_stops_the_start_with_one_line_naming_the_key() {
    let scratch = Scratch::new();
    let store = scratch.store();
    let socket = scratch.socket();
    let long = scratch.0.join("x".repeat(120));
    let cases = [
        (
            format!(
                "[garden]\nstore = \"{}\"\nsocket = \"{}\"\nflavour = \"mint\"\n",
                store.display(),
                socket.display()
            ),
            "unknown key garden.flavour",
        ),
        (
            format!("[garden]\nstore = \"{}\"\n", store.display()),
            "garden.socket is missing",
        ),
        (
            format!("[garden]\nsocket = \"{}\"\n", socket.display()),
            "garden.store is missing",
        ),
        (
            format!(
                "[garden]\nstore = \"{}\"\nsocket = \"{}\"\n",
                store.display(),
                long.display()
            ),
            "at most 107 are allowed",
        ),
    ];
    for (text, expected) in cases {
        let Started::Exited(code, stderr) = launch(&scratch.config(&text)) else {
            panic!("daemon started on {text:?}");
        };
        assert_eq!(code, Some(1), "{stderr}");
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        assert!(stderr.starts_with("garden: "), "{stderr}");
        assert!(stderr.contains(expected), "{expected}: {stderr}");
        assert!(!socket.exists(), "{text}");
    }
}
