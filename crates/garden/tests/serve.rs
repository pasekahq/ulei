//! `ulei-garden serve` through the built binary and raw HTTP on its socket; the peer check and
//! a store failure through `ulei_garden::serve::Server` on a socketpair. All people, places and
//! texts are fictional.

#![allow(missing_docs, clippy::unwrap_used, clippy::needless_pass_by_value)]

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use ulei_garden::api::ApiConfig;
use ulei_garden::miss::{self, ProducerKey};
use ulei_garden::model::{
    Attrs, Entity, EntityId, EntityKind, Fact, FactId, Provenance, Sensitivity, SourceKind,
    TtlKind, TtlTable,
};
use ulei_garden::serve::{MAX_BODY, Server, Settings, TIMEOUT, open};
use ulei_garden::store::{Store, starting_domains};
use ulei_garden::wire::c7;
use ulei_garden::worker::{Schedule, Wake};

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

    fn key(&self) -> PathBuf {
        key_file(&self.0)
    }

    /// The config lines naming the producer key in `[garden]`, then the inbox socket, which
    /// nobody serves here.
    fn key_line(&self) -> String {
        format!("producer_key_file = \"{}\"\n", self.key().display())
    }

    fn inbox_section(&self) -> String {
        format!(
            "[inbox]\nsocket = \"{}\"\n",
            self.0.join("inbox.sock").display()
        )
    }

    /// `ulei.toml` holding `text`.
    fn config(&self, text: &str) -> PathBuf {
        let path = self.0.join("ulei.toml");
        fs::write(&path, text).unwrap();
        path
    }

    /// `ulei.toml` naming the store and the socket.
    fn full_config(&self) -> PathBuf {
        self.config_with("")
    }

    /// `ulei.toml` naming the store, the socket and the producer key on lines 2 to 4, then
    /// `extra` from line 5, then the inbox socket.
    fn config_with(&self, extra: &str) -> PathBuf {
        self.config(&format!(
            "[garden]\nstore = \"{}\"\nsocket = \"{}\"\n{}{extra}{}",
            self.store().display(),
            self.socket().display(),
            self.key_line(),
            self.inbox_section()
        ))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The producer key file in `dir`, written owner-only with 32 synthetic bytes on first use.
fn key_file(dir: &Path) -> PathBuf {
    let path = dir.join("producer.key");
    if !path.exists() {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(&[0x5a; 32])
            .unwrap();
    }
    path
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
    start_with(scratch, "")
}

/// The daemon of [`Scratch::config_with`] `extra`.
fn start_with(scratch: &Scratch, extra: &str) -> Daemon {
    match launch(&scratch.config_with(extra)) {
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
    let mut robot = owner_fact("Rex Sample", "Rex Sample hums.");
    robot["kind"] = json!("robot");
    assert_eq!(refused(post(&socket, "propose", &robot)), invalid);
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

/// The default settings of the store at `store`.
fn settings(store: &Path) -> Settings {
    Settings {
        store: store.to_owned(),
        socket: store.with_extension("sock"),
        producer_key: ProducerKey::read(
            &key_file(store.parent().unwrap()),
            rustix::process::geteuid().as_raw(),
        )
        .unwrap(),
        inbox_socket: store.with_extension("inbox"),
        domains: starting_domains().unwrap(),
        ttl: TtlTable::default(),
        api: ApiConfig::default(),
        schedule: Schedule::default(),
    }
}

/// What `Server::serve` answers for uid `uid` on a socketpair whose client sent `sent`.
fn serve_pair(
    store: &Path,
    uid: u32,
    setup: impl FnOnce(&ulei_garden::api::Garden),
    sent: &[u8],
) -> (u16, Vec<u8>) {
    let garden = open(&settings(store)).unwrap();
    setup(&garden);
    let (mut client, server) = UnixStream::pair().unwrap();
    client.write_all(sent).unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    Server::new(garden, uid, TIMEOUT, Arc::new(Wake::default())).serve(server);
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
fn a_propose_the_store_rolls_back_is_counted_in_health_and_named_on_stderr() {
    let scratch = Scratch::new();
    let mut daemon = start(&scratch);
    let socket = scratch.socket();
    ok(
        &socket,
        "propose",
        &owner_fact("Bram Sample", "Bram Sample sees the dentist on Fridays."),
    );
    let count = |table: &str| -> i64 {
        Store::open(&scratch.store())
            .unwrap()
            .conn()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    };
    let before = [count("edge_proposal"), count("entities"), count("facts")];
    // The proposal row and the new entity are written; the fact is refused.
    Store::open(&scratch.store())
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TRIGGER refuse_facts BEFORE INSERT ON facts
             BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
        )
        .unwrap();
    let candidate = owner_fact("Cleo Placeholder", "Cleo Placeholder grows tomatoes.");
    assert_eq!(
        refused(post(&socket, "propose", &candidate)),
        (500, c7::ErrorCode::StoreFailed)
    );
    assert_eq!(
        [count("edge_proposal"), count("entities"), count("facts")],
        before
    );
    let health: c7::Health = serde_json::from_slice(&ok(&socket, "health", &json!({}))).unwrap();
    assert_eq!(health.failed_proposes, 1);

    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    let mut stderr = String::new();
    daemon
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(stderr, "garden: propose failed: sqlite_constraint\n");
}

#[test]
fn a_failed_worker_pass_is_one_stderr_line_and_requests_are_still_served() {
    let scratch = Scratch::new();
    let at = now() - 600;
    {
        let mut garden = open(&settings(&scratch.store())).unwrap();
        let store = garden.store_mut();
        store
            .upsert_entity(&Entity {
                id: EntityId("pl-kiosk".into()),
                kind: EntityKind::Place,
                name: "Harbour Kiosk".into(),
                aliases: Vec::new(),
                domain: None,
                attrs: Attrs::None,
            })
            .unwrap();
        store
            .insert_fact(&Fact {
                id: FactId("f-kiosk".into()),
                entity: EntityId("pl-kiosk".into()),
                text: "Harbour Kiosk sells ice cream today.".into(),
                domain: None,
                attribute: None,
                ttl: TtlKind::General,
                valid_from: at,
                valid_to: None,
                recorded_at: at,
                superseded_at: None,
                superseded_by: None,
                provenance: Provenance {
                    kind: SourceKind::Internet,
                    source_ref: "https://example.invalid/kiosk".into(),
                    observed_at: at,
                    quote: None,
                    proof_count: 1,
                },
                expires_at: Some(at + 60),
                sensitivity: Sensitivity::Public,
                confidence: 1.0,
            })
            .unwrap();
        store
            .conn()
            .execute_batch(
                "CREATE TRIGGER refuse_close BEFORE UPDATE ON facts
                 BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
            )
            .unwrap();
    }
    let mut daemon = start(&scratch);
    // The fact expired before the start, so the first expiry pass is due at once.
    let mut stderr = BufReader::new(daemon.0.stderr.take().unwrap());
    let mut line = String::new();
    stderr.read_line(&mut line).unwrap();
    assert_eq!(line, "garden: worker: sqlite_constraint\n");
    let health: c7::Health =
        serde_json::from_slice(&ok(&scratch.socket(), "health", &json!({}))).unwrap();
    assert_eq!(health.version, ulei_garden::serve::VERSION);
    let found = search(&scratch.socket(), &json!({"q": "ice cream"}));
    assert_eq!(found.facts.len(), 0, "expired, though its pass failed");

    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    let mut rest = String::new();
    stderr.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "");
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
                "[garden]\nstore = \"{}\"\nsocket = \"{}\"\n{}{}",
                store.display(),
                long.display(),
                scratch.key_line(),
                scratch.inbox_section()
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

fn propose(socket: &Path, candidate: &Value) -> c7::ProposeResponse {
    serde_json::from_slice(&ok(socket, "propose", candidate)).unwrap()
}

fn search(socket: &Path, request: &Value) -> c7::SearchResponse {
    serde_json::from_slice(&ok(socket, "search", request)).unwrap()
}

fn now() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    i64::try_from(elapsed.as_secs()).unwrap()
}

/// An internet fact about `entity` of `ttl_kind`, observed at `observed_at`.
fn internet_fact(entity: &str, fact: &str, ttl_kind: &str, observed_at: i64) -> Value {
    json!({
        "fact": fact,
        "entity": entity,
        "source_ref": "https://example.invalid/page",
        "kind": "place",
        "valid_from": 1_700_000_000,
        "observed_at": observed_at,
        "source_kind": "internet",
        "ttl_kind": ttl_kind,
    })
}

#[test]
fn domains_file_replaces_the_starting_domains_as_the_seed() {
    let scratch = Scratch::new();
    let file = scratch.0.join("domains.json");
    fs::write(
        &file,
        r#"[{"slug": "herbs", "label": "Herbs", "qid": "Q7"},
            {"slug": "basil", "label": "Basil", "parent": "herbs"}]"#,
    )
    .unwrap();
    let _daemon = start_with(
        &scratch,
        &format!("domains_file = \"{}\"\n", file.display()),
    );
    let socket = scratch.socket();
    let mut own = owner_fact("Dina Sample", "Dina Sample grows basil on the balcony.");
    own["domain"] = json!("basil");
    assert_eq!(propose(&socket, &own).status, c7::ProposalStatus::Accepted);
    let built_in = owner_fact("Dina Sample", "Dina Sample sees the dentist in May.");
    let refused = propose(&socket, &built_in);
    assert_eq!(refused.status, c7::ProposalStatus::Rejected);
    assert_eq!(refused.reason, Some(c7::ProposalReason::Invalid));
}

#[test]
fn domains_file_on_a_seeded_store_adds_its_new_domains_and_keeps_the_stored_ones() {
    let scratch = Scratch::new();
    drop(start(&scratch));
    let file = scratch.0.join("domains.json");
    fs::write(
        &file,
        r#"[{"slug": "herbs", "label": "Herbs", "qid": "Q7"},
            {"slug": "doctors", "label": "Medics", "parent": "herbs"}]"#,
    )
    .unwrap();
    let daemon = start_with(
        &scratch,
        &format!("domains_file = \"{}\"\n", file.display()),
    );
    let socket = scratch.socket();
    for domain in ["herbs", "doctors", "theatre"] {
        let mut own = owner_fact("Dina Sample", &format!("Dina Sample is into {domain}."));
        own["domain"] = json!(domain);
        assert_eq!(
            propose(&socket, &own).status,
            c7::ProposalStatus::Accepted,
            "{domain}"
        );
    }
    drop(daemon);
    let store = Store::open(&scratch.store()).unwrap();
    let doctors = store.domain("doctors").unwrap().unwrap();
    assert_eq!(
        (doctors.label.as_str(), doctors.parent.as_deref()),
        ("Doctors", Some("health"))
    );
    assert_eq!(store.domains().unwrap().len(), 21);
}

#[test]
fn ttl_rows_of_the_config_set_expires_at_and_other_kinds_keep_their_defaults() {
    let observed = 1_700_000_000;
    let weather = internet_fact("Fogtown", "Fogtown expects drizzle.", "weather", observed);

    let defaults = Scratch::new();
    let daemon = start(&defaults);
    let stale = propose(&defaults.socket(), &weather);
    assert_eq!(
        stale.status,
        c7::ProposalStatus::Rejected,
        "three hours old"
    );
    drop(daemon);

    let scratch = Scratch::new();
    let _daemon = start_with(
        &scratch,
        "[garden.ttl]\nweather = 400000000\nplace = \"never\"\n",
    );
    let socket = scratch.socket();
    let place = internet_fact(
        "Mill Square",
        "Mill Square hosts a fountain.",
        "place",
        observed,
    );
    let fresh = now();
    let news = internet_fact(
        "Pebble Gazette",
        "Pebble Gazette prints a quiz.",
        "news",
        fresh,
    );
    for candidate in [&weather, &place, &news] {
        assert_eq!(
            propose(&socket, candidate).status,
            c7::ProposalStatus::Accepted
        );
    }
    let expires = |q: &str| {
        let found = search(&socket, &json!({"q": q}));
        assert_eq!(found.facts.len(), 1, "{q}");
        found.facts[0].expires_at
    };
    assert_eq!(expires("drizzle"), Some(2_100_000_000.0));
    assert_eq!(expires("fountain"), None);
    #[allow(clippy::cast_precision_loss)]
    let a_day_later = (fresh + 86_400) as f64;
    assert_eq!(expires("quiz"), Some(a_day_later));
}

#[test]
fn min_confidence_of_the_config_decides_the_same_candidate() {
    let mut unsure = owner_fact("Ezra Sample", "Ezra Sample plays the cello.");
    unsure["confidence"] = json!(0.5);

    let defaults = Scratch::new();
    let daemon = start(&defaults);
    let queued = propose(&defaults.socket(), &unsure);
    assert_eq!(queued.status, c7::ProposalStatus::Queued);
    assert_eq!(queued.reason, Some(c7::ProposalReason::LowConfidence));
    drop(daemon);

    let scratch = Scratch::new();
    let _daemon = start_with(&scratch, "[garden.edge]\nmin_confidence = \"0.4\"\n");
    let socket = scratch.socket();
    assert_eq!(
        propose(&socket, &unsure).status,
        c7::ProposalStatus::Accepted
    );
}

#[test]
fn search_k_of_the_config_caps_a_search_without_k() {
    let scratch = Scratch::new();
    let _daemon = start_with(&scratch, "[garden.api]\nsearch_k = 2\n");
    let socket = scratch.socket();
    for name in ["Fay Sample", "Gus Sample", "Hal Sample"] {
        let fact = format!("{name} keeps a tulip garden.");
        assert_eq!(
            propose(&socket, &owner_fact(name, &fact)).status,
            c7::ProposalStatus::Accepted
        );
    }
    assert_eq!(search(&socket, &json!({"q": "tulip"})).facts.len(), 2);
    assert_eq!(
        search(&socket, &json!({"q": "tulip", "k": 3})).facts.len(),
        3
    );
}

#[test]
fn bad_garden_value_stops_the_start_with_one_line_naming_the_key_and_line() {
    let scratch = Scratch::new();
    let domains = scratch.0.join("domains.json");
    let domains_file = format!("domains_file = \"{}\"\n", domains.display());
    let cases = [
        ("[garden.ttl]\nweather = -5\n", None, "garden.ttl.weather"),
        ("[garden.ttl]\nnews = 0\n", None, "garden.ttl.news"),
        ("[garden.ttl]\nlaw = \"forever\"\n", None, "garden.ttl.law"),
        ("[garden.ttl]\nprice = \"3600\"\n", None, "garden.ttl.price"),
        (
            "[garden.edge]\nmin_confidence = \"0.6x\"\n",
            None,
            "garden.edge.min_confidence",
        ),
        (
            "[garden.edge]\nmin_confidence = \"1.5\"\n",
            None,
            "garden.edge.min_confidence",
        ),
        (
            "[garden.edge]\nmin_confidence = 2\n",
            None,
            "garden.edge.min_confidence",
        ),
        ("[garden.api]\nsearch_k = 0\n", None, "garden.api.search_k"),
        ("[garden.api]\nsearch_k = 51\n", None, "garden.api.search_k"),
        (
            "[garden.schedule]\nexpire_interval = 0\n",
            None,
            "garden.schedule.expire_interval",
        ),
        (
            "[garden.schedule]\nmaintain_interval = -1\n",
            None,
            "garden.schedule.maintain_interval",
        ),
        (domains_file.as_str(), None, "garden.domains_file"),
        (
            domains_file.as_str(),
            Some("[{\"slug\": "),
            "garden.domains_file",
        ),
        (
            domains_file.as_str(),
            Some(r#"[{"slug": "herbs", "label": "Herbs", "qid": "7"}]"#),
            "garden.domains_file",
        ),
        (
            domains_file.as_str(),
            Some(r#"[{"slug": "Herbs!", "label": "Herbs"}]"#),
            "garden.domains_file",
        ),
        (
            domains_file.as_str(),
            Some(r#"[{"slug": "basil", "label": "Basil", "parent": "herbs"}]"#),
            "garden.domains_file",
        ),
        (
            domains_file.as_str(),
            Some(
                r#"[{"slug": "herbs", "label": "Herbs", "parent": "basil"},
                    {"slug": "basil", "label": "Basil", "parent": "herbs"}]"#,
            ),
            "garden.domains_file",
        ),
    ];
    for (extra, file, key) in cases {
        match file {
            Some(text) => fs::write(&domains, text).unwrap(),
            None => drop(fs::remove_file(&domains)),
        }
        let line = if extra.starts_with('[') { 6 } else { 5 };
        let config = scratch.config_with(extra);
        let Started::Exited(code, stderr) = launch(&config) else {
            panic!("daemon started on {extra:?} {file:?}");
        };
        assert_eq!(code, Some(1), "{stderr}");
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        let head = format!("garden: config {}:{line}: {key} ", config.display());
        assert!(stderr.starts_with(&head), "{head}: {stderr}");
        assert!(!scratch.socket().exists(), "{extra}");
        assert!(!scratch.store().exists(), "{extra}");
    }
}

#[test]
fn store_is_created_owner_only_and_a_wider_existing_file_is_refused() {
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let scratch = Scratch::new();
    let store = scratch.0.join("data/deep/garden.sqlite3");
    let config = scratch.config(&format!(
        "[garden]\nstore = \"{}\"\nsocket = \"{}\"\n{}{}",
        store.display(),
        scratch.socket().display(),
        scratch.key_line(),
        scratch.inbox_section()
    ));
    let Started::Ready(daemon) = launch(&config) else {
        panic!("daemon did not start");
    };
    assert_eq!(mode(&store), 0o600);
    assert_eq!(mode(store.parent().unwrap()), 0o700);
    assert_eq!(mode(&scratch.0.join("data")), 0o700);
    drop(daemon);

    let open = Scratch::new();
    fs::write(open.store(), b"").unwrap();
    fs::set_permissions(open.store(), fs::Permissions::from_mode(0o644)).unwrap();
    let Started::Exited(code, stderr) = launch(&open.full_config()) else {
        panic!("daemon started on a 0644 store");
    };
    assert_eq!(code, Some(1), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains("mode 0644"), "{stderr}");
    assert_eq!(mode(&open.store()), 0o644);
    assert!(!open.socket().exists());
}

#[test]
fn bad_producer_key_or_missing_inbox_socket_stops_the_start_with_one_line_naming_the_key() {
    let scratch = Scratch::new();
    let key = scratch.0.join("bad.key");
    let write_key = |bytes: &[u8], mode: u32| {
        let _ = fs::remove_file(&key);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&key)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(mode)).unwrap();
    };
    let head = format!(
        "[garden]\nstore = \"{}\"\nsocket = \"{}\"\n",
        scratch.store().display(),
        scratch.socket().display()
    );
    let key_line = format!("producer_key_file = \"{}\"\n", key.display());
    let keyed = format!("{head}{key_line}{}", scratch.inbox_section());
    let cases: [(&str, &[u8], u32, &str, &str); 4] = [
        (
            &keyed,
            &[0x5a; 32],
            0o640,
            "garden.producer_key_file",
            "has mode 0640, it must be 0600 or stricter",
        ),
        (
            &keyed,
            &[0x5a; 31],
            0o600,
            "garden.producer_key_file",
            "holds 31 bytes, it must hold 32",
        ),
        (
            &format!("{head}{}", scratch.inbox_section()),
            &[0x5a; 32],
            0o600,
            "garden.producer_key_file",
            "is missing",
        ),
        (
            &format!("{head}{key_line}"),
            &[0x5a; 32],
            0o600,
            "inbox.socket",
            "is missing",
        ),
    ];
    for (text, bytes, mode, name, detail) in cases {
        write_key(bytes, mode);
        let Started::Exited(code, stderr) = launch(&scratch.config(text)) else {
            panic!("daemon started without {name}: {detail}");
        };
        assert_eq!(code, Some(1), "{stderr}");
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        assert!(stderr.starts_with("garden: config "), "{stderr}");
        assert!(stderr.contains(name) && stderr.contains(detail), "{stderr}");
        assert!(!scratch.socket().exists(), "{name}: {detail}");
    }
}

#[test]
fn a_miss_over_the_outbox_cap_drops_the_oldest_with_one_stderr_line() {
    let scratch = Scratch::new();
    {
        let mut garden = open(&settings(&scratch.store())).unwrap();
        garden
            .store_mut()
            .atomically(|s| {
                for n in 0..miss::MAX_UNPUBLISHED {
                    miss::queue(s, &format!("violin teacher number {n}"), now())?;
                }
                Ok::<_, ulei_garden::store::StoreError>(())
            })
            .unwrap();
    }
    let mut daemon = start(&scratch);
    let found = search(&scratch.socket(), &json!({"q": "harp teacher nearby"}));
    assert_eq!(found.facts.len(), 0);
    let health: c7::Health =
        serde_json::from_slice(&ok(&scratch.socket(), "health", &json!({}))).unwrap();
    assert_eq!(
        health.unpublished_misses,
        miss::MAX_UNPUBLISHED.unsigned_abs()
    );
    let oldest: i64 = Store::open(&scratch.store())
        .unwrap()
        .conn()
        .query_row(
            "SELECT count(*) FROM miss_outbox WHERE payload LIKE '%number 0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(oldest, 0, "the oldest row was dropped");

    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    let mut stderr = String::new();
    daemon
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(
        stderr,
        "garden: miss outbox holds 1000 unpublished misses, dropped the oldest\n"
    );
}
