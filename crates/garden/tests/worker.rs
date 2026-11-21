//! The daemon's worker through `ulei_garden::worker` on an injected clock, and the wake a
//! request sends it through `ulei_garden::serve::Server`. All people, places and texts are
//! fictional.

#![allow(missing_docs, clippy::unwrap_used)]

mod scripted;

use std::cell::Cell;
use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::json;
use ulei_garden::api::ApiConfig;
use ulei_garden::edge::EdgeConfig;
use ulei_garden::embed::pipe::{CALL_DEADLINE, Helper, StartError};
use ulei_garden::embed::supervisor::Supervisor;
use ulei_garden::embed::{Embedder, ONNX_FILE, TOKENIZER_FILE};
use ulei_garden::miss::{Inbox, ProducerKey};
use ulei_garden::model::{
    Attrs, Entity, EntityId, EntityKind, Fact, FactId, Provenance, Sensitivity, SourceKind,
    TtlKind, TtlTable,
};
use ulei_garden::rerank;
use ulei_garden::second::Provider;
use ulei_garden::serve::{Server, Settings, TIMEOUT, open};
use ulei_garden::store::{Store, starting_domains};
use ulei_garden::wire::c7;
use ulei_garden::worker::{Clock, Passes, QUIET, Schedule, SystemClock, Wake, Worker};

const EMBED: &str = env!("CARGO_BIN_EXE_ulei-garden-embed");
const NOW: i64 = 1_800_000_000;
const SCHEDULE: Schedule = Schedule {
    expire_interval: 60,
    recheck_interval: 3_600,
    recheck_batch: 8,
    maintain_interval: 1_000,
};

/// A clock the test sets.
#[derive(Clone, Default)]
struct Fake(Rc<Cell<i64>>);

impl Fake {
    fn at(t: i64) -> Self {
        let clock = Self::default();
        clock.0.set(t);
        clock
    }
}

impl Clock for Fake {
    fn now(&self) -> i64 {
        self.0.get()
    }
}

/// A fresh directory holding a store, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ulei-garden-worker-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self(dir)
    }

    fn store(&self) -> PathBuf {
        self.0.join("garden.sqlite3")
    }

    /// A synthetic producer key, written owner-only on first use.
    fn key(&self) -> ProducerKey {
        let path = self.0.join("producer.key");
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
        ProducerKey::read(&path, rustix::process::geteuid().as_raw()).unwrap()
    }

    /// An inbox nobody serves; these tests queue no miss.
    fn inbox(&self) -> Inbox {
        Inbox::new(self.0.join("inbox.sock"), self.key())
    }

    /// The store with the café entity.
    fn open(&self) -> Store {
        let mut store = Store::open(&self.store()).unwrap();
        store
            .upsert_entity(&Entity {
                id: EntityId("pl-cafe".into()),
                kind: EntityKind::Place,
                name: "Cafe Lumen".into(),
                aliases: Vec::new(),
                domain: None,
                attrs: Attrs::None,
            })
            .unwrap();
        store
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A fact about the café recorded at `NOW` that expires at `expires_at`.
fn fact(id: &str, text: &str, expires_at: Option<i64>) -> Fact {
    fact_at(id, text, NOW, expires_at)
}

/// A fact about the café observed and recorded at `at` that expires at `expires_at`.
fn fact_at(id: &str, text: &str, at: i64, expires_at: Option<i64>) -> Fact {
    Fact {
        id: FactId(id.into()),
        entity: EntityId("pl-cafe".into()),
        text: text.into(),
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
            source_ref: format!("https://example.invalid/{id}"),
            observed_at: at,
            quote: None,
            proof_count: 1,
        },
        expires_at,
        sensitivity: Sensitivity::Public,
        confidence: 1.0,
    }
}

fn current(path: &Path, id: &str) -> bool {
    Store::open(path)
        .unwrap()
        .fact(&FactId(id.into()))
        .unwrap()
        .unwrap()
        .superseded_at
        .is_none()
}

fn passes(expiry: u64, maintenance: u64, failed: u64) -> Passes {
    Passes {
        expiry,
        maintenance,
        failed,
    }
}

#[test]
fn expiry_pass_runs_at_the_nearest_expiry_and_not_before() {
    let scratch = Scratch::new();
    let mut store = scratch.open();
    store
        .insert_facts(&[
            fact("f-early", "Cafe Lumen bakes rye today.", Some(NOW + 100)),
            fact("f-next", "Cafe Lumen pours cider today.", Some(NOW + 130)),
            fact(
                "f-late",
                "Cafe Lumen hosts a quiz this week.",
                Some(NOW + 500),
            ),
            fact("f-kept", "Cafe Lumen has a green door.", None),
        ])
        .unwrap();
    let clock = Fake::at(NOW);
    let mut worker = Worker::new(
        store,
        SCHEDULE,
        clock.clone(),
        scratch.inbox(),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap();

    assert_eq!(worker.tick(), NOW + 100);
    clock.0.set(NOW + 99);
    assert_eq!(worker.tick(), NOW + 100);
    assert_eq!(worker.passes(), passes(0, 0, 0));
    assert!(current(&scratch.store(), "f-early"));

    clock.0.set(NOW + 100);
    // f-next expires at +130, but the next pass waits for the interval after this one.
    assert_eq!(worker.tick(), NOW + 160);
    assert_eq!(worker.passes(), passes(1, 0, 0));
    assert!(!current(&scratch.store(), "f-early"));
    assert!(current(&scratch.store(), "f-next"));

    clock.0.set(NOW + 159);
    assert_eq!(worker.tick(), NOW + 160);
    assert_eq!(worker.passes().expiry, 1);
    clock.0.set(NOW + 160);
    assert_eq!(worker.tick(), NOW + 500);
    assert_eq!(worker.passes().expiry, 2);
    assert!(!current(&scratch.store(), "f-next"));
    assert!(current(&scratch.store(), "f-late"));
    assert!(current(&scratch.store(), "f-kept"));
}

#[test]
fn without_expiring_facts_no_expiry_pass_is_due_and_the_only_deadline_is_maintenance() {
    let scratch = Scratch::new();
    let mut store = scratch.open();
    store
        .insert_fact(&fact("f-kept", "Cafe Lumen has a green door.", None))
        .unwrap();
    let clock = Fake::at(NOW);
    let mut worker = Worker::new(
        store,
        SCHEDULE,
        clock.clone(),
        scratch.inbox(),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap();
    assert_eq!(worker.tick(), NOW + 1_000);
    clock.0.set(NOW + 999);
    assert_eq!(worker.tick(), NOW + 1_000);
    assert_eq!(worker.passes(), passes(0, 0, 0));
}

#[test]
fn maintenance_pass_runs_on_its_interval_and_the_index_still_answers() {
    let scratch = Scratch::new();
    let mut store = scratch.open();
    store
        .insert_fact(&fact("f-kept", "Cafe Lumen has a green door.", None))
        .unwrap();
    let clock = Fake::at(NOW);
    let mut worker = Worker::new(
        store,
        SCHEDULE,
        clock.clone(),
        scratch.inbox(),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap();
    clock.0.set(NOW + 999);
    assert_eq!(worker.tick(), NOW + 1_000);
    assert_eq!(worker.passes().maintenance, 0);
    clock.0.set(NOW + 1_000);
    assert_eq!(worker.tick(), NOW + 2_000);
    clock.0.set(NOW + 2_000);
    assert_eq!(worker.tick(), NOW + 3_000);
    assert_eq!(worker.passes(), passes(0, 2, 0));
    let hits = Store::open(&scratch.store())
        .unwrap()
        .lexical("green door", 5, NOW, NOW, false)
        .unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn restarts_more_often_than_the_interval_keep_maintenance_due_from_the_last_pass() {
    let scratch = Scratch::new();
    let clock = Fake::at(NOW);
    let mut first = Worker::new(
        scratch.open(),
        SCHEDULE,
        clock.clone(),
        scratch.inbox(),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap();
    clock.0.set(NOW + 1_000);
    assert_eq!(first.tick(), NOW + 2_000);
    assert_eq!(first.passes().maintenance, 1);
    drop(first);

    for restart in [NOW + 1_400, NOW + 1_800, NOW + 1_999] {
        clock.0.set(restart);
        let mut again = Worker::new(
            scratch.open(),
            SCHEDULE,
            clock.clone(),
            scratch.inbox(),
            Provider::None.source(),
            EdgeConfig::default(),
        )
        .unwrap();
        assert_eq!(again.tick(), NOW + 2_000, "restart at {restart}");
        assert_eq!(again.passes().maintenance, 0);
    }
    let mut last = Worker::new(
        scratch.open(),
        SCHEDULE,
        clock.clone(),
        scratch.inbox(),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap();
    clock.0.set(NOW + 2_000);
    assert_eq!(last.tick(), NOW + 3_000);
    assert_eq!(last.passes().maintenance, 1);
    drop(last);

    clock.0.set(NOW + 9_000);
    let mut late = Worker::new(
        scratch.open(),
        SCHEDULE,
        clock.clone(),
        scratch.inbox(),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap();
    assert_eq!(
        late.tick(),
        NOW + 10_000,
        "an overdue pass runs at the first tick"
    );
    assert_eq!(late.passes().maintenance, 1);
}

#[test]
fn a_failed_pass_is_counted_and_due_again_after_its_interval() {
    let scratch = Scratch::new();
    let mut store = scratch.open();
    store
        .insert_fact(&fact(
            "f-early",
            "Cafe Lumen bakes rye today.",
            Some(NOW + 10),
        ))
        .unwrap();
    let clock = Fake::at(NOW + 10);
    let mut worker = Worker::new(
        store,
        SCHEDULE,
        clock.clone(),
        scratch.inbox(),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap();
    let other = Store::open(&scratch.store()).unwrap();
    other
        .conn()
        .execute_batch(
            "CREATE TRIGGER refuse_close BEFORE UPDATE ON facts
             BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
        )
        .unwrap();

    assert_eq!(worker.tick(), NOW + 70);
    assert_eq!(worker.passes(), passes(0, 0, 1));
    assert!(current(&scratch.store(), "f-early"));

    other
        .conn()
        .execute_batch("DROP TRIGGER refuse_close")
        .unwrap();
    clock.0.set(NOW + 69);
    worker.tick();
    assert_eq!(worker.passes(), passes(0, 0, 1));
    clock.0.set(NOW + 70);
    worker.tick();
    assert_eq!(worker.passes(), passes(1, 0, 1));
    assert!(!current(&scratch.store(), "f-early"));
}

#[test]
fn with_provider_none_a_waiting_recheck_sets_no_deadline() {
    let scratch = Scratch::new();
    let mut store = scratch.open();
    store
        .insert_fact(&fact(
            "f-early",
            "Cafe Lumen bakes rye today.",
            Some(NOW + 10),
        ))
        .unwrap();
    let schedule = Schedule {
        recheck_interval: 100,
        ..SCHEDULE
    };
    let mut worker = Worker::new(
        store,
        schedule,
        Fake::at(NOW + 10),
        scratch.inbox(),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap();
    assert_eq!(
        worker.tick(),
        NOW + 10 + SCHEDULE.maintain_interval,
        "maintenance is the only deadline"
    );
    let waiting = Store::open(&scratch.store()).unwrap().rechecks(10).unwrap();
    assert_eq!(waiting.len(), 1);
}

/// The `200` body of the answer of `server` to `method` with `body` on a socketpair.
fn call<C: Clock>(server: &mut Server<C>, method: &str, body: &serde_json::Value) -> Vec<u8> {
    let body = body.to_string();
    let (mut client, end) = UnixStream::pair().unwrap();
    write!(
        client,
        "POST /v1/{method} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    server.serve(end);
    let mut raw = Vec::new();
    client.read_to_end(&mut raw).unwrap();
    let start = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(raw.starts_with(b"HTTP/1.1 200 "));
    raw.split_off(start)
}

/// The answer of `server` to a `propose` of `candidate` on a socketpair.
fn propose<C: Clock>(server: &mut Server<C>, candidate: &serde_json::Value) -> c7::ProposeResponse {
    serde_json::from_slice(&call(server, "propose", candidate)).unwrap()
}

#[test]
fn a_fact_a_request_writes_wakes_the_worker_and_an_earlier_expiry_moves_its_deadline() {
    let scratch = Scratch::new();
    let garden = open(
        &Settings {
            store: scratch.store(),
            socket: scratch.0.join("garden.sock"),
            producer_key: scratch.key(),
            inbox_socket: scratch.0.join("inbox.sock"),
            domains: starting_domains().unwrap(),
            ttl: TtlTable::default(),
            api: ApiConfig::default(),
            schedule: SCHEDULE,
            second_level: Provider::None,
            reranker: rerank::Choice::None,
            embedder: None,
        },
        None,
    )
    .unwrap();
    let wake = Arc::new(Wake::default());
    let uid = rustix::process::geteuid().as_raw();
    let mut server = Server::new(garden, uid, TIMEOUT, Arc::clone(&wake), SystemClock);
    let now = SystemClock.now();
    let far = now + 2 * 86_400;
    let mut store = scratch.open();
    store
        .insert_fact(&fact_at(
            "f-far",
            "Cafe Lumen closes for winter.",
            now,
            Some(far),
        ))
        .unwrap();
    let clock = Fake::at(now);
    let mut worker = Worker::new(
        store,
        Schedule::default(),
        clock,
        scratch.inbox(),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap();
    assert_eq!(worker.tick(), far);
    assert!(!wake.wait(Duration::ZERO));

    let answer = propose(
        &mut server,
        &json!({
            "fact": "Fogtown expects drizzle.",
            "entity": "Fogtown",
            "kind": "place",
            "source_ref": "https://example.invalid/forecast",
            "source_kind": "internet",
            "valid_from": now,
            "observed_at": now,
            "ttl_kind": "weather",
        }),
    );
    assert_eq!(answer.status, c7::ProposalStatus::Accepted);
    assert!(wake.wait(Duration::ZERO), "the request woke the worker");
    let written = Store::open(&scratch.store())
        .unwrap()
        .fact(&FactId(answer.fact_id.unwrap()))
        .unwrap()
        .unwrap();
    assert_eq!(written.expires_at, Some(now + 3 * 3_600));
    assert_eq!(worker.tick(), now + 3 * 3_600);
    assert_eq!(worker.passes(), passes(0, 0, 0));
}

#[test]
fn a_request_reads_its_time_from_the_daemon_clock() {
    let scratch = Scratch::new();
    let garden = open(
        &Settings {
            store: scratch.store(),
            socket: scratch.0.join("garden.sock"),
            producer_key: scratch.key(),
            inbox_socket: scratch.0.join("inbox.sock"),
            domains: starting_domains().unwrap(),
            ttl: TtlTable::default(),
            api: ApiConfig::default(),
            schedule: SCHEDULE,
            second_level: Provider::None,
            reranker: rerank::Choice::None,
            embedder: None,
        },
        None,
    )
    .unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let clock = Fake::at(NOW);
    let mut server = Server::new(garden, uid, TIMEOUT, Arc::new(Wake::default()), clock);
    let answer = propose(
        &mut server,
        &json!({
            "fact": "Fogtown expects drizzle.",
            "entity": "Fogtown",
            "kind": "place",
            "source_ref": "https://example.invalid/forecast",
            "source_kind": "internet",
            "valid_from": NOW,
            "observed_at": NOW,
            "ttl_kind": "weather",
        }),
    );
    assert_eq!(answer.status, c7::ProposalStatus::Accepted);
    let written = Store::open(&scratch.store())
        .unwrap()
        .fact(&FactId(answer.fact_id.unwrap()))
        .unwrap()
        .unwrap();
    assert_eq!(written.recorded_at, NOW);
}

#[test]
fn a_wake_reaches_a_waiting_worker_and_one_sent_before_the_wait_is_kept_once() {
    let wake = Wake::default();
    wake.wake();
    assert!(wake.wait(Duration::from_secs(3_600)));
    assert!(!wake.wait(Duration::ZERO));

    let wake = Arc::new(Wake::default());
    let waiter = {
        let wake = Arc::clone(&wake);
        thread::spawn(move || wake.wait(Duration::MAX))
    };
    wake.wake();
    assert!(waiter.join().unwrap());
}

/// The supervisor of the fake helper `ulei-garden-embed --embedder fake` over synthetic
/// artifacts in `dir`, the tokenizer file holding `tokenizer`.
fn fake_helper(dir: &Path, tokenizer: &str) -> Arc<Supervisor> {
    let model = dir.join("model");
    fs::create_dir_all(model.join(ONNX_FILE).parent().unwrap()).unwrap();
    fs::write(model.join(ONNX_FILE), "synthetic model").unwrap();
    fs::write(model.join(TOKENIZER_FILE), tokenizer).unwrap();
    let start = move || {
        let mut command = Command::new(EMBED);
        command
            .args(["--embedder", "fake", "--model-dir"])
            .arg(&model);
        Helper::start(command, Duration::from_secs(30), CALL_DEADLINE)
    };
    Arc::new(Supervisor::start(start, Arc::new(Wake::default())).unwrap())
}

/// The supervisor whose start `n`, from 0, runs `start(n)`.
fn supervised(
    start: impl Fn(usize) -> Result<Helper, StartError> + Send + Sync + 'static,
) -> (Arc<Supervisor>, Arc<AtomicUsize>) {
    let starts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&starts);
    let supervisor = Supervisor::start(
        move || start(counted.fetch_add(1, Ordering::SeqCst)),
        Arc::new(Wake::default()),
    )
    .unwrap();
    (Arc::new(supervisor), starts)
}

/// The store of `scratch` created owner-only, with the café entity.
fn private_store(scratch: &Scratch) -> Store {
    drop(open(&settings(scratch), None).unwrap());
    scratch.open()
}

/// The settings of the store of `scratch` with [`SCHEDULE`].
fn settings(scratch: &Scratch) -> Settings {
    Settings {
        store: scratch.store(),
        socket: scratch.0.join("garden.sock"),
        producer_key: scratch.key(),
        inbox_socket: scratch.0.join("inbox.sock"),
        domains: starting_domains().unwrap(),
        ttl: TtlTable::default(),
        api: ApiConfig::default(),
        schedule: SCHEDULE,
        second_level: Provider::None,
        reranker: rerank::Choice::None,
        embedder: None,
    }
}

/// The server of the store of `scratch` over `helper` on `clock`.
fn server_over(scratch: &Scratch, helper: &Arc<Supervisor>, clock: &Fake) -> Server<Fake> {
    let garden = open(&settings(scratch), Some(Arc::clone(helper))).unwrap();
    let uid = rustix::process::geteuid().as_raw();
    Server::new(
        garden,
        uid,
        TIMEOUT,
        Arc::new(Wake::default()),
        clock.clone(),
    )
}

/// The worker of `store` with `helper` on `clock`.
fn worker_over(
    scratch: &Scratch,
    store: Store,
    helper: &Arc<Supervisor>,
    clock: &Fake,
) -> Worker<Fake> {
    Worker::new(
        store,
        SCHEDULE,
        clock.clone(),
        scratch.inbox(),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap()
    .with_helper(Arc::clone(helper))
}

fn health<C: Clock>(server: &mut Server<C>) -> c7::Health {
    serde_json::from_slice(&call(server, "health", &json!({}))).unwrap()
}

fn search<C: Clock>(server: &mut Server<C>, q: &str) -> c7::SearchResponse {
    serde_json::from_slice(&call(server, "search", &json!({ "q": q }))).unwrap()
}

fn missing(scratch: &Scratch, helper: &Supervisor) -> usize {
    let model = helper.helper().unwrap().model_id().to_owned();
    Store::open(&scratch.store())
        .unwrap()
        .missing_vectors(&model)
        .unwrap()
        .len()
}

#[test]
fn the_worker_embeds_a_new_fact_after_its_wake_and_the_dense_side_finds_what_the_lexical_misses() {
    let scratch = Scratch::new();
    let helper = fake_helper(&scratch.0, "{}");
    let clock = Fake::at(NOW);
    let garden = open(&settings(&scratch), Some(Arc::clone(&helper))).unwrap();
    let wake = Arc::new(Wake::default());
    let uid = rustix::process::geteuid().as_raw();
    let mut server = Server::new(garden, uid, TIMEOUT, Arc::clone(&wake), clock.clone());
    assert_eq!(health(&mut server).dense, c7::DenseState::Ready);
    let mut worker = worker_over(&scratch, scratch.open(), &helper, &clock);

    for (entity, text) in [
        ("Bram Sample", "Bram Sample sees the dentist on Fridays."),
        ("Cleo Placeholder", "Cleo Placeholder grows tomatoes."),
    ] {
        let answer = propose(
            &mut server,
            &json!({
                "fact": text,
                "entity": entity,
                "kind": "person",
                "domain": "doctors",
                "source_ref": "msg:7",
                "source_kind": "owner",
                "valid_from": 1_700_000_000,
            }),
        );
        assert_eq!(answer.status, c7::ProposalStatus::Accepted);
    }
    assert!(wake.wait(Duration::ZERO), "the new facts woke the worker");
    let lexical = search(&mut server, "dentists");
    assert!(
        lexical.facts.is_empty(),
        "no word of the facts starts with the query and no vector exists yet"
    );
    assert_eq!(lexical.lexical_only, None);
    assert_eq!(missing(&scratch, &helper), 2);

    clock.0.set(NOW + QUIET);
    assert_eq!(worker.tick(), NOW + QUIET);
    assert_eq!(worker.tick(), NOW + QUIET);
    assert_eq!(missing(&scratch, &helper), 0);
    let dense = search(&mut server, "dentists");
    assert_eq!(
        dense.facts[0].fact,
        "Bram Sample sees the dentist on Fridays."
    );
}

#[test]
fn a_changed_artifact_changes_the_model_id_and_the_worker_embeds_every_fact_again_under_it() {
    let scratch = Scratch::new();
    let mut store = scratch.open();
    store
        .insert_fact(&fact("f-kept", "Cafe Lumen has a green door.", None))
        .unwrap();
    let clock = Fake::at(NOW);
    let first = fake_helper(&scratch.0, "{\"version\": 1}");
    worker_over(&scratch, store, &first, &clock).tick();
    let first = first.helper().unwrap();
    let check = Store::open(&scratch.store()).unwrap();
    assert_eq!(
        check
            .vectors(first.model_id(), NOW, NOW, false)
            .unwrap()
            .len(),
        1
    );

    let supervisor = fake_helper(&scratch.0, "{\"version\": 2}");
    let second = supervisor.helper().unwrap();
    assert_eq!(
        second.identity().model_sha256,
        first.identity().model_sha256
    );
    assert_ne!(second.model_id(), first.model_id());
    assert_eq!(
        check.missing_vectors(second.model_id()).unwrap().len(),
        1,
        "a vector of the old artifact is stale"
    );
    worker_over(&scratch, scratch.open(), &supervisor, &clock).tick();
    assert_eq!(check.missing_vectors(second.model_id()).unwrap().len(), 0);
    assert_eq!(
        check
            .vectors(second.model_id(), NOW, NOW, false)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn without_a_query_for_two_seconds_the_worker_embeds_one_passage_per_deadline_and_a_query_goes_first()
 {
    let scratch = Scratch::new();
    let mut store = private_store(&scratch);
    for (id, text) in [
        ("f-door", "Cafe Lumen has a green door."),
        ("f-hours", "Cafe Lumen opens at eight."),
        ("f-cake", "Cafe Lumen bakes plum cake."),
    ] {
        store.insert_fact(&fact(id, text, None)).unwrap();
    }
    let script = scripted::script(&scratch.0, "helper", scripted::ANSWER);
    let (helper, _) = supervised(move |_| scripted::start(&script, CALL_DEADLINE));
    let clock = Fake::at(NOW);
    let mut server = server_over(&scratch, &helper, &clock);
    let mut worker = worker_over(&scratch, store, &helper, &clock);

    assert_eq!(worker.tick(), NOW, "the next passage is due at once");
    assert_eq!(missing(&scratch, &helper), 2);
    assert_eq!(search(&mut server, "plum cake").lexical_only, None);
    assert_eq!(
        worker.tick(),
        NOW + QUIET,
        "the passages wait for the query"
    );
    clock.0.set(NOW + QUIET - 1);
    assert_eq!(worker.tick(), NOW + QUIET);
    assert_eq!(missing(&scratch, &helper), 2);
    clock.0.set(NOW + QUIET);
    assert_eq!(worker.tick(), NOW + QUIET);
    assert_eq!(missing(&scratch, &helper), 1);
    assert_eq!(worker.tick(), NOW + QUIET);
    assert_eq!(missing(&scratch, &helper), 0);
    assert_eq!(
        worker.tick(),
        NOW + SCHEDULE.maintain_interval,
        "nothing is left to embed"
    );
    let kinds: Vec<String> = scripted::log(&scratch.0, "helper")
        .iter()
        .map(|line| {
            let request: serde_json::Value = serde_json::from_str(line).unwrap();
            request["kind"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(kinds, ["passage", "query", "passage", "passage"]);
}

#[test]
fn a_helper_that_goes_down_is_restarted_after_1_2_4_up_to_60_seconds_and_answers_are_lexical_only_until_then()
 {
    let scratch = Scratch::new();
    let mut store = private_store(&scratch);
    store
        .insert_fact(&fact("f-door", "Cafe Lumen has a green door.", None))
        .unwrap();
    let exits = scripted::script(&scratch.0, "exits", &scripted::exits_after(0));
    let answers = scripted::script(&scratch.0, "answers", scripted::ANSWER);
    let absent = scratch.0.join("absent");
    let allowed = Arc::new(AtomicBool::new(false));
    let allow = Arc::clone(&allowed);
    let (helper, starts) = supervised(move |n| {
        let path = match n {
            0 => &exits,
            _ if allow.load(Ordering::SeqCst) => &answers,
            _ => &absent,
        };
        scripted::start(path, CALL_DEADLINE)
    });
    let clock = Fake::at(NOW);
    let mut server = server_over(&scratch, &helper, &clock);
    let mut worker = worker_over(&scratch, store, &helper, &clock);
    assert_eq!(health(&mut server).dense, c7::DenseState::Ready);

    assert_eq!(worker.tick(), NOW + 1, "its passage found the helper gone");
    let down = health(&mut server);
    assert_eq!(down.dense, c7::DenseState::Down);
    assert_eq!(
        down.dense_error.as_deref(),
        Some("helper pipe: the helper closed it")
    );
    let found = search(&mut server, "green door");
    assert_eq!(found.facts.len(), 1);
    assert_eq!(found.lexical_only, Some(true));
    let pointers: c7::PointersResponse =
        serde_json::from_slice(&call(&mut server, "pointers", &json!({"q": "green door"})))
            .unwrap();
    assert_eq!(pointers.lexical_only, Some(true));

    let mut at = NOW + 1;
    for pause in [2, 4, 8, 16, 32, 60, 60] {
        let tried = starts.load(Ordering::SeqCst);
        clock.0.set(at - 1);
        assert_eq!(worker.tick(), at, "no restart before its pause is over");
        assert_eq!(starts.load(Ordering::SeqCst), tried);
        clock.0.set(at);
        assert_eq!(worker.tick(), at + pause);
        assert_eq!(starts.load(Ordering::SeqCst), tried + 1);
        assert_eq!(health(&mut server).dense, c7::DenseState::Down);
        at += pause;
    }
    assert!(
        health(&mut server)
            .dense_error
            .unwrap()
            .starts_with("restart: cannot start: ")
    );

    allowed.store(true, Ordering::SeqCst);
    clock.0.set(at);
    assert_eq!(worker.tick(), at, "back, and its passage is embedded");
    assert_eq!(health(&mut server).dense, c7::DenseState::Ready);
    assert_eq!(health(&mut server).dense_error, None);
    assert_eq!(missing(&scratch, &helper), 0);
    assert_eq!(search(&mut server, "green door").lexical_only, None);
}

#[test]
fn a_restart_under_another_identity_is_ready_and_every_fact_is_embedded_again_under_it() {
    let scratch = Scratch::new();
    let mut store = private_store(&scratch);
    store
        .insert_fact(&fact("f-door", "Cafe Lumen has a green door.", None))
        .unwrap();
    store
        .insert_fact(&fact("f-cake", "Cafe Lumen bakes plum cake.", None))
        .unwrap();
    let first = scripted::script(&scratch.0, "first", &scripted::exits_after(1));
    let second = scripted::script(&scratch.0, "second", scripted::ANSWER);
    let (helper, _) =
        supervised(move |n| scripted::start(if n == 0 { &first } else { &second }, CALL_DEADLINE));
    let old = helper.helper().unwrap().model_id().to_owned();
    let clock = Fake::at(NOW);
    let mut server = server_over(&scratch, &helper, &clock);
    let mut worker = worker_over(&scratch, store, &helper, &clock);
    assert_eq!(worker.tick(), NOW);

    scripted::set_identity(&scratch.0, 'b');
    assert_eq!(
        worker.tick(),
        NOW + 1,
        "the first helper exits on its second call"
    );
    clock.0.set(NOW + 1);
    assert_eq!(worker.tick(), NOW + 1);
    assert_eq!(health(&mut server).dense, c7::DenseState::Ready);
    let new = helper.helper().unwrap().model_id().to_owned();
    assert_ne!(new, old);
    assert_eq!(worker.tick(), NOW + 1);
    let check = Store::open(&scratch.store()).unwrap();
    assert_eq!(check.missing_vectors(&new).unwrap().len(), 0);
    assert_eq!(check.vectors(&new, NOW, NOW, false).unwrap().len(), 2);
    assert_eq!(check.vectors(&old, NOW, NOW, false).unwrap().len(), 1);
}

/// The scripted helper of `scratch` that puts the texts about meat and vegetarians on each
/// other and every other text opposite them.
fn diet_helper(scratch: &Scratch) -> Arc<Supervisor> {
    let script = scripted::script(
        &scratch.0,
        "diet",
        &scripted::close_when(&["meat", "vegetarian"]),
    );
    supervised(move |_| scripted::start(&script, CALL_DEADLINE)).0
}

/// An owner's candidate about Ada without an attribute.
fn about_ada(text: &str) -> serde_json::Value {
    json!({
        "fact": text,
        "entity": "Ada Example",
        "kind": "person",
        "source_ref": "msg:ada",
        "source_kind": "owner",
    })
}

fn accepted<C: Clock>(server: &mut Server<C>, text: &str) -> String {
    let answer = propose(server, &about_ada(text));
    assert_eq!(answer.status, c7::ProposalStatus::Accepted, "{text}");
    answer.fact_id.unwrap()
}

fn digest<C: Clock>(server: &mut Server<C>) -> Vec<c7::DigestItem> {
    let answer: c7::DigestResponse =
        serde_json::from_slice(&call(server, "digest", &json!({}))).unwrap();
    answer.items
}

fn resolve<C: Clock>(server: &mut Server<C>, body: &serde_json::Value) -> c7::ProposeResponse {
    serde_json::from_slice(&call(server, "resolve", body)).unwrap()
}

/// Ticks `worker` until its deadline is no longer now; the ticks that were due at once.
fn drain(worker: &mut Worker<Fake>, clock: &Fake) -> usize {
    let mut due = 0;
    while worker.tick() == clock.now() {
        due += 1;
        assert!(due < 20, "the worker keeps asking for itself");
    }
    due
}

/// "Ada eats meat." at `NOW` and "Ada is vegetarian." at `NOW + 10`, both written, and the
/// worker over `helper` ticked until the contradiction is queued.
struct Contradiction {
    server: Server<Fake>,
    clock: Fake,
    meat: String,
    vegetarian: String,
    item: c7::DigestItem,
}

fn contradiction(scratch: &Scratch) -> Contradiction {
    let store = private_store(scratch);
    let helper = diet_helper(scratch);
    let clock = Fake::at(NOW);
    let mut server = server_over(scratch, &helper, &clock);
    let mut worker = worker_over(scratch, store, &helper, &clock);
    let meat = accepted(&mut server, "Ada eats meat.");
    clock.0.set(NOW + 10);
    let vegetarian = accepted(&mut server, "Ada is vegetarian.");
    drain(&mut worker, &clock);
    let mut items = digest(&mut server);
    assert_eq!(items.len(), 1);
    Contradiction {
        server,
        clock,
        meat,
        vegetarian,
        item: items.remove(0),
    }
}

fn texts(found: &c7::SearchResponse) -> Vec<&str> {
    found.facts.iter().map(|f| f.fact.as_str()).collect()
}

#[test]
fn a_new_fact_close_to_a_current_one_is_written_and_queued_as_a_contradiction_once_reconciled() {
    let scratch = Scratch::new();
    let store = private_store(&scratch);
    let helper = diet_helper(&scratch);
    let clock = Fake::at(NOW);
    let mut server = server_over(&scratch, &helper, &clock);
    let mut worker = worker_over(&scratch, store, &helper, &clock);
    let meat = accepted(&mut server, "Ada eats meat.");
    clock.0.set(NOW + 10);
    let vegetarian = accepted(&mut server, "Ada is vegetarian.");
    assert!(
        scripted::log(&scratch.0, "diet").is_empty(),
        "a propose embeds nothing"
    );
    assert_eq!(digest(&mut server).len(), 0);

    assert_eq!(worker.tick(), NOW + 10);
    assert_eq!(worker.tick(), NOW + 10, "both passages embedded");
    assert_eq!(missing(&scratch, &helper), 0);
    let found = search(&mut server, "Ada eats meat");
    assert!(texts(&found).contains(&"Ada eats meat."));
    assert!(texts(&found).contains(&"Ada is vegetarian."));
    assert_eq!(
        worker.tick(),
        NOW + 10 + QUIET,
        "the reconciliation waits for the query"
    );
    assert_eq!(digest(&mut server).len(), 0);

    clock.0.set(NOW + 10 + QUIET);
    assert_eq!(drain(&mut worker, &clock), 2, "each new fact once");
    let items = digest(&mut server);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].reason, c7::DigestReason::Contradiction);
    assert_eq!(items[0].candidate.fact, "Ada is vegetarian.");
    assert_eq!(items[0].conflicts_with.as_deref(), Some(meat.as_str()));
    assert_eq!(items[0].conflicting_fact.as_deref(), Some("Ada eats meat."));
    assert!(current(&scratch.store(), &meat));
    assert!(current(&scratch.store(), &vegetarian));

    clock.0.set(NOW + 100);
    assert_eq!(drain(&mut worker, &clock), 0);
    assert_eq!(
        digest(&mut server).len(),
        1,
        "a reconciled fact is not taken again"
    );
}

#[test]
fn reject_retracts_the_new_fact_in_favour_of_its_rival_and_as_of_before_answers_as_before() {
    let scratch = Scratch::new();
    let mut c = contradiction(&scratch);
    c.clock.0.set(NOW + 50);
    let answer = resolve(
        &mut c.server,
        &json!({"proposal_id": c.item.proposal_id, "action": "reject"}),
    );
    assert_eq!(answer.status, c7::ProposalStatus::Rejected);
    assert_eq!(answer.reason, Some(c7::ProposalReason::OwnerRejected));
    assert_eq!(digest(&mut c.server).len(), 0);

    let store = Store::open(&scratch.store()).unwrap();
    let retracted = store.fact(&FactId(c.vegetarian.clone())).unwrap().unwrap();
    assert_eq!(retracted.superseded_at, Some(NOW + 50));
    assert_eq!(retracted.superseded_by, Some(FactId(c.meat.clone())));
    assert!(current(&scratch.store(), &c.meat));
    let now = search(&mut c.server, "Ada vegetarian meat");
    assert_eq!(texts(&now), ["Ada eats meat."]);
    let before = serde_json::from_slice::<c7::SearchResponse>(&call(
        &mut c.server,
        "search",
        &json!({"q": "Ada vegetarian meat", "as_of": NOW + 20}),
    ))
    .unwrap();
    assert!(texts(&before).contains(&"Ada is vegetarian."));
    assert!(texts(&before).contains(&"Ada eats meat."));
}

#[test]
fn accept_retracts_the_rival_in_favour_of_the_new_fact() {
    let scratch = Scratch::new();
    let mut c = contradiction(&scratch);
    c.clock.0.set(NOW + 50);
    let answer = resolve(
        &mut c.server,
        &json!({"proposal_id": c.item.proposal_id, "action": "accept"}),
    );
    assert_eq!(answer.status, c7::ProposalStatus::Accepted);
    assert_eq!(answer.fact_id.as_deref(), Some(c.vegetarian.as_str()));
    assert_eq!(digest(&mut c.server).len(), 0);

    let store = Store::open(&scratch.store()).unwrap();
    let rival = store.fact(&FactId(c.meat.clone())).unwrap().unwrap();
    assert_eq!(rival.superseded_at, Some(NOW + 50));
    assert_eq!(rival.superseded_by, Some(FactId(c.vegetarian.clone())));
    assert!(current(&scratch.store(), &c.vegetarian));
    let now = search(&mut c.server, "Ada vegetarian meat");
    assert_eq!(texts(&now), ["Ada is vegetarian."]);
}

#[test]
fn replace_writes_the_replacement_and_retracts_the_new_fact_in_its_favour() {
    let scratch = Scratch::new();
    let mut c = contradiction(&scratch);
    c.clock.0.set(NOW + 50);
    let answer = resolve(
        &mut c.server,
        &json!({
            "proposal_id": c.item.proposal_id,
            "action": "replace",
            "replacement": about_ada("Ada eats fish but no meat."),
        }),
    );
    assert_eq!(answer.status, c7::ProposalStatus::Accepted);
    let replacement = answer.fact_id.unwrap();
    assert_eq!(digest(&mut c.server).len(), 0);

    let store = Store::open(&scratch.store()).unwrap();
    let retracted = store.fact(&FactId(c.vegetarian.clone())).unwrap().unwrap();
    assert_eq!(retracted.superseded_by, Some(FactId(replacement.clone())));
    assert!(current(&scratch.store(), &replacement));
    assert!(current(&scratch.store(), &c.meat));
}

#[test]
fn a_dissimilar_fact_of_the_same_entity_makes_no_item() {
    let scratch = Scratch::new();
    let store = private_store(&scratch);
    let helper = diet_helper(&scratch);
    let clock = Fake::at(NOW);
    let mut server = server_over(&scratch, &helper, &clock);
    let mut worker = worker_over(&scratch, store, &helper, &clock);
    accepted(&mut server, "Ada eats meat.");
    clock.0.set(NOW + 10);
    let chess = accepted(&mut server, "Ada plays chess.");
    assert_eq!(drain(&mut worker, &clock), 4);
    assert_eq!(digest(&mut server).len(), 0);
    assert!(current(&scratch.store(), &chess));
}

#[test]
fn a_rival_with_the_same_normalised_text_is_no_contradiction() {
    let scratch = Scratch::new();
    let store = private_store(&scratch);
    let helper = diet_helper(&scratch);
    let clock = Fake::at(NOW);
    let mut server = server_over(&scratch, &helper, &clock);
    let mut worker = worker_over(&scratch, store, &helper, &clock);
    accepted(&mut server, "Ada eats meat.");
    Store::open(&scratch.store())
        .unwrap()
        .insert_fact(&Fact {
            entity: EntityId("ada-example".into()),
            ..fact("f-same", "ADA  eats meat!", None)
        })
        .unwrap();
    assert_eq!(drain(&mut worker, &clock), 3);
    assert_eq!(digest(&mut server).len(), 0);
}

#[test]
fn without_a_helper_nothing_is_reconciled_and_the_item_comes_once_the_helper_is_there() {
    let scratch = Scratch::new();
    let store = private_store(&scratch);
    let clock = Fake::at(NOW);
    let garden = open(&settings(&scratch), None).unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let mut server = Server::new(
        garden,
        uid,
        TIMEOUT,
        Arc::new(Wake::default()),
        clock.clone(),
    );
    let mut lexical = Worker::new(
        store,
        SCHEDULE,
        clock.clone(),
        scratch.inbox(),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap();
    let meat = accepted(&mut server, "Ada eats meat.");
    clock.0.set(NOW + 10);
    accepted(&mut server, "Ada is vegetarian.");
    assert_eq!(lexical.tick(), NOW + SCHEDULE.maintain_interval);
    assert_eq!(digest(&mut server).len(), 0);

    let helper = diet_helper(&scratch);
    let mut dense = worker_over(&scratch, scratch.open(), &helper, &clock);
    assert_eq!(drain(&mut dense, &clock), 4);
    let items = digest(&mut server);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].conflicts_with.as_deref(), Some(meat.as_str()));
}
