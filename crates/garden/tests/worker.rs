//! The daemon's worker through `ulei_garden::worker` on an injected clock, and the wake a
//! request sends it through `ulei_garden::serve::Server`. All people, places and texts are
//! fictional.

#![allow(missing_docs, clippy::unwrap_used)]

mod scripted;
#[cfg(feature = "native")]
mod tiny_bert;

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
use ulei_garden::embed::check::Verdict;
use ulei_garden::embed::pipe::{CALL_DEADLINE, Helper, StartError};
use ulei_garden::embed::supervisor::{STEADY, Supervisor};
use ulei_garden::embed::{Embedder, ONNX_FILE, TOKENIZER_FILE};
use ulei_garden::miss::{Inbox, ProducerKey};
use ulei_garden::model::{
    Attrs, Entity, EntityId, EntityKind, Fact, FactId, Provenance, Sensitivity, SourceKind,
    TtlKind, TtlTable,
};
use ulei_garden::rerank::{self, NoReranker};
use ulei_garden::search::{SearchParams, Searcher};
use ulei_garden::second::Provider;
use ulei_garden::serve::{Server, Settings, TIMEOUT, open};
use ulei_garden::store::{ModelChecks, Store, starting_domains};
use ulei_garden::wire::c7;
use ulei_garden::worker::{Clock, Passes, QUIET, Schedule, SystemClock, Wake, Worker};

const EMBED: &str = env!("CARGO_BIN_EXE_ulei-garden-embed");
const NOW: i64 = 1_800_000_000;
const SCHEDULE: Schedule = Schedule {
    expire_interval: 60,
    recheck_interval: 3_600,
    recheck_batch: 8,
    maintain_interval: 1_000,
    verify_interval: 86_400,
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
        checks: 0,
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

/// The answer of `server` to `candidate` on the night path over a socketpair: written by
/// `propose`, then processed at once by `seedbed_done`.
fn decided<C: Clock>(server: &mut Server<C>, candidate: &serde_json::Value) -> c7::ProposeResponse {
    let written: c7::ProposeResponse =
        serde_json::from_slice(&call(server, "propose", candidate)).unwrap();
    assert_eq!(written.status, c7::ProposalStatus::Seeded);
    let items = serde_json::json!({"items": [{"ref": written.proposal_id}]});
    let mut done: c7::SeedbedDoneResponse =
        serde_json::from_slice(&call(server, "seedbed_done", &items)).unwrap();
    done.items.remove(0).outcomes.remove(0)
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
            core: None,
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

    let answer = decided(
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
            core: None,
        },
        None,
    )
    .unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let clock = Fake::at(NOW);
    let mut server = Server::new(garden, uid, TIMEOUT, Arc::new(Wake::default()), clock);
    let answer = decided(
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
        core: None,
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
    missing_ids(scratch, helper).len()
}

/// The facts without a vector of `helper`'s model, in the order the worker embeds them.
fn missing_ids(scratch: &Scratch, helper: &Supervisor) -> Vec<String> {
    let model = helper.helper().unwrap().model_id().to_owned();
    let missing = Store::open(&scratch.store())
        .unwrap()
        .missing_vectors(&model)
        .unwrap();
    missing.into_iter().map(|m| m.0.0).collect()
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
        let answer = decided(
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
    assert!(
        check
            .vectors(first.model_id(), NOW, NOW, false)
            .unwrap()
            .is_empty(),
        "the start under the new identity deleted the old vectors"
    );
}

/// Ticks `worker` past [`QUIET`] until every fact has a vector of `helper`'s model.
#[cfg(feature = "native")]
fn drain_embeddings(
    worker: &mut Worker<Fake>,
    clock: &Fake,
    scratch: &Scratch,
    helper: &Supervisor,
) {
    for _ in 0..20 {
        if missing(scratch, helper) == 0 {
            return;
        }
        clock.0.set(clock.now() + QUIET + 1);
        worker.tick();
    }
    panic!(
        "the worker left {} facts without a vector",
        missing(scratch, helper)
    );
}

/// ADR-0086 E6: a deployment that moves from the ONNX helper (here the fake over ONNX-shaped
/// artifacts) to the encoder engine's weight file starts the helper as the daemon does, with
/// `--ort-lib` and `--threads` it now ignores; the new identity leaves every stored vector
/// stale, the worker deletes them and embeds every fact again under the engine's model id and
/// dimension, and the dense side finds them.
#[cfg(feature = "native")]
#[test]
fn moving_to_the_engines_weight_file_changes_the_model_id_and_the_worker_embeds_every_fact_again() {
    use ulei_garden::embed::ENGINE_FILE;
    use ulei_garden::serve::EmbedderSettings;

    let scratch = Scratch::new();
    let mut store = scratch.open();
    store.insert_fact(&fact("f-a", "a a", None)).unwrap();
    store.insert_fact(&fact("f-b", "b", None)).unwrap();
    let clock = Fake::at(NOW);
    let old = fake_helper(&scratch.0, "{}");
    let mut worker = worker_over(&scratch, store, &old, &clock);
    drain_embeddings(&mut worker, &clock, &scratch, &old);
    let old = old.helper().unwrap();
    let check = Store::open(&scratch.store()).unwrap();
    assert_eq!(
        check
            .vectors(old.model_id(), NOW, NOW, false)
            .unwrap()
            .len(),
        2
    );
    drop(worker);

    // The engine's weight file in a model directory of its own, as `convert` writes it.
    let tiny = tiny_bert::fixture("worker-switch");
    let out = tiny_bert::convert(&tiny.0, "w.uiw", &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let model_dir = scratch.0.join("engine");
    fs::create_dir_all(&model_dir).unwrap();
    fs::rename(tiny.0.join("w.uiw"), model_dir.join(ENGINE_FILE)).unwrap();
    let settings = EmbedderSettings {
        helper: PathBuf::from(EMBED),
        model_dir,
        ort_library: scratch.0.join("no-onnxruntime.so"),
        threads: 2,
        load_timeout: Duration::from_secs(30),
    };
    let start = move || settings.start();
    let engine = Arc::new(Supervisor::start(start, Arc::new(Wake::default())).unwrap());
    let new = engine.helper().unwrap();
    assert!(
        new.model_id().starts_with("ulei-infer-bert-int8-mean-6/"),
        "{}",
        new.model_id()
    );
    assert_ne!(new.model_id(), old.model_id());
    assert_eq!(new.dim(), tiny_bert::H);
    assert_eq!(
        check.missing_vectors(new.model_id()).unwrap().len(),
        2,
        "both are stale"
    );

    let mut worker = worker_over(&scratch, scratch.open(), &engine, &clock);
    drain_embeddings(&mut worker, &clock, &scratch, &engine);
    assert_eq!(check.missing_vectors(new.model_id()).unwrap().len(), 0);
    let vectors = check.vectors(new.model_id(), NOW, NOW, false).unwrap();
    assert_eq!(vectors.len(), 2);
    assert!(
        check
            .vectors(old.model_id(), NOW, NOW, false)
            .unwrap()
            .is_empty(),
        "the start under the engine's identity deleted the old vectors"
    );
    let q = new.embed_query("a a").unwrap();
    assert_eq!(q.len(), tiny_bert::H);
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
fn closed_versions_are_embedded_after_the_current_ones_behind_a_query_and_as_of_finds_them_dense() {
    let scratch = Scratch::new();
    let helper = fake_helper(&scratch.0, "{}");
    let mut store = private_store(&scratch);
    let old = "Cafe Lumen hosts a dentist on Fridays.";
    store
        .insert_fact(&fact_at("f-old", old, NOW - 100, None))
        .unwrap();
    store.supersede(&FactId("f-old".into()), NOW - 50).unwrap();
    for (id, text) in [
        ("f-door", "Cafe Lumen has a green door."),
        ("f-cake", "Cafe Lumen bakes plum cake."),
    ] {
        store.insert_fact(&fact(id, text, None)).unwrap();
    }
    let clock = Fake::at(NOW);
    let mut server = server_over(&scratch, &helper, &clock);
    let mut worker = worker_over(&scratch, store, &helper, &clock);
    let before = |server: &mut Server<Fake>| -> c7::SearchResponse {
        let body = json!({"q": "dentists", "as_of": NOW - 80});
        serde_json::from_slice(&call(server, "search", &body)).unwrap()
    };

    assert_eq!(worker.tick(), NOW);
    assert_eq!(missing_ids(&scratch, &helper), ["f-cake", "f-old"]);
    let unembedded = before(&mut server);
    assert!(
        unembedded.facts.is_empty(),
        "no word of the closed version starts with the query and it has no vector yet"
    );
    assert_eq!(
        worker.tick(),
        NOW + QUIET,
        "the passages wait for the query"
    );
    assert_eq!(missing_ids(&scratch, &helper), ["f-cake", "f-old"]);
    clock.0.set(NOW + QUIET);
    assert_eq!(worker.tick(), NOW + QUIET);
    assert_eq!(missing_ids(&scratch, &helper), ["f-old"]);
    assert_eq!(worker.tick(), NOW + QUIET);
    assert_eq!(missing(&scratch, &helper), 0);
    assert!(worker.tick() > NOW + QUIET, "nothing is left to embed");

    let dense = before(&mut server);
    assert_eq!(dense.lexical_only, None);
    assert_eq!(texts(&dense), [old]);
    assert!(!texts(&search(&mut server, "dentists")).contains(&old));
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
fn a_restart_under_another_identity_is_ready_drops_the_old_vectors_and_embeds_every_fact_again() {
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
    assert!(
        check.vectors(&old, NOW, NOW, false).unwrap().is_empty(),
        "the vector of the old identity is deleted"
    );
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
    let answer = decided(server, &about_ada(text));
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

/// Asserts that the dense side over the resident matrix of `helper` equals the one over the
/// vectors of the store at `path`, at `at` on both axes, for a few queries with and without
/// private facts.
fn matrix_agrees(path: &Path, helper: &Supervisor, at: i64) {
    let store = Store::open(path).unwrap();
    let embedder = helper.helper().unwrap();
    let mut found = 0;
    for q in ["green door", "opens at eight", "plum cake", "quiz night"] {
        let v = embedder.embed_query(q).unwrap();
        for private in [false, true] {
            let resident = helper
                .matrix()
                .nearest(embedder.model_id(), &v, at, at, private, 100)
                .expect("the matrix answers at its sync time");
            let p = SearchParams {
                include_private: private,
                ..SearchParams::at(at)
            };
            let stored = Searcher::new(&store, Some(embedder.as_ref()), &NoReranker)
                .candidates(q, &p)
                .unwrap()
                .dense;
            assert_eq!(resident, stored, "{q:?}, private {private}");
            found += stored.len();
        }
    }
    assert!(found > 0, "the dense side found something");
}

/// The facts the matrix of `helper` returns at `at` for a query about the café, private ones
/// included, sorted.
fn resident_ids(helper: &Supervisor, at: i64) -> Vec<String> {
    let embedder = helper.helper().unwrap();
    let v = embedder.embed_query("Cafe Lumen").unwrap();
    let mut ids: Vec<String> = helper
        .matrix()
        .nearest(embedder.model_id(), &v, at, at, true, 100)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id.0)
        .collect();
    ids.sort();
    ids
}

#[test]
fn the_matrix_is_built_from_the_store_at_start_and_follows_inserts_supersedes_expiry_and_merges() {
    let scratch = Scratch::new();
    let mut store = private_store(&scratch);
    let private = Fact {
        sensitivity: Sensitivity::Private,
        ..fact("f-quiz", "Cafe Lumen hosts a private quiz night.", None)
    };
    store
        .insert_facts(&[
            fact("f-door", "Cafe Lumen has a green door.", None),
            fact("f-cake", "Cafe Lumen bakes plum cake.", Some(NOW + 500)),
            private,
        ])
        .unwrap();
    let clock = Fake::at(NOW);
    let first = fake_helper(&scratch.0, "{}");
    drain(&mut worker_over(&scratch, store, &first, &clock), &clock);
    drop(first);

    let helper = fake_helper(&scratch.0, "{}");
    assert!(
        helper.matrix().is_empty(),
        "a new daemon starts without one"
    );
    let mut worker = worker_over(&scratch, scratch.open(), &helper, &clock);
    assert_eq!(drain(&mut worker, &clock), 0, "nothing is left to embed");
    assert_eq!(helper.matrix().len(), 3, "built from the stored vectors");
    matrix_agrees(&scratch.store(), &helper, NOW);

    clock.0.set(NOW + 10);
    let mut store = scratch.open();
    let hours = |id: &str, text: &str, at: i64| Fact {
        attribute: Some("hours".into()),
        ..fact_at(id, text, at, None)
    };
    store
        .insert_fact(&hours("f-eight", "Cafe Lumen opens at eight.", NOW + 10))
        .unwrap();
    drain(&mut worker, &clock);
    assert!(resident_ids(&helper, NOW + 10).contains(&"f-eight".to_owned()));
    matrix_agrees(&scratch.store(), &helper, NOW + 10);

    clock.0.set(NOW + 20);
    store
        .insert_fact(&hours("f-nine", "Cafe Lumen opens at nine.", NOW + 20))
        .unwrap();
    store.supersede(&FactId("f-door".into()), NOW + 20).unwrap();
    drain(&mut worker, &clock);
    let now = resident_ids(&helper, NOW + 20);
    assert!(now.contains(&"f-nine".to_owned()));
    assert!(!now.contains(&"f-eight".to_owned()) && !now.contains(&"f-door".to_owned()));
    matrix_agrees(&scratch.store(), &helper, NOW + 20);
    assert_eq!(
        helper.matrix().nearest(
            helper.helper().unwrap().model_id(),
            &[1.0],
            NOW + 15,
            NOW + 15,
            true,
            100
        ),
        None,
        "a search known before the last sync reads the store"
    );
    let before = Searcher::new(&store, Some(helper.helper().unwrap().as_ref()), &NoReranker)
        .candidates("green door", &SearchParams::at(NOW + 15))
        .unwrap()
        .dense;
    assert!(before.iter().any(|(id, _)| id.0 == "f-door"));

    clock.0.set(NOW + 500);
    drain(&mut worker, &clock);
    assert!(!resident_ids(&helper, NOW + 500).contains(&"f-cake".to_owned()));
    matrix_agrees(&scratch.store(), &helper, NOW + 500);

    clock.0.set(NOW + 600);
    store
        .upsert_entity(&Entity {
            id: EntityId("pl-lumen".into()),
            kind: EntityKind::Place,
            name: "Lumen".into(),
            aliases: Vec::new(),
            domain: None,
            attrs: Attrs::None,
        })
        .unwrap();
    let copies = store
        .merge_entities(
            &EntityId("pl-cafe".into()),
            &EntityId("pl-lumen".into()),
            NOW + 600,
        )
        .unwrap();
    drain(&mut worker, &clock);
    let merged = resident_ids(&helper, NOW + 600);
    let moved: Vec<&String> = copies
        .iter()
        .map(|c| &c.0)
        .filter(|c| c.starts_with("f-nine~") || c.starts_with("f-quiz~"))
        .collect();
    assert_eq!(moved.len(), 2);
    for copy in moved {
        assert!(merged.contains(copy), "{copy}");
    }
    assert!(!merged.contains(&"f-nine".to_owned()) && !merged.contains(&"f-quiz".to_owned()));
    matrix_agrees(&scratch.store(), &helper, NOW + 600);
}

/// A scripted helper that refuses, with `refusal`, or exits on, without it, every request
/// holding "poison", and answers every other one.
fn poisoned(scratch: &Scratch, refusal: Option<&str>) -> (Arc<Supervisor>, Arc<AtomicUsize>) {
    let on_poison = match refusal {
        Some(reason) => format!("printf '{{\"error\":\"{reason}\"}}\\n'"),
        None => "exit 0".to_owned(),
    };
    let per_call = format!(
        "case \"$line\" in *poison*) {on_poison} ;; *) {} ;; esac",
        scripted::ANSWER
    );
    let script = scripted::script(&scratch.0, "poisoned", &per_call);
    supervised(move |_| scripted::start(&script, CALL_DEADLINE))
}

/// The passages the script of [`poisoned`] was sent, by the fact text they end with.
fn sent(scratch: &Scratch) -> Vec<String> {
    scripted::log(&scratch.0, "poisoned")
        .iter()
        .map(|line| {
            let request: serde_json::Value = serde_json::from_str(line).unwrap();
            let text = request["texts"][0].as_str().unwrap();
            text.rsplit(' ')
                .next()
                .unwrap()
                .trim_end_matches('.')
                .to_owned()
        })
        .collect()
}

/// The café store of `scratch` with a passage that holds "poison" first and two plain ones.
fn poison_store(scratch: &Scratch) -> Store {
    let mut store = private_store(scratch);
    store
        .insert_facts(&[
            fact("f-bad", "Cafe Lumen sells poison ivy.", None),
            fact("f-door", "Cafe Lumen has a green door.", None),
            fact("f-cake", "Cafe Lumen bakes plum cake.", None),
        ])
        .unwrap();
    store
}

#[test]
fn a_helper_that_falls_on_one_passage_backs_off_and_the_passage_is_skipped_after_three_falls() {
    let scratch = Scratch::new();
    let store = poison_store(&scratch);
    let (helper, starts) = poisoned(&scratch, None);
    let clock = Fake::at(NOW);
    let mut server = server_over(&scratch, &helper, &clock);
    let mut worker = worker_over(&scratch, store, &helper, &clock);
    let model = helper.helper().unwrap().model_id().to_owned();
    let missing = || -> Vec<String> {
        let store = Store::open(&scratch.store()).unwrap();
        let missing = store.missing_vectors(&model).unwrap();
        missing.into_iter().map(|m| m.0.0).collect()
    };

    assert_eq!(
        worker.tick(),
        NOW + 1,
        "the helper fell on the first passage"
    );
    clock.0.set(NOW + 1);
    assert_eq!(worker.tick(), NOW + 1);
    assert_eq!(worker.tick(), NOW + 1);
    assert_eq!(missing(), ["f-bad"], "the other passages went first");
    assert_eq!(
        worker.tick(),
        NOW + 3,
        "fallen again soon after its restart, it waits twice as long"
    );
    clock.0.set(NOW + 3);
    assert_eq!(worker.tick(), NOW + 7, "the third fall skips the passage");
    assert_eq!(missing(), Vec::<String>::new());
    clock.0.set(NOW + 7);
    assert_eq!(worker.tick(), NOW + SCHEDULE.maintain_interval);
    assert_eq!(
        starts.load(Ordering::SeqCst),
        4,
        "the first start and 3 restarts"
    );
    assert_eq!(health(&mut server).dense, c7::DenseState::Ready);
    assert_eq!(sent(&scratch), ["ivy", "door", "cake", "ivy", "ivy"]);
    let vectors = Store::open(&scratch.store())
        .unwrap()
        .vectors(&model, NOW + 7, NOW + 7, false)
        .unwrap();
    let embedded: Vec<&str> = vectors.iter().map(|(id, _)| id.0.as_str()).collect();
    assert_eq!(
        embedded,
        ["f-door", "f-cake"],
        "the skipped passage has no vector"
    );
    let found = search(&mut server, "ivy");
    assert_eq!(
        texts(&found)[0],
        "Cafe Lumen sells poison ivy.",
        "found by its words"
    );
    assert_eq!(found.lexical_only, None);

    let later = NOW + 7 + STEADY;
    clock.0.set(later);
    scratch
        .open()
        .insert_fact(&fact_at(
            "f-worse",
            "Cafe Lumen sells poison oak.",
            later,
            None,
        ))
        .unwrap();
    assert_eq!(
        worker.tick(),
        later + 1,
        "a helper that stayed up falls back to the first pause"
    );
}

#[test]
fn a_refused_passage_does_not_hold_up_the_next_and_is_skipped_after_three_refusals() {
    let scratch = Scratch::new();
    let store = poison_store(&scratch);
    let (helper, starts) = poisoned(&scratch, Some("cannot embed"));
    let clock = Fake::at(NOW);
    let mut worker = worker_over(&scratch, store, &helper, &clock);

    assert_eq!(worker.tick(), NOW, "the next passage is due at once");
    assert_eq!(
        missing_ids(&scratch, &helper),
        ["f-door", "f-cake", "f-bad"]
    );
    assert_eq!(drain(&mut worker, &clock), 4);
    assert_eq!(sent(&scratch), ["ivy", "door", "cake", "ivy", "ivy"]);
    assert_eq!(missing_ids(&scratch, &helper), Vec::<String>::new());
    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "a refusal restarts nothing"
    );
    assert_eq!(worker.passes(), passes(0, 0, 3));
}

#[test]
fn a_helper_that_died_while_idle_charges_no_failure_to_the_next_passage() {
    let scratch = Scratch::new();
    let mut store = private_store(&scratch);
    store
        .insert_fact(&fact("f-bad", "Cafe Lumen sells poison ivy.", None))
        .unwrap();
    let (helper, starts) = poisoned(&scratch, None);
    let clock = Fake::at(NOW);
    let mut worker = worker_over(&scratch, store, &helper, &clock);
    let model = helper.helper().unwrap().model_id().to_owned();
    let missing = || -> Vec<String> {
        let store = Store::open(&scratch.store()).unwrap();
        let missing = store.missing_vectors(&model).unwrap();
        missing.into_iter().map(|m| m.0.0).collect()
    };

    scripted::kill_idle(scripted::pid(&scratch.0, "poisoned"));
    assert_eq!(worker.tick(), NOW + 1, "found down before any passage");
    assert_eq!(sent(&scratch), Vec::<String>::new());
    clock.0.set(NOW + 1);
    assert_eq!(
        worker.tick(),
        NOW + 3,
        "back, then its first fall on the passage"
    );
    clock.0.set(NOW + 3);
    assert_eq!(worker.tick(), NOW + 7, "its second fall");
    assert_eq!(missing(), ["f-bad"], "two falls do not skip the passage");
    clock.0.set(NOW + 7);
    assert_eq!(worker.tick(), NOW + 15, "its third fall");
    assert_eq!(missing(), Vec::<String>::new());
    assert_eq!(sent(&scratch), ["ivy", "ivy", "ivy"]);
    assert_eq!(starts.load(Ordering::SeqCst), 4);
}

#[test]
fn a_search_at_now_in_the_daemon_is_answered_from_the_matrix_and_one_before_its_sync_from_the_store()
 {
    let scratch = Scratch::new();
    let mut store = private_store(&scratch);
    let dentist = "Cafe Lumen hosts a dentist on Fridays.";
    store
        .insert_fact(&fact("f-dentist", dentist, None))
        .unwrap();
    let helper = fake_helper(&scratch.0, "{}");
    let clock = Fake::at(NOW);
    let mut server = server_over(&scratch, &helper, &clock);
    let mut worker = worker_over(&scratch, store, &helper, &clock);
    drain(&mut worker, &clock);
    clock.0.set(NOW + 10);
    scratch
        .open()
        .insert_fact(&fact_at(
            "f-door",
            "Cafe Lumen has a green door.",
            NOW + 10,
            None,
        ))
        .unwrap();
    drain(&mut worker, &clock);
    assert_eq!(
        texts(&search(&mut server, "dentists")).first(),
        Some(&dentist)
    );

    let deleted = Store::open(&scratch.store())
        .unwrap()
        .conn()
        .execute("DELETE FROM fact_vectors", [])
        .unwrap();
    assert_eq!(deleted, 2);
    let now = search(&mut server, "dentists");
    assert_eq!(now.lexical_only, None);
    assert_eq!(
        texts(&now).first(),
        Some(&dentist),
        "no word starts with the query and the store holds no vector: only the matrix has it"
    );
    let before: c7::SearchResponse = serde_json::from_slice(&call(
        &mut server,
        "search",
        &json!({"q": "dentists", "as_of": NOW + 5}),
    ))
    .unwrap();
    assert_eq!(
        texts(&before),
        Vec::<&str>::new(),
        "known before the last sync, the search reads the store"
    );
}

/// A content check of the model file (ADR-0086 open question 15) that answers what `verdict`
/// holds when it runs, counting its runs.
fn scripted_check(
    verdict: &Arc<std::sync::Mutex<Verdict>>,
    runs: &Arc<AtomicUsize>,
) -> impl Fn() -> Verdict + Send + 'static {
    let (verdict, runs) = (Arc::clone(verdict), Arc::clone(runs));
    move || {
        runs.fetch_add(1, Ordering::SeqCst);
        verdict.lock().unwrap().clone()
    }
}

/// The worker of `store` with `helper` on `clock`, checking the model file with `check` every
/// `interval` seconds.
fn checking_worker(
    scratch: &Scratch,
    store: Store,
    helper: &Arc<Supervisor>,
    clock: &Fake,
    interval: i64,
    check: impl Fn() -> Verdict + Send + 'static,
) -> Worker<Fake> {
    Worker::new(
        store,
        Schedule {
            verify_interval: interval,
            ..SCHEDULE
        },
        clock.clone(),
        scratch.inbox(),
        Provider::None.source(),
        EdgeConfig::default(),
    )
    .unwrap()
    .with_helper(Arc::clone(helper))
    .with_check(check)
}

/// The digest of the scripted helpers' ready line of [`scripted::set_identity`] `digit`.
fn digest_of(digit: char) -> String {
    std::iter::repeat_n(digit, 64).collect()
}

/// The facts of the store of `scratch` with a vector of `model`, by id.
fn embedded(scratch: &Scratch, model: &str) -> Vec<String> {
    let store = Store::open(&scratch.store()).unwrap();
    let mut ids: Vec<String> = store
        .vectors(model, NOW, i64::MAX, true)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id.0)
        .collect();
    ids.sort();
    ids
}

/// The texts of the passages the scripted helper `name` of `scratch` was sent, in order.
fn passages(scratch: &Scratch, name: &str) -> Vec<String> {
    scripted::log(&scratch.0, name)
        .iter()
        .filter_map(|line| {
            let request: serde_json::Value = serde_json::from_str(line).unwrap();
            (request["kind"] == "passage").then(|| request["texts"][0].as_str().unwrap().to_owned())
        })
        .collect()
}

/// The café's door, opening hours and cake, all current.
fn three_facts(store: &mut Store) {
    for (id, text) in [
        ("f-door", "Cafe Lumen has a green door."),
        ("f-hours", "Cafe Lumen opens at eight."),
        ("f-cake", "Cafe Lumen bakes plum cake."),
    ] {
        store.insert_fact(&fact(id, text, None)).unwrap();
    }
}

const CORRUPTED: &str = "model file corrupted: /models/e5/model.ulei-infer: content SHA-256 is \
                         0f, the preamble says aa";

#[test]
fn the_model_check_runs_at_the_first_tick_then_every_interval_as_a_deadline_and_records_its_start_and_digest()
 {
    let scratch = Scratch::new();
    let mut store = private_store(&scratch);
    store
        .insert_fact(&fact("f-door", "Cafe Lumen has a green door.", None))
        .unwrap();
    let script = scripted::script(&scratch.0, "helper", scripted::ANSWER);
    let (helper, _) = supervised(move |_| scripted::start(&script, CALL_DEADLINE));
    let verdict = Arc::new(std::sync::Mutex::new(Verdict::Good(digest_of('a'))));
    let runs = Arc::new(AtomicUsize::new(0));
    let clock = Fake::at(NOW);
    let mut worker = checking_worker(
        &scratch,
        store,
        &helper,
        &clock,
        400,
        scripted_check(&verdict, &runs),
    );
    assert_eq!(worker.tick(), NOW, "embedded, and the next tick is due");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "checked at the first tick");
    let recorded = || {
        Store::open(&scratch.store())
            .unwrap()
            .model_checks()
            .unwrap()
    };
    assert_eq!(
        recorded(),
        ModelChecks {
            passed: Some((NOW, digest_of('a'))),
            failed: None
        }
    );
    assert_eq!(
        worker.tick(),
        NOW + 400,
        "the next check is the nearest deadline, before maintenance at NOW + 1000"
    );
    clock.0.set(NOW + 399);
    assert_eq!(worker.tick(), NOW + 400);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    clock.0.set(NOW + 400);
    assert_eq!(worker.tick(), NOW + 800);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    assert_eq!(recorded().passed, Some((NOW + 400, digest_of('a'))));
    assert_eq!(worker.passes().checks, 2);

    // A check without a verdict changes nothing but its due time.
    *verdict.lock().unwrap() = Verdict::Failed("verify exited 1: no such file".into());
    clock.0.set(NOW + 800);
    assert_eq!(worker.tick(), NOW + 1000);
    assert_eq!(recorded().passed, Some((NOW + 400, digest_of('a'))));
    assert!(helper.helper().is_ok());
}

#[test]
fn a_corrupted_model_file_holds_the_helper_down_drops_only_the_vectors_since_the_last_passed_check_and_a_passed_check_brings_it_back()
 {
    let scratch = Scratch::new();
    let mut store = private_store(&scratch);
    three_facts(&mut store);
    let script = scripted::script(&scratch.0, "helper", scripted::ANSWER);
    let (helper, starts) = supervised(move |_| scripted::start(&script, CALL_DEADLINE));
    let model = helper.helper().unwrap().model_id().to_owned();
    // Embedded before the first check, by an earlier run of the daemon.
    store
        .put_vector(&FactId("f-door".into()), &model, &[1.0], NOW - 100)
        .unwrap();
    let verdict = Arc::new(std::sync::Mutex::new(Verdict::Good(digest_of('a'))));
    let runs = Arc::new(AtomicUsize::new(0));
    let clock = Fake::at(NOW);
    let mut server = server_over(&scratch, &helper, &clock);
    let mut worker = checking_worker(
        &scratch,
        store,
        &helper,
        &clock,
        400,
        scripted_check(&verdict, &runs),
    );
    drain(&mut worker, &clock);
    clock.0.set(NOW + 10);
    drain(&mut worker, &clock);
    assert_eq!(embedded(&scratch, &model), ["f-cake", "f-door", "f-hours"]);
    let pid = scripted::pid(&scratch.0, "helper");

    *verdict.lock().unwrap() = Verdict::Corrupted(CORRUPTED.into());
    clock.0.set(NOW + 400);
    assert_eq!(worker.tick(), NOW + 800, "only the next check is due");
    let down = health(&mut server);
    assert_eq!(down.dense, c7::DenseState::Down);
    assert_eq!(down.dense_error.as_deref(), Some(CORRUPTED));
    assert_eq!(
        embedded(&scratch, &model),
        ["f-door"],
        "the vectors computed since the check at NOW went, the older one stayed"
    );
    assert!(!scripted::alive(pid), "the helper is stopped");
    let found = search(&mut server, "plum cake");
    assert_eq!(found.facts.len(), 1);
    assert_eq!(found.lexical_only, Some(true));
    let recorded = Store::open(&scratch.store())
        .unwrap()
        .model_checks()
        .unwrap();
    assert_eq!(recorded.passed, Some((NOW, digest_of('a'))));
    assert_eq!(recorded.failed, Some((NOW + 400, CORRUPTED.into())));

    // Held down: no restart and no passage, however long it waits.
    for at in [NOW + 401, NOW + 460, NOW + 700, NOW + 799] {
        clock.0.set(at);
        assert_eq!(worker.tick(), NOW + 800);
    }
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(health(&mut server).dense, c7::DenseState::Down);

    // Verified again with the same digest: the same model comes back and only the dropped
    // facts are embedded again.
    *verdict.lock().unwrap() = Verdict::Good(digest_of('a'));
    clock.0.set(NOW + 800);
    assert_eq!(worker.tick(), NOW + 800, "the restart is due at once");
    drain(&mut worker, &clock);
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(health(&mut server).dense, c7::DenseState::Ready);
    assert_eq!(helper.helper().unwrap().model_id(), model);
    assert_eq!(embedded(&scratch, &model), ["f-cake", "f-door", "f-hours"]);
    assert_eq!(
        passages(&scratch, "helper"),
        [
            "Cafe Lumen: Cafe Lumen opens at eight.",
            "Cafe Lumen: Cafe Lumen bakes plum cake.",
            "Cafe Lumen: Cafe Lumen opens at eight.",
            "Cafe Lumen: Cafe Lumen bakes plum cake.",
        ],
        "f-door was never sent again"
    );
    assert_eq!(
        Store::open(&scratch.store())
            .unwrap()
            .model_checks()
            .unwrap(),
        ModelChecks {
            passed: Some((NOW + 800, digest_of('a'))),
            failed: None
        }
    );
    assert_eq!(search(&mut server, "plum cake").lexical_only, None);
}

#[test]
fn a_corrupted_file_replaced_by_another_model_comes_back_as_a_new_model_and_every_fact_is_embedded_again()
 {
    let scratch = Scratch::new();
    let mut store = private_store(&scratch);
    three_facts(&mut store);
    let script = scripted::script(&scratch.0, "helper", scripted::ANSWER);
    let (helper, _) = supervised(move |_| scripted::start(&script, CALL_DEADLINE));
    let old = helper.helper().unwrap().model_id().to_owned();
    store
        .put_vector(&FactId("f-door".into()), &old, &[1.0], NOW - 100)
        .unwrap();
    let verdict = Arc::new(std::sync::Mutex::new(Verdict::Good(digest_of('a'))));
    let runs = Arc::new(AtomicUsize::new(0));
    let clock = Fake::at(NOW);
    let mut server = server_over(&scratch, &helper, &clock);
    let mut worker = checking_worker(
        &scratch,
        store,
        &helper,
        &clock,
        400,
        scripted_check(&verdict, &runs),
    );
    drain(&mut worker, &clock);
    *verdict.lock().unwrap() = Verdict::Corrupted(CORRUPTED.into());
    clock.0.set(NOW + 400);
    worker.tick();
    assert_eq!(embedded(&scratch, &old), ["f-door"]);

    scripted::set_identity(&scratch.0, 'b');
    *verdict.lock().unwrap() = Verdict::Good(digest_of('b'));
    clock.0.set(NOW + 800);
    drain(&mut worker, &clock);
    assert_eq!(health(&mut server).dense, c7::DenseState::Ready);
    let new = helper.helper().unwrap().model_id().to_owned();
    assert_ne!(new, old);
    assert_eq!(
        embedded(&scratch, &old),
        Vec::<String>::new(),
        "the old model's vectors went"
    );
    assert_eq!(embedded(&scratch, &new), ["f-cake", "f-door", "f-hours"]);
}

#[test]
fn a_model_file_replaced_under_the_running_helper_restarts_it_on_the_new_file() {
    let scratch = Scratch::new();
    let mut store = private_store(&scratch);
    three_facts(&mut store);
    let script = scripted::script(&scratch.0, "helper", scripted::ANSWER);
    let (helper, starts) = supervised(move |_| scripted::start(&script, CALL_DEADLINE));
    let old = helper.helper().unwrap().model_id().to_owned();
    let verdict = Arc::new(std::sync::Mutex::new(Verdict::Good(digest_of('a'))));
    let runs = Arc::new(AtomicUsize::new(0));
    let clock = Fake::at(NOW);
    let mut worker = checking_worker(
        &scratch,
        store,
        &helper,
        &clock,
        400,
        scripted_check(&verdict, &runs),
    );
    drain(&mut worker, &clock);
    assert_eq!(embedded(&scratch, &old).len(), 3);

    // A new file renamed over the old one: the running helper still maps the old file.
    scripted::set_identity(&scratch.0, 'b');
    *verdict.lock().unwrap() = Verdict::Good(digest_of('b'));
    clock.0.set(NOW + 400);
    drain(&mut worker, &clock);
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    let new = helper.helper().unwrap().model_id().to_owned();
    assert_ne!(new, old);
    assert_eq!(embedded(&scratch, &old), Vec::<String>::new());
    assert_eq!(embedded(&scratch, &new).len(), 3);
}

#[test]
fn a_restart_refused_for_a_corrupted_file_is_held_down_and_checked_at_once() {
    let scratch = Scratch::new();
    let mut store = private_store(&scratch);
    three_facts(&mut store);
    let exits = scripted::script(&scratch.0, "exits", &scripted::exits_after(0));
    let (helper, starts) = supervised(move |n| match n {
        0 => scripted::start(&exits, CALL_DEADLINE),
        _ => Err(StartError::Corrupt(CORRUPTED.into())),
    });
    // The first check passes at the first tick; the file breaks after it.
    let verdict = Arc::new(std::sync::Mutex::new(Verdict::Good(digest_of('a'))));
    let runs = Arc::new(AtomicUsize::new(0));
    let clock = Fake::at(NOW);
    let mut server = server_over(&scratch, &helper, &clock);
    let mut worker = checking_worker(
        &scratch,
        store,
        &helper,
        &clock,
        86_400,
        scripted_check(&verdict, &runs),
    );
    assert_eq!(
        worker.tick(),
        NOW + 1,
        "the helper fell on its first passage"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    *verdict.lock().unwrap() = Verdict::Corrupted(CORRUPTED.into());
    clock.0.set(NOW + 1);
    worker.tick();
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "checked at once, not a day later"
    );
    assert_eq!(
        Store::open(&scratch.store())
            .unwrap()
            .model_checks()
            .unwrap()
            .failed,
        Some((NOW + 1, CORRUPTED.into()))
    );
    assert_eq!(health(&mut server).dense_error.as_deref(), Some(CORRUPTED));
    for at in [NOW + 2, NOW + 3, NOW + 5, NOW + 9, NOW + 70, NOW + 200] {
        clock.0.set(at);
        assert_eq!(
            worker.tick(),
            NOW + SCHEDULE.maintain_interval,
            "nothing is due before maintenance"
        );
    }
    assert_eq!(
        starts.load(Ordering::SeqCst),
        2,
        "no restart against the corrupted file"
    );
}
