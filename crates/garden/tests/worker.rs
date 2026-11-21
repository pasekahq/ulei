//! The daemon's worker through `ulei_garden::worker` on an injected clock, and the wake a
//! request sends it through `ulei_garden::serve::Server`. All people, places and texts are
//! fictional.

#![allow(missing_docs, clippy::unwrap_used)]

use std::cell::Cell;
use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::json;
use ulei_garden::api::ApiConfig;
use ulei_garden::edge::EdgeConfig;
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
use ulei_garden::worker::{Clock, Passes, Schedule, SystemClock, Wake, Worker};

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

/// The answer of `server` to a `propose` of `candidate` on a socketpair.
fn propose<C: Clock>(server: &mut Server<C>, candidate: &serde_json::Value) -> c7::ProposeResponse {
    let body = candidate.to_string();
    let (mut client, end) = UnixStream::pair().unwrap();
    write!(
        client,
        "POST /v1/propose HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    server.serve(end);
    let mut raw = Vec::new();
    client.read_to_end(&mut raw).unwrap();
    let start = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(raw.starts_with(b"HTTP/1.1 200 "));
    serde_json::from_slice(&raw[start..]).unwrap()
}

#[test]
fn a_fact_a_request_writes_wakes_the_worker_and_an_earlier_expiry_moves_its_deadline() {
    let scratch = Scratch::new();
    let garden = open(&Settings {
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
    })
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
    let garden = open(&Settings {
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
    })
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
