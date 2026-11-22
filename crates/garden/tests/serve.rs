//! `ulei-garden serve` through the built binary and raw HTTP on its socket; the peer check, a
//! store failure and the effects of `seedbed_done` on the daemon through
//! `ulei_garden::serve::Server` on a socketpair. All people, places and texts are fictional.

#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    clippy::cast_precision_loss
)]

mod scripted;

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use ulei_garden::api::ApiConfig;
use ulei_garden::edge::EdgeConfig;
use ulei_garden::embed::{ONNX_FILE, TOKENIZER_FILE};
use ulei_garden::miss::{self, ProducerKey};
use ulei_garden::model::{
    Attrs, Entity, EntityId, EntityKind, Fact, FactId, Provenance, Sensitivity, SourceKind,
    TtlKind, TtlTable,
};
use ulei_garden::profile::Profile;
use ulei_garden::rerank;
use ulei_garden::second::Provider;
use ulei_garden::serve::{MAX_BODY, Server, Settings, TIMEOUT, open};
use ulei_garden::store::{ForgetQueued, SeedMark, Store, starting_domains};
use ulei_garden::wire::c7;
use ulei_garden::worker::{Schedule, SystemClock, Wake};

const GARDEN: &str = env!("CARGO_BIN_EXE_ulei-garden");
const EMBED: &str = env!("CARGO_BIN_EXE_ulei-garden-embed");

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
    decided(socket, &first);
    let queued = decided(socket, &weak_address());
    assert_eq!(queued.status, c7::ProposalStatus::Queued);
    queued.proposal_id
}

#[test]
fn every_method_round_trips_over_the_socket_and_the_store_outlives_a_restart() {
    let scratch = Scratch::new();
    // A one-fact store keeps bm25 near zero, below the default pointer gate.
    let open_gate = "[garden.api]\npointer_min_lexical = 0\n";
    let daemon = start_with(&scratch, open_gate);
    let socket = scratch.socket();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().unwrap()), 0o700);

    // `doctors` is a starting domain: the record is accepted only when the seed ran.
    let proposed = propose(
        &socket,
        &owner_fact("Bram Sample", "Bram Sample sees the dentist on Fridays."),
    );
    assert_eq!(proposed.status, c7::ProposalStatus::Seeded);
    assert_eq!(
        taken_refs(&socket, &json!({})),
        std::slice::from_ref(&proposed.proposal_id)
    );
    let processed = done(&socket, &json!({"items": [{"ref": proposed.proposal_id}]}));
    let outcome = &processed.items[0].outcomes[0];
    assert_eq!(outcome.status, c7::ProposalStatus::Accepted);
    let fact_id = outcome.fact_id.clone().unwrap();

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
    let _daemon = start_with(&scratch, open_gate);
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
    assert_eq!(refused(post(&socket, "erase", &json!({}))), unknown);
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
        second_level: Provider::None,
        reranker: rerank::Choice::None,
        embedder: None,
        core: None,
    }
}

/// What `Server::serve` answers for uid `uid` on a socketpair whose client sent `sent`.
fn serve_pair(
    store: &Path,
    uid: u32,
    setup: impl FnOnce(&ulei_garden::api::Garden),
    sent: &[u8],
) -> (u16, Vec<u8>) {
    let garden = open(&settings(store), None).unwrap();
    setup(&garden);
    let (mut client, server) = UnixStream::pair().unwrap();
    client.write_all(sent).unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    Server::new(garden, uid, TIMEOUT, Arc::new(Wake::default()), SystemClock).serve(server);
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
    let count = |table: &str| rows(&scratch, &format!("SELECT count(*) FROM {table}"));
    let before = [count("edge_proposal"), count("seedbed")];
    Store::open(&scratch.store())
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TRIGGER refuse_records BEFORE INSERT ON seedbed
             BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
        )
        .unwrap();
    let candidate = owner_fact("Cleo Placeholder", "Cleo Placeholder grows tomatoes.");
    assert_eq!(
        refused(post(&socket, "propose", &candidate)),
        (500, c7::ErrorCode::StoreFailed)
    );
    assert_eq!([count("edge_proposal"), count("seedbed")], before);
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
        let mut garden = open(&settings(&scratch.store()), None).unwrap();
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

#[test]
fn an_ambiguous_name_waits_in_the_digest_until_resolve_names_its_entity() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    for name in ["Mira Velde", "Мира Валда"] {
        let first = owner_fact(name, &format!("{name} sees the dentist."));
        assert_eq!(
            decided(&socket, &first).status,
            c7::ProposalStatus::Accepted
        );
    }
    let queued = decided(&socket, &owner_fact("Mira Valde", "Mira rows on Sundays."));
    assert_eq!(
        (queued.status, queued.reason),
        (
            c7::ProposalStatus::Queued,
            Some(c7::ProposalReason::AmbiguousEntity)
        )
    );
    let digest: c7::DigestResponse =
        serde_json::from_slice(&ok(&socket, "digest", &json!({}))).unwrap();
    let named = digest.items[0].entity_candidates.clone().unwrap();
    assert_eq!(named.len(), 2);
    let velde = named.iter().find(|c| c.name == "Mira Velde").unwrap();
    let resolve = |entity: Option<&str>| {
        let mut req = json!({"proposal_id": queued.proposal_id, "action": "accept"});
        if let Some(e) = entity {
            req["entity"] = json!(e);
        }
        post(&socket, "resolve", &req)
    };
    let invalid = (400, c7::ErrorCode::InvalidRequest);
    assert_eq!(refused(resolve(None)), invalid);
    assert_eq!(refused(resolve(Some("mira-valde"))), invalid);
    let (status, body) = resolve(Some(&velde.entity));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let found = search(&socket, &json!({"q": "rows Sundays"}));
    assert_eq!(found.facts.len(), 1);
    assert_eq!(found.facts[0].entity, velde.entity);
}

#[test]
fn search_meets_other_case_forms_and_latin_spellings_of_cyrillic_names() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let orsa = decided(
        &socket,
        &owner_fact("Орса Линд", "Ходит к стоматологу по пятницам."),
    );
    let bram = decided(
        &socket,
        &owner_fact("Bram Sample", "Bram Sample rows on Sundays."),
    );
    for p in [&orsa, &bram] {
        assert_eq!(p.status, c7::ProposalStatus::Accepted);
    }
    let orsa = orsa.fact_id.unwrap();
    for q in ["стоматологи", "Orsa Lind", "orsa"] {
        let found = search(&socket, &json!({"q": q}));
        let ids: Vec<&str> = found
            .facts
            .iter()
            .filter_map(|f| f.fact_id.as_deref())
            .collect();
        assert_eq!(ids, [orsa.as_str()], "{q}");
    }
}

fn propose(socket: &Path, candidate: &Value) -> c7::ProposeResponse {
    serde_json::from_slice(&ok(socket, "propose", candidate)).unwrap()
}

/// The answer to `candidate` on the night path: written by `propose`, then processed at once by
/// `seedbed_done`; a candidate refused at the write answers its refusal.
fn decided(socket: &Path, candidate: &Value) -> c7::ProposeResponse {
    let written = propose(socket, candidate);
    if written.status != c7::ProposalStatus::Seeded {
        return written;
    }
    let mut answer = done(socket, &json!({"items": [{"ref": written.proposal_id}]}));
    answer.items.remove(0).outcomes.remove(0)
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
fn a_propose_without_an_entity_is_seeded_and_the_night_refuses_it_invalid_storing_no_fact() {
    let scratch = Scratch::new();
    let daemon = start(&scratch);
    let socket = scratch.socket();
    let written = propose(&socket, &lifted());
    assert_eq!(written.status, c7::ProposalStatus::Seeded);
    let answer = done(&socket, &json!({"items": [{"ref": written.proposal_id}]}));
    let r = &answer.items[0].outcomes[0];
    assert_eq!(r.status, c7::ProposalStatus::Rejected);
    assert_eq!(r.reason, Some(c7::ProposalReason::Invalid));
    assert_eq!(search(&socket, &json!({"q": "clinic"})).facts.len(), 0);
    drop(daemon);
    let store = Store::open(&scratch.store()).unwrap();
    let written: i64 = store
        .conn()
        .query_row(
            "SELECT (SELECT count(*) FROM entities) + (SELECT count(*) FROM facts)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(written, 0);
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
    assert_eq!(decided(&socket, &own).status, c7::ProposalStatus::Accepted);
    let built_in = owner_fact("Dina Sample", "Dina Sample sees the dentist in May.");
    let refused = decided(&socket, &built_in);
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
            decided(&socket, &own).status,
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
    let stale = decided(&defaults.socket(), &weather);
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
            decided(&socket, candidate).status,
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
fn the_recheck_schedule_is_read_from_the_config() {
    let scratch = Scratch::new();
    let config =
        scratch.config_with("[garden.schedule]\nrecheck_interval = 120\nrecheck_batch = 3\n");
    let schedule = Settings::read(Some(&config)).unwrap().schedule;
    assert_eq!(
        schedule,
        Schedule {
            recheck_interval: 120,
            recheck_batch: 3,
            ..Schedule::default()
        }
    );
}

#[test]
fn the_reconcile_threshold_is_read_from_the_config() {
    let scratch = Scratch::new();
    let config = scratch.config_with("[garden.edge]\nreconcile_min_cosine = \"0.5\"\n");
    let edge = Settings::read(Some(&config)).unwrap().api.edge;
    assert_eq!(
        edge,
        EdgeConfig {
            reconcile_min_cosine: 0.5,
            ..EdgeConfig::default()
        }
    );
}

#[test]
fn search_flags_law_facts_with_recheck_before_acting_when_the_config_overrides_their_ttl() {
    let scratch = Scratch::new();
    let config = scratch.config_with("[garden.ttl]\nlaw = 86400\n");
    let mut garden = open(&Settings::read(Some(&config)).unwrap(), None).unwrap();
    let at = 1_700_000_000;
    for (fact, ttl_kind) in [
        ("Mill Square bans kites on Sundays.", "law"),
        ("Mill Square hosts a fountain.", "general"),
    ] {
        let candidate = internet_fact("Mill Square", fact, ttl_kind, at);
        let req = serde_json::from_value(candidate).unwrap();
        let written = garden.garden_propose(&req, at).unwrap();
        let done = c7::SeedbedDoneRequest {
            items: vec![c7::SeedbedDoneItem {
                r#ref: written.proposal_id,
                candidates: None,
            }],
        };
        let answer = garden.garden_seedbed_done(&done, at).unwrap();
        let status = answer.items[0].outcomes[0].status;
        assert_eq!(status, c7::ProposalStatus::Accepted, "{fact}");
    }
    let mut found = |q: &str| {
        let req = c7::SearchRequest {
            q: q.into(),
            k: None,
            as_of: None,
            private: None,
        };
        let mut facts = garden.garden_search(&req, at + 10).unwrap().response.facts;
        assert_eq!(facts.len(), 1, "{q}");
        facts.remove(0)
    };
    let law = found("kites");
    assert_eq!(law.recheck_before_acting, Some(true));
    assert_eq!(law.expires_at, Some(1_700_086_400.0));
    let general = found("fountain");
    assert_eq!(general.recheck_before_acting, None);
}

#[test]
fn min_confidence_of_the_config_decides_the_same_candidate() {
    let mut unsure = owner_fact("Ezra Sample", "Ezra Sample plays the cello.");
    unsure["confidence"] = json!(0.5);

    let defaults = Scratch::new();
    let daemon = start(&defaults);
    let queued = decided(&defaults.socket(), &unsure);
    assert_eq!(queued.status, c7::ProposalStatus::Queued);
    assert_eq!(queued.reason, Some(c7::ProposalReason::LowConfidence));
    drop(daemon);

    let scratch = Scratch::new();
    let _daemon = start_with(&scratch, "[garden.edge]\nmin_confidence = \"0.4\"\n");
    let socket = scratch.socket();
    assert_eq!(
        decided(&socket, &unsure).status,
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
            decided(&socket, &owner_fact(name, &fact)).status,
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
        (
            "[garden.schedule]\nrecheck_interval = 0\n",
            None,
            "garden.schedule.recheck_interval",
        ),
        (
            "[garden.schedule]\nrecheck_batch = 0\n",
            None,
            "garden.schedule.recheck_batch",
        ),
        (
            "[garden.schedule]\nrecheck_batch = -8\n",
            None,
            "garden.schedule.recheck_batch",
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
        refuses_start(&scratch, extra, key);
    }
}

/// The start with `extra` in the config exits 1 with one stderr line naming `key` and its line
/// before it creates the socket or the store.
fn refuses_start(scratch: &Scratch, extra: &str, key: &str) {
    let line = if extra.starts_with('[') { 6 } else { 5 };
    let config = scratch.config_with(extra);
    let Started::Exited(code, stderr) = launch(&config) else {
        panic!("daemon started on {extra:?}");
    };
    assert_eq!(code, Some(1), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    let head = format!("garden: config {}:{line}: {key} ", config.display());
    assert!(stderr.starts_with(&head), "{head}: {stderr}");
    assert!(!scratch.socket().exists(), "{extra}");
    assert!(!scratch.store().exists(), "{extra}");
}

#[test]
fn a_bad_core_char_limit_stops_the_start_with_one_line_naming_the_key() {
    let scratch = Scratch::new();
    for value in ["0", "-3"] {
        refuses_start(
            &scratch,
            &format!("core_char_limit = {value}\n"),
            "garden.core_char_limit",
        );
    }
}

/// The `[garden]` lines of a core profile at `file` with a budget of `limit` characters.
fn core_lines(file: &Path, limit: usize) -> String {
    format!(
        "core_file = \"{}\"\ncore_char_limit = {limit}\n",
        file.display()
    )
}

#[test]
fn the_daemon_renders_the_core_at_start_and_after_seedbed_done_and_counts_what_its_budget_leaves_out()
 {
    let scratch = Scratch::new();
    let core = scratch.0.join("brain/CORE.md");
    let ada = "Ada Example: Ada sings in a choir.";
    let bram = "Bram Sample: Bram keeps bees.";
    let mut daemon = start_with(&scratch, &core_lines(&core, ada.chars().count()));
    assert_eq!(fs::read_to_string(&core).unwrap(), "");
    assert_eq!(
        fs::metadata(&core).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let socket = scratch.socket();
    for (entity, text) in [
        ("Ada Example", "Ada sings in a choir."),
        ("Bram Sample", "Bram keeps bees."),
    ] {
        let mut candidate = owner_fact(entity, text);
        candidate["pin"] = json!(true);
        assert_eq!(
            decided(&socket, &candidate).status,
            c7::ProposalStatus::Accepted
        );
        assert_eq!(fs::read_to_string(&core).unwrap(), ada, "after {entity}");
    }
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
        "garden: core profile over its budget, 1 facts left out\n"
    );
    drop(daemon);

    fs::remove_file(&core).unwrap();
    let _daemon = start_with(&scratch, &core_lines(&core, 1375));
    assert_eq!(
        fs::read_to_string(&core).unwrap(),
        format!("{ada}\n§\n{bram}")
    );
}

#[test]
fn bad_pointer_threshold_stops_the_start_with_one_line_naming_the_key() {
    let scratch = Scratch::new();
    for (extra, key) in [
        (
            "pointer_min_cosine = \"1.5\"",
            "garden.api.pointer_min_cosine",
        ),
        (
            "pointer_min_cosine = \"0.3x\"",
            "garden.api.pointer_min_cosine",
        ),
        ("pointer_min_cosine = 2", "garden.api.pointer_min_cosine"),
        ("pointer_min_lexical = -1", "garden.api.pointer_min_lexical"),
        (
            "pointer_min_lexical = \"4,6\"",
            "garden.api.pointer_min_lexical",
        ),
    ] {
        refuses_start(&scratch, &format!("[garden.api]\n{extra}\n"), key);
    }
}

#[test]
fn bad_reconcile_threshold_stops_the_start_with_one_line_naming_the_key() {
    let scratch = Scratch::new();
    for value in ["\"1.5\"", "\"0.85x\"", "-1", "2"] {
        refuses_start(
            &scratch,
            &format!("[garden.edge]\nreconcile_min_cosine = {value}\n"),
            "garden.edge.reconcile_min_cosine",
        );
    }
}

#[test]
fn a_second_level_other_than_none_stops_the_start_with_one_line_naming_the_key() {
    let scratch = Scratch::new();
    let config = scratch.config_with("second_level = \"web\"\n");
    let Started::Exited(code, stderr) = launch(&config) else {
        panic!("daemon started with a web second level");
    };
    assert_eq!(code, Some(1), "{stderr}");
    assert_eq!(
        stderr,
        format!(
            "garden: config {}:5: garden.second_level \"web\": only \"none\" is built\n",
            config.display()
        )
    );
    assert!(!scratch.store().exists());
    drop(start_with(&scratch, "second_level = \"none\"\n"));
}

#[test]
fn a_reranker_other_than_none_stops_the_start_with_one_line_naming_the_key() {
    let scratch = Scratch::new();
    let config = scratch.config_with("reranker = \"x\"\n");
    let Started::Exited(code, stderr) = launch(&config) else {
        panic!("daemon started with reranker x");
    };
    assert_eq!(code, Some(1), "{stderr}");
    assert_eq!(
        stderr,
        format!(
            "garden: config {}:5: garden.reranker \"x\": only \"none\" is built\n",
            config.display()
        )
    );
    assert!(!scratch.store().exists());
    drop(start_with(&scratch, "reranker = \"none\"\n"));
}

#[test]
fn pointer_thresholds_of_the_config_reach_the_gate() {
    let fact = owner_fact("Ivo Sample", "Ivo Sample keeps bees on the roof.");
    let defaults = Scratch::new();
    let daemon = start(&defaults);
    decided(&defaults.socket(), &fact);
    let none: c7::PointersResponse =
        serde_json::from_slice(&ok(&defaults.socket(), "pointers", &json!({"q": "bees"}))).unwrap();
    assert_eq!(
        none.pointers.len(),
        0,
        "a one-fact store stays below the default gate"
    );
    drop(daemon);

    let scratch = Scratch::new();
    let _daemon = start_with(
        &scratch,
        "[garden.api]\npointer_min_cosine = 1\npointer_min_lexical = \"0.0\"\n",
    );
    decided(&scratch.socket(), &fact);
    let one: c7::PointersResponse =
        serde_json::from_slice(&ok(&scratch.socket(), "pointers", &json!({"q": "bees"}))).unwrap();
    assert_eq!(one.pointers.len(), 1);
    assert_eq!(one.pointers[0].entity, "Ivo Sample");
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
        let settings = settings(&scratch.store());
        let mut garden = open(&settings, None).unwrap();
        garden
            .store_mut()
            .atomically(|s| {
                for n in 0..miss::MAX_UNPUBLISHED {
                    let q = format!("violin teacher number {n}");
                    miss::queue(s, &settings.producer_key, &q, now())?;
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

/// An executable shell script `name` in the scratch directory running `body`.
fn script(scratch: &Scratch, name: &str, body: &str) -> PathBuf {
    let path = scratch.0.join(name);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)
        .unwrap()
        .write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
        .unwrap();
    path
}

/// The `[garden.embedder]` section enabling `helper` over the model directory `model_dir`, then
/// `extra`.
fn embedder_section(scratch: &Scratch, helper: &Path, model_dir: &Path, extra: &str) -> String {
    format!(
        "[garden.embedder]\nenabled = true\nhelper = \"{}\"\nmodel_dir = \"{}\"\nort_library = \"{}\"\n{extra}",
        helper.display(),
        model_dir.display(),
        scratch.0.join("libonnxruntime.so").display()
    )
}

/// The helper `ulei-garden-embed --embedder fake`, behind a script that adds the flag to the
/// arguments the daemon passes.
fn fake_helper(scratch: &Scratch) -> PathBuf {
    script(
        scratch,
        "fake-embed",
        &format!("exec '{EMBED}' --embedder fake \"$@\""),
    )
}

#[test]
fn a_helper_that_refuses_exits_or_stays_silent_stops_the_start_with_one_line() {
    let scratch = Scratch::new();
    let empty = scratch.0.join("no-model");
    fs::create_dir(&empty).unwrap();
    let cases = [
        (
            fake_helper(&scratch),
            "",
            format!("{ONNX_FILE}: No such file or directory"),
        ),
        (
            script(&scratch, "exits", "exit 3"),
            "",
            "exited before its ready line (exit status: 3)".to_owned(),
        ),
        (
            script(&scratch, "silent", "exec sleep 30"),
            "load_timeout = 1\n",
            "no ready line within 1s".to_owned(),
        ),
    ];
    for (helper, extra, reason) in cases {
        let section = embedder_section(&scratch, &helper, &empty, extra);
        let Started::Exited(code, stderr) = launch(&scratch.config_with(&section)) else {
            panic!("daemon started with {}", helper.display());
        };
        assert_eq!(code, Some(1), "{stderr}");
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        let head = format!("garden: embedder: {}: ", helper.display());
        assert!(stderr.starts_with(&head), "{head}: {stderr}");
        assert!(stderr.contains(&reason), "{reason}: {stderr}");
        assert!(!scratch.socket().exists(), "{stderr}");
    }

    let Started::Exited(code, stderr) =
        launch(&scratch.config_with("[garden.embedder]\nenabled = true\n"))
    else {
        panic!("daemon started without garden.embedder.helper");
    };
    assert_eq!(code, Some(1), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.contains("garden.embedder.helper is missing"),
        "{stderr}"
    );
    assert!(!scratch.socket().exists(), "{stderr}");
}

#[test]
fn health_answers_dense_ready_with_the_helper_and_off_without_it() {
    let dense = |scratch: &Scratch| {
        let health: c7::Health =
            serde_json::from_slice(&ok(&scratch.socket(), "health", &json!({}))).unwrap();
        health.dense
    };
    let lexical = Scratch::new();
    let _off = start(&lexical);
    assert_eq!(dense(&lexical), c7::DenseState::Off);

    let scratch = Scratch::new();
    let model = scratch.0.join("model");
    fs::create_dir_all(model.join(ONNX_FILE).parent().unwrap()).unwrap();
    fs::write(model.join(ONNX_FILE), "synthetic model").unwrap();
    fs::write(model.join(TOKENIZER_FILE), "{}").unwrap();
    let helper = fake_helper(&scratch);
    let _ready = start_with(&scratch, &embedder_section(&scratch, &helper, &model, ""));
    assert_eq!(dense(&scratch), c7::DenseState::Ready);
}

#[test]
fn a_helper_lost_after_the_start_turns_dense_down_and_answers_lexical_only_until_its_restart() {
    let scratch = Scratch::new();
    let helper = scripted::script(&scratch.0, "helper", &scripted::exits_after(1));
    let section = embedder_section(&scratch, &helper, &scratch.0, "");
    let mut daemon = start_with(&scratch, &section);
    let socket = scratch.socket();
    let health =
        || -> c7::Health { serde_json::from_slice(&ok(&socket, "health", &json!({}))).unwrap() };
    let query = json!({"q": "dentist on Fridays"});
    assert_eq!(search(&socket, &query).lexical_only, None);
    assert_eq!(search(&socket, &query).lexical_only, Some(true));
    let pointers: c7::PointersResponse =
        serde_json::from_slice(&ok(&socket, "pointers", &query)).unwrap();
    assert_eq!(pointers.lexical_only, Some(true));
    let down = health();
    assert_eq!(down.dense, c7::DenseState::Down);
    assert_eq!(
        down.dense_error.as_deref(),
        Some("helper pipe: the helper closed it")
    );

    let waiting = std::time::Instant::now();
    while health().dense != c7::DenseState::Ready {
        assert!(
            waiting.elapsed() < std::time::Duration::from_secs(10),
            "no restart"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(health().dense_error, None);
    assert_eq!(search(&socket, &query).lexical_only, None);

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
        "garden: embedder: down: helper pipe: the helper closed it\ngarden: embedder: back\n"
    );
}

/// The one integer `sql` selects from the store of `scratch`.
fn rows(scratch: &Scratch, sql: &str) -> i64 {
    Store::open(&scratch.store())
        .unwrap()
        .conn()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

/// Writes `candidate` into the seedbed of `scratch` at `written_at` in process and returns its
/// ref; a record that `disputes` a fact is resolved to that fact's entity.
fn write_seed(
    scratch: &Scratch,
    candidate: &Value,
    disputes: Option<&str>,
    written_at: i64,
) -> String {
    let mut store = Store::open(&scratch.store()).unwrap();
    let rival = disputes.map(|id| store.fact(&FactId(id.to_owned())).unwrap().unwrap());
    let seq = store
        .write_seed(
            &serde_json::from_value(candidate.clone()).unwrap(),
            rival.as_ref().map(|f| &f.entity),
            rival.as_ref().map(|f| &f.id),
            written_at,
        )
        .unwrap();
    format!("seed:{seq}")
}

fn seq_of(record: &str) -> i64 {
    record.strip_prefix("seed:").unwrap().parse().unwrap()
}

fn taken(socket: &Path, request: &Value) -> Vec<c7::SeedRecord> {
    let out: c7::SeedbedTakeResponse =
        serde_json::from_slice(&ok(socket, "seedbed_take", request)).unwrap();
    out.records
}

fn taken_refs(socket: &Path, request: &Value) -> Vec<String> {
    taken(socket, request)
        .into_iter()
        .map(|r| r.r#ref)
        .collect()
}

fn done(socket: &Path, request: &Value) -> c7::SeedbedDoneResponse {
    serde_json::from_slice(&ok(socket, "seedbed_done", request)).unwrap()
}

/// A candidate without `valid_from`, lifted from the archive without an entity.
fn lifted() -> Value {
    json!({
        "fact": "The clinic moved to 9 Sample Road.",
        "source_ref": "archive:41",
        "source_kind": "raw",
        "observed_at": 1_700_000_000,
    })
}

#[test]
fn seedbed_take_answers_unprocessed_records_oldest_first_until_seedbed_done_marks_them() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let t = now();
    let bees = write_seed(
        &scratch,
        &owner_fact("Bram Sample", "Bram keeps bees."),
        None,
        t - 200,
    );
    let choir = owner_fact("Ada Example", "Ada sings in a choir.");
    let sings = write_seed(&scratch, &choir, None, t - 300);
    let chess = write_seed(
        &scratch,
        &owner_fact("Cleo Sample", "Cleo plays chess."),
        None,
        t - 100,
    );
    assert!(
        seq_of(&bees) < seq_of(&sings),
        "oldest by written_at, not by seq"
    );
    let all = vec![sings.clone(), bees.clone(), chess.clone()];

    assert_eq!(taken_refs(&socket, &json!({})), all);
    assert_eq!(taken_refs(&socket, &json!({"limit": 2})), all[..2]);
    assert_eq!(taken_refs(&socket, &json!({"until": t - 150})), all[..2]);
    assert_eq!(
        taken(&socket, &json!({"limit": 1})),
        [c7::SeedRecord {
            r#ref: sings.clone(),
            candidate: serde_json::from_value(choir).unwrap(),
            written_at: (t - 300) as f64,
            conflicts_with: None,
        }]
    );
    assert_eq!(
        taken_refs(&socket, &json!({})),
        all,
        "a take leases nothing"
    );

    done(&socket, &json!({"items": [{"ref": sings}, {"ref": bees}]}));
    assert_eq!(taken_refs(&socket, &json!({})), [chess]);
}

#[test]
fn seedbed_done_writes_the_record_through_the_edge_from_its_written_at_once() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let said = now() - 3_600;
    let mut dentist = owner_fact("Cleo Sample", "Cleo Sample sees the dentist on Mondays.");
    dentist.as_object_mut().unwrap().remove("valid_from");
    let record = write_seed(&scratch, &dentist, None, said);

    let request = json!({"items": [{"ref": record}]});
    let answer = done(&socket, &request);
    assert_eq!(answer.items.len(), 1);
    assert_eq!(answer.items[0].r#ref, record);
    let [outcome] = answer.items[0].outcomes.as_slice() else {
        panic!("{answer:?}");
    };
    assert_eq!(outcome.status, c7::ProposalStatus::Accepted);
    let fact_id = outcome.fact_id.clone().unwrap();

    let found = search(&socket, &json!({"q": "dentist"}));
    assert_eq!(found.facts.len(), 1);
    assert_eq!(found.facts[0].fact_id.as_deref(), Some(fact_id.as_str()));
    assert_eq!(found.facts[0].valid_from, said as f64);
    assert_eq!(
        rows(
            &scratch,
            &format!("SELECT seed FROM edge_proposal WHERE fact_id = '{fact_id}'")
        ),
        seq_of(&record)
    );

    let writes = "SELECT (SELECT count(*) FROM edge_proposal) + (SELECT count(*) FROM facts)
        + (SELECT count(*) FROM entities) + (SELECT count(*) FROM pins)";
    let before = rows(&scratch, writes);
    assert_eq!(done(&socket, &request), answer, "the stored outcome");
    assert_eq!(rows(&scratch, writes), before);
}

#[test]
fn seedbed_done_decides_the_given_candidates_and_an_empty_array_writes_no_fact() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let t = now();
    let placed = write_seed(&scratch, &lifted(), None, t - 600);
    let dropped = write_seed(
        &scratch,
        &owner_fact("Bram Sample", "Bram keeps bees."),
        None,
        t - 500,
    );
    let placement =
        [("Sample Clinic", "place"), ("Ada Example", "person")].map(|(entity, kind)| {
            let mut c = lifted();
            c["entity"] = json!(entity);
            c["kind"] = json!(kind);
            c
        });

    let answer = done(
        &socket,
        &json!({"items": [
            {"ref": placed, "candidates": placement},
            {"ref": dropped, "candidates": []},
        ]}),
    );
    let statuses: Vec<_> = answer.items[0].outcomes.iter().map(|o| o.status).collect();
    assert_eq!(statuses, [c7::ProposalStatus::Accepted; 2]);
    assert_eq!(answer.items[1].outcomes, []);
    assert_eq!(rows(&scratch, "SELECT count(*) FROM facts"), 2);
    assert_eq!(
        rows(
            &scratch,
            &format!(
                "SELECT count(*) FROM edge_proposal WHERE seed = {}",
                seq_of(&placed)
            )
        ),
        2
    );
    assert_eq!(search(&socket, &json!({"q": "bees"})).facts.len(), 0);
    assert_eq!(taken_refs(&socket, &json!({})), Vec::<String>::new());
}

#[test]
fn seedbed_done_of_an_unknown_ref_is_not_found_before_any_item_is_processed() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let waiting = write_seed(
        &scratch,
        &owner_fact("Bram Sample", "Bram keeps bees."),
        None,
        now(),
    );
    assert_eq!(
        refused(post(
            &socket,
            "seedbed_done",
            &json!({"items": [{"ref": waiting}, {"ref": "seed:999"}]})
        )),
        (404, c7::ErrorCode::NotFound)
    );
    let invalid = (400, c7::ErrorCode::InvalidRequest);
    for items in [
        json!([]),
        json!([{"ref": "prop:1"}]),
        json!([{"ref": "seed:01"}]),
    ] {
        assert_eq!(
            refused(post(&socket, "seedbed_done", &json!({"items": items}))),
            invalid,
            "{items}"
        );
    }
    assert_eq!(taken_refs(&socket, &json!({})), [waiting]);
    assert_eq!(rows(&scratch, "SELECT count(*) FROM edge_proposal"), 0);
}

#[test]
fn a_record_disputed_in_the_day_becomes_a_digest_contradiction_at_processing() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let mut home = owner_fact("Ada Example", "Ada lives at 3 Old Road.");
    home["attribute"] = json!("address");
    let current = decided(&socket, &home).fact_id.unwrap();
    let record = write_seed(&scratch, &weak_address(), Some(&current), now());
    assert_eq!(
        taken(&socket, &json!({}))[0].conflicts_with.as_deref(),
        Some(current.as_str())
    );

    let answer = done(&socket, &json!({"items": [{"ref": record}]}));
    let [outcome] = answer.items[0].outcomes.as_slice() else {
        panic!("{answer:?}");
    };
    assert_eq!(outcome.status, c7::ProposalStatus::Queued);
    assert_eq!(outcome.reason, Some(c7::ProposalReason::Contradiction));
    assert_eq!(outcome.conflicts_with.as_deref(), Some(current.as_str()));
    let digest: c7::DigestResponse =
        serde_json::from_slice(&ok(&socket, "digest", &json!({}))).unwrap();
    assert_eq!(digest.items.len(), 1);
    assert_eq!(digest.items[0].proposal_id, outcome.proposal_id);
    assert_eq!(digest.items[0].reason, c7::DigestReason::Contradiction);
}

#[test]
fn seedbed_done_renders_the_core_profile_and_wakes_the_worker() {
    let scratch = Scratch::new();
    let core = scratch.0.join("CORE.md");
    let garden = open(&settings(&scratch.store()), None).unwrap();
    let mut pinned = owner_fact("Ada Example", "Ada sings in a choir.");
    pinned["pin"] = json!(true);
    let record = write_seed(&scratch, &pinned, None, now());
    let wake = Arc::new(Wake::default());
    let uid = rustix::process::geteuid().as_raw();
    let mut server = Server::new(garden, uid, TIMEOUT, Arc::clone(&wake), SystemClock)
        .with_profile(Arc::new(Profile::new(core.clone(), 1_000)));

    let (mut client, stream) = UnixStream::pair().unwrap();
    let body = json!({"items": [{"ref": record}]}).to_string();
    client
        .write_all(&request("/v1/seedbed_done", body.as_bytes()))
        .unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    server.serve(stream);
    let (status, out) = parse(&read_all(&mut client));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&out));
    assert!(
        wake.wait(Duration::ZERO),
        "the written fact woke the worker"
    );
    assert_eq!(
        fs::read_to_string(&core).unwrap(),
        "Ada Example: Ada sings in a choir."
    );
}

fn pointed(socket: &Path, request: &Value) -> Vec<c7::Pointer> {
    let out: c7::PointersResponse =
        serde_json::from_slice(&ok(socket, "pointers", request)).unwrap();
    out.pointers
}

#[test]
fn a_proposed_fact_is_found_unprocessed_with_its_expiry_until_seedbed_done_hands_over_its_fact() {
    let scratch = Scratch::new();
    // A store of one record keeps bm25 near zero, below the default pointer gate.
    let _daemon = start_with(&scratch, "[garden.api]\npointer_min_lexical = 0\n");
    let socket = scratch.socket();
    let t = now();
    let candidate = internet_fact("Cafe Lumen", "Cafe Lumen opens at nine.", "place", t);
    let written = propose(&socket, &candidate);
    assert_eq!(written.status, c7::ProposalStatus::Seeded);
    let expires_at = Some((t + 180 * 86_400) as f64);

    let q = json!({"q": "when does Cafe Lumen open"});
    let day = search(&socket, &q);
    let [record] = day.facts.as_slice() else {
        panic!("{day:?}");
    };
    assert_eq!(
        record.fact_id.as_deref(),
        Some(written.proposal_id.as_str())
    );
    assert_eq!(record.unprocessed, Some(true));
    assert_eq!(record.expires_at, expires_at);
    assert_eq!(record.entity, "Cafe Lumen");
    assert_eq!((record.pinned, record.proof_count), (None, None));
    let health: c7::Health = serde_json::from_slice(&ok(&socket, "health", &json!({}))).unwrap();
    assert_eq!(health.unpublished_misses, 0, "a record answered the search");
    assert_eq!(
        pointed(&socket, &q),
        [c7::Pointer {
            fact_id: written.proposal_id.clone(),
            entity: "Cafe Lumen".into(),
            kind: c7::EntityKind::Place,
            score: pointed(&socket, &q)[0].score,
            hint: "there is an unprocessed record about Cafe Lumen (place)".into(),
            unprocessed: Some(true),
            disputed: None,
        }]
    );

    let answer = done(&socket, &json!({"items": [{"ref": written.proposal_id}]}));
    let fact_id = answer.items[0].outcomes[0].fact_id.clone().unwrap();
    let night = search(&socket, &q);
    let [fact] = night.facts.as_slice() else {
        panic!("{night:?}");
    };
    assert_eq!(fact.fact_id.as_deref(), Some(fact_id.as_str()));
    assert_eq!((fact.unprocessed, fact.expires_at), (None, expires_at));
    let [pointer] = pointed(&socket, &q).try_into().unwrap();
    assert_eq!((pointer.fact_id, pointer.unprocessed), (fact_id, None));
}

fn forget(socket: &Path, request: &Value) -> Vec<(String, u64, u64)> {
    let out: c7::ForgetResponse = serde_json::from_slice(&ok(socket, "forget", request)).unwrap();
    out.items
        .into_iter()
        .map(|i| (i.r#ref, i.marks, i.queue))
        .collect()
}

fn forget_queue(scratch: &Scratch) -> Vec<ForgetQueued> {
    Store::open(&scratch.store())
        .unwrap()
        .forget_queue()
        .unwrap()
}

fn seed_marks(scratch: &Scratch, record: &str) -> Vec<SeedMark> {
    Store::open(&scratch.store())
        .unwrap()
        .seed_marks(seq_of(record))
        .unwrap()
}

/// Every row of `reconcile_queue`, as fact and candidate.
fn reconcile_queue(scratch: &Scratch) -> Vec<(String, String)> {
    let store = Store::open(&scratch.store()).unwrap();
    let mut st = store
        .conn()
        .prepare("SELECT fact, candidate FROM reconcile_queue ORDER BY fact")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Stands in for the night stage, which is not built: marks the queue rows of `r` taken.
fn night_takes(scratch: &Scratch, r: &str) {
    Store::open(&scratch.store())
        .unwrap()
        .conn()
        .execute(
            "UPDATE forget_queue SET taken_at = ?1 WHERE ref = ?2",
            rusqlite::params![now(), r],
        )
        .unwrap();
}

/// Seeds `candidate` over the socket, hands it to the edge and answers the record's ref and
/// the fact it wrote.
fn seeded_fact(socket: &Path, candidate: &Value) -> (String, String) {
    let record = propose(socket, candidate).proposal_id;
    let answer = done(socket, &json!({"items": [{"ref": record}]}));
    let fact = answer.items[0].outcomes[0].fact_id.clone().unwrap();
    (record, fact)
}

#[test]
fn a_forgotten_record_leaves_search_pointers_take_and_done_until_undo_brings_it_back() {
    let scratch = Scratch::new();
    let _daemon = start_with(&scratch, "[garden.api]\npointer_min_lexical = 0\n");
    let socket = scratch.socket();
    let t = now();
    let cafe = propose(
        &socket,
        &internet_fact("Cafe Lumen", "Cafe Lumen opens at nine.", "place", t),
    )
    .proposal_id;
    let bees = write_seed(
        &scratch,
        &owner_fact("Bram Sample", "Bram keeps bees."),
        None,
        t - 10,
    );
    let q = json!({"q": "when does Cafe Lumen open"});
    let ids = |found: c7::SearchResponse| -> Vec<String> {
        found.facts.into_iter().filter_map(|f| f.fact_id).collect()
    };
    assert_eq!(ids(search(&socket, &q)), std::slice::from_ref(&cafe));
    assert_eq!(pointed(&socket, &q).len(), 1);

    let request = json!({"refs": [cafe], "reason": "The owner asked.", "provenance": "s3-t12"});
    assert_eq!(forget(&socket, &request), [(cafe.clone(), 1, 0)]);
    assert_eq!(ids(search(&socket, &q)), Vec::<String>::new());
    assert_eq!(pointed(&socket, &q), []);
    assert_eq!(taken_refs(&socket, &json!({})), std::slice::from_ref(&bees));
    assert_eq!(
        refused(post(
            &socket,
            "seedbed_done",
            &json!({"items": [{"ref": bees}, {"ref": cafe}]})
        )),
        (404, c7::ErrorCode::NotFound)
    );
    assert_eq!(
        forget(&socket, &request),
        [(cafe.clone(), 0, 0)],
        "a forgotten record gets no second live mark"
    );

    let undo = json!({"refs": [cafe], "provenance": "s3-t14-n88", "undo": true});
    assert_eq!(forget(&socket, &undo), [(cafe.clone(), 1, 0)]);
    assert_eq!(forget(&socket, &undo), [(cafe.clone(), 0, 0)]);
    assert_eq!(ids(search(&socket, &q)), std::slice::from_ref(&cafe));
    assert_eq!(pointed(&socket, &q)[0].fact_id, cafe);
    assert_eq!(taken_refs(&socket, &json!({})), [bees, cafe.clone()]);
    let answer = done(&socket, &json!({"items": [{"ref": cafe}]}));
    assert_eq!(
        answer.items[0].outcomes[0].status,
        c7::ProposalStatus::Accepted
    );
}

#[test]
fn forget_queues_current_facts_of_the_ref_and_of_the_records_proposals_and_undo_unqueues_the_untaken()
 {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let from = |entity: &str, fact: &str, source: &str| {
        let mut c = owner_fact(entity, fact);
        c["source_ref"] = json!(source);
        c
    };
    let (choir_record, choir) = seeded_fact(
        &socket,
        &from("Ada Example", "Ada sings in a choir.", "c8:41"),
    );
    let (_, bees) = seeded_fact(&socket, &from("Bram Sample", "Bram keeps bees.", "c8:41"));
    seeded_fact(&socket, &from("Cleo Sample", "Cleo plays chess.", "c8:50"));
    Store::open(&scratch.store())
        .unwrap()
        .supersede(&FactId(bees), now())
        .unwrap();
    let clinic_record = write_seed(&scratch, &lifted(), None, now());
    let mut placed = lifted();
    placed["entity"] = json!("Sample Clinic");
    placed["kind"] = json!("place");
    let answer = done(
        &socket,
        &json!({"items": [{"ref": clinic_record, "candidates": [placed]}]}),
    );
    let clinic = answer.items[0].outcomes[0].fact_id.clone().unwrap();
    let reconciling = reconcile_queue(&scratch);
    assert_ne!(reconciling.len(), 0);

    let before = now();
    let request = json!({
        "refs": ["c8:41", clinic_record, choir_record, "c8:session:3"],
        "reason": "The owner asked to forget the week.",
        "provenance": "s3-t12",
    });
    assert_eq!(
        forget(&socket, &request),
        [
            ("c8:41".to_owned(), 0, 1),
            (clinic_record.clone(), 1, 1),
            (choir_record.clone(), 1, 1),
            ("c8:session:3".to_owned(), 0, 0),
        ]
    );
    let after = now();
    let queue = forget_queue(&scratch);
    let queued: Vec<_> = queue
        .iter()
        .map(|q| (q.fact.0.as_str(), q.r#ref.as_str()))
        .collect();
    assert_eq!(
        queued,
        [
            (choir.as_str(), "c8:41"),
            (clinic.as_str(), clinic_record.as_str()),
            (choir.as_str(), choir_record.as_str()),
        ]
    );
    for q in &queue {
        assert!((before..=after).contains(&q.marked_at), "{q:?}");
        assert_eq!(q.queued_at, q.marked_at);
        assert_eq!(q.reason, "The owner asked to forget the week.");
        assert_eq!(q.taken_at, None);
    }
    assert_eq!(
        rows(&scratch, "SELECT count(*) FROM seed_marks"),
        2,
        "c8 refs mark nothing in the garden"
    );
    assert_eq!(
        forget(&socket, &request).iter().map(|r| r.2).sum::<u64>(),
        0,
        "a waiting row is not queued twice"
    );
    assert_eq!(reconcile_queue(&scratch), reconciling);

    night_takes(&scratch, &choir_record);
    let undo = json!({
        "refs": ["c8:41", clinic_record, choir_record],
        "provenance": "s3-t14-n88",
        "undo": true,
    });
    assert_eq!(
        forget(&socket, &undo),
        [
            ("c8:41".to_owned(), 0, 1),
            (clinic_record.clone(), 1, 1),
            (choir_record.clone(), 1, 0),
        ]
    );
    let left: Vec<_> = forget_queue(&scratch)
        .into_iter()
        .map(|q| (q.fact.0, q.r#ref))
        .collect();
    assert_eq!(left, [(choir, choir_record)], "the night took it");
    assert_eq!(reconcile_queue(&scratch), reconciling);
}

#[test]
fn marks_and_lifts_keep_time_reason_and_provenance_and_are_never_deleted() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let record = write_seed(
        &scratch,
        &owner_fact("Bram Sample", "Bram keeps bees."),
        None,
        now(),
    );
    let t0 = now();
    forget(
        &socket,
        &json!({"refs": [record], "reason": "Not for the garden.", "provenance": "s3-t12"}),
    );
    let t1 = now();
    forget(
        &socket,
        &json!({"refs": [record], "provenance": "s3-t14-n88", "undo": true}),
    );
    let t2 = now();
    forget(
        &socket,
        &json!({"refs": [record], "reason": "Forget it after all.", "provenance": "s4-t2-n90"}),
    );
    let t3 = now();

    let marks = seed_marks(&scratch, &record);
    let [first, second] = marks.as_slice() else {
        panic!("{marks:?}");
    };
    assert!((t0..=t1).contains(&first.at));
    assert_eq!(
        (first.reason.as_str(), first.provenance.as_str()),
        ("Not for the garden.", "s3-t12")
    );
    let (lifted_at, lifted_by) = first.lifted.clone().unwrap();
    assert!((t1..=t2).contains(&lifted_at));
    assert_eq!(lifted_by, "s3-t14-n88");
    assert!((t2..=t3).contains(&second.at));
    assert_eq!(
        (
            second.reason.as_str(),
            second.provenance.as_str(),
            &second.lifted
        ),
        ("Forget it after all.", "s4-t2-n90", &None)
    );
    assert_eq!(taken_refs(&socket, &json!({})), Vec::<String>::new());
}

#[test]
fn forget_refuses_archive_and_malformed_refs_and_unknown_records_and_changes_nothing() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let mut said = owner_fact("Bram Sample", "Bram keeps bees.");
    said["source_ref"] = json!("c8:41");
    let (record, _) = seeded_fact(&socket, &said);
    let waiting = write_seed(
        &scratch,
        &owner_fact("Cleo Sample", "Cleo plays chess."),
        None,
        now(),
    );
    let mark =
        |refs: Value| json!({"refs": refs, "reason": "The owner asked.", "provenance": "s3-t12"});

    assert_eq!(
        refused(post(
            &socket,
            "forget",
            &mark(json!(["c8:41", record, "seed:999"]))
        )),
        (404, c7::ErrorCode::NotFound)
    );
    let undo = json!({"refs": ["seed:999"], "provenance": "s3-t12", "undo": true});
    assert_eq!(
        refused(post(&socket, "forget", &undo)),
        (404, c7::ErrorCode::NotFound)
    );
    let invalid = (400, c7::ErrorCode::InvalidRequest);
    for refs in [
        json!(["c8:41", "archive:41"]),
        json!([]),
        json!(["c8:01"]),
        json!(["c8:session:x"]),
        json!(["msg:7"]),
        json!(["seed:0"]),
    ] {
        assert_eq!(
            refused(post(&socket, "forget", &mark(refs.clone()))),
            invalid,
            "{refs}"
        );
    }
    for body in [
        json!({"refs": [waiting], "provenance": "s3-t12"}),
        json!({"refs": [waiting], "reason": "", "provenance": "s3-t12"}),
        json!({"refs": [waiting], "reason": "The owner asked.", "provenance": "turn 12"}),
        json!({"refs": [waiting], "reason": "The owner asked.", "provenance": "s3-t12-n"}),
        json!({"refs": [waiting], "reason": "The owner asked."}),
    ] {
        assert_eq!(refused(post(&socket, "forget", &body)), invalid, "{body}");
    }
    assert_eq!(forget_queue(&scratch), []);
    assert_eq!(rows(&scratch, "SELECT count(*) FROM seed_marks"), 0);
    assert_eq!(taken_refs(&socket, &json!({})), [waiting]);
}
