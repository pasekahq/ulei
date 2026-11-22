//! The second level end to end: a search miss or an expired fact on
//! `ulei_garden::serve::Server`, the worker on an injected clock asking a fake
//! `ulei_garden::second::Source`, and a fake C1 inbox on a Unix socket in the test. Places,
//! URLs and keys are fictional.

#![allow(missing_docs, clippy::unwrap_used, clippy::cast_precision_loss)]

use std::cell::Cell;
use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use ulei_garden::api::ApiConfig;
use ulei_garden::edge::EdgeConfig;
use ulei_garden::miss::{Inbox, ProducerKey};
use ulei_garden::model::{FactId, SourceKind, TtlKind, TtlTable};
use ulei_garden::rerank;
use ulei_garden::second::{Provider, Source, SourceError};
use ulei_garden::serve::{Server, Settings, TIMEOUT, open};
use ulei_garden::store::{Store, starting_domains};
use ulei_garden::wire::c7;
use ulei_garden::worker::{Clock, Schedule, Wake, Worker};

const NOW: i64 = 1_800_000_000;
const SCHEDULE: Schedule = Schedule {
    expire_interval: 60,
    recheck_interval: 3_600,
    recheck_batch: 8,
    maintain_interval: 100_000_000,
};
const QUESTION: &str = "opening hours of Cafe Lumen";
const URL: &str = "https://cafe-lumen.example.org/hours";
const OBSERVED: i64 = NOW - 100;

/// A clock the test sets, shared by the request path and the worker.
#[derive(Clone)]
struct Fake(Rc<Cell<i64>>);

impl Clock for Fake {
    fn now(&self) -> i64 {
        self.0.get()
    }
}

/// What the fake source answers to one lookup.
enum Answer {
    Finds(Vec<c7::ProposeRequest>),
    Fails(&'static str),
}

/// A source that answers lookups in order from its script and records each query it was
/// asked.
#[derive(Clone, Default)]
struct Scripted {
    script: Arc<Mutex<VecDeque<Answer>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Scripted {
    fn new(answers: Vec<Answer>) -> Self {
        Self {
            script: Arc::new(Mutex::new(answers.into())),
            asked: Arc::default(),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl Source for Scripted {
    fn find(&self, q: &str, _now: i64) -> Result<Vec<c7::ProposeRequest>, SourceError> {
        self.asked.lock().unwrap().push(q.to_owned());
        match self.script.lock().unwrap().pop_front() {
            Some(Answer::Finds(found)) => Ok(found),
            Some(Answer::Fails(class)) => Err(SourceError::new(class)),
            None => Ok(Vec::new()),
        }
    }
}

/// The opening hours of the café as an internet page states them.
fn hours() -> c7::ProposeRequest {
    serde_json::from_value(json!({
        "fact": "Cafe Lumen opening hours are 8 to 18 on weekdays.",
        "entity": "Cafe Lumen",
        "kind": "place",
        "source_ref": URL,
        "source_kind": "internet",
        "observed_at": OBSERVED,
        "ttl_kind": "opening_hours",
    }))
    .unwrap()
}

/// A fresh directory short enough for socket paths, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let temp = std::env::temp_dir();
        let base = if temp.as_os_str().len() <= 48 {
            temp
        } else {
            PathBuf::from("/tmp")
        };
        let dir = base.join(format!("ulei-garden-second-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self(dir)
    }

    fn store(&self) -> PathBuf {
        self.0.join("garden.sqlite3")
    }

    fn inbox_socket(&self) -> PathBuf {
        self.0.join("inbox.sock")
    }

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

    /// The request side of the daemon and its worker asking `source`, both on `clock`.
    fn daemon(&self, clock: &Fake, source: &Scripted) -> (Server<Fake>, Worker<Fake>) {
        self.daemon_with(clock, source, SCHEDULE)
    }

    /// [`Scratch::daemon`] with the worker on `schedule`.
    fn daemon_with(
        &self,
        clock: &Fake,
        source: &Scripted,
        schedule: Schedule,
    ) -> (Server<Fake>, Worker<Fake>) {
        let garden = open(
            &Settings {
                store: self.store(),
                socket: self.0.join("garden.sock"),
                producer_key: self.key(),
                inbox_socket: self.inbox_socket(),
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
        let server = Server::new(
            garden,
            uid,
            TIMEOUT,
            Arc::new(Wake::default()),
            clock.clone(),
        );
        let worker = Worker::new(
            Store::open(&self.store()).unwrap(),
            schedule,
            clock.clone(),
            Inbox::new(self.inbox_socket(), self.key()),
            Some(Box::new(source.clone())),
            EdgeConfig::default(),
        )
        .unwrap();
        (server, worker)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A fake C1 inbox at `socket` that stores one packet and returns the request body.
fn inbox(socket: &PathBuf) -> JoinHandle<Value> {
    let _ = fs::remove_file(socket);
    let listener = UnixListener::bind(socket).unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        let body = loop {
            let n = stream.read(&mut chunk).unwrap();
            assert!(n > 0, "request ended early");
            raw.extend_from_slice(&chunk[..n]);
            let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = std::str::from_utf8(&raw[..end]).unwrap();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .unwrap()
                .parse()
                .unwrap();
            if raw.len() >= end + 4 + length {
                break serde_json::from_slice::<Value>(&raw[end + 4..]).unwrap();
            }
        };
        let text = json!({"id": body["packet"]["id"]}).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
            text.len()
        )
        .unwrap();
        body
    })
}

/// The status and JSON body `server` answers to `method` with `body` on a socketpair.
fn call(server: &mut Server<Fake>, method: &str, body: &Value) -> Value {
    let text = body.to_string();
    let (mut client, end) = UnixStream::pair().unwrap();
    write!(
        client,
        "POST /v1/{method} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{text}",
        text.len()
    )
    .unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    server.serve(end);
    let mut raw = Vec::new();
    client.read_to_end(&mut raw).unwrap();
    let start = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert_eq!(&raw[9..12], b"200", "{}", String::from_utf8_lossy(&raw));
    serde_json::from_slice(&raw[start..]).unwrap()
}

fn search(server: &mut Server<Fake>, q: &str) -> c7::SearchResponse {
    serde_json::from_value(call(server, "search", &json!({ "q": q }))).unwrap()
}

fn unpublished(server: &mut Server<Fake>) -> u64 {
    serde_json::from_value::<c7::Health>(call(server, "health", &json!({})))
        .unwrap()
        .unpublished_misses
}

#[test]
fn a_miss_the_source_answers_is_written_with_its_provenance_answered_from_the_garden_and_not_published()
 {
    let scratch = Scratch::new();
    let clock = Fake(Rc::new(Cell::new(NOW)));
    let source = Scripted::new(vec![Answer::Finds(vec![hours()])]);
    let (mut server, mut worker) = scratch.daemon(&clock, &source);
    let expires = TtlTable::default()
        .expiry(TtlKind::OpeningHours, OBSERVED)
        .unwrap();

    assert_eq!(search(&mut server, QUESTION).facts.len(), 0);
    assert_eq!(source.asked().len(), 0, "the request never asks the source");
    // Nobody serves the inbox, so a publish would leave the miss unpublished.
    assert_eq!(
        worker.tick(),
        expires,
        "the new fact's expiry is the next deadline"
    );
    assert_eq!(source.asked(), vec![QUESTION.to_owned()]);
    assert_eq!(
        unpublished(&mut server),
        0,
        "the answered miss is not published"
    );

    let found = search(&mut server, QUESTION);
    assert_eq!(found.facts.len(), 1, "{found:?}");
    let fact = &found.facts[0];
    assert_eq!(
        fact.fact,
        "Cafe Lumen opening hours are 8 to 18 on weekdays."
    );
    assert_eq!(fact.source_kind, Some(c7::SourceKind::Internet));
    assert_eq!(fact.source_ref, URL);
    assert_eq!(fact.expires_at, Some(expires as f64));
    let id = FactId(fact.fact_id.clone().unwrap());
    let stored = Store::open(&scratch.store())
        .unwrap()
        .fact(&id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.provenance.kind, SourceKind::Internet);
    assert_eq!(stored.provenance.observed_at, OBSERVED);
    assert_eq!(
        unpublished(&mut server),
        0,
        "an answered search queues no miss"
    );

    clock.0.set(expires - 1);
    assert_eq!(
        search(&mut server, QUESTION).facts.len(),
        1,
        "still in force"
    );
    clock.0.set(expires);
    worker.tick();
    assert_eq!(search(&mut server, QUESTION).facts.len(), 0, "expired");
    let rechecks = Store::open(&scratch.store()).unwrap().rechecks(10).unwrap();
    assert_eq!(
        rechecks.iter().map(|f| &f.id).collect::<Vec<_>>(),
        vec![&id]
    );
}

#[test]
fn a_miss_the_source_finds_nothing_for_is_published_with_its_query() {
    let scratch = Scratch::new();
    let clock = Fake(Rc::new(Cell::new(NOW)));
    let source = Scripted::new(vec![Answer::Finds(Vec::new())]);
    let (mut server, mut worker) = scratch.daemon(&clock, &source);
    search(&mut server, QUESTION);

    let fake = inbox(&scratch.inbox_socket());
    worker.tick();
    let body = fake.join().unwrap();
    assert_eq!(source.asked(), vec![QUESTION.to_owned()]);
    assert_eq!(body["packet"]["kind"], "garden_miss");
    assert_eq!(
        body["packet"]["payload"],
        format!("The garden has nothing for: {QUESTION}")
    );
    assert_eq!(unpublished(&mut server), 0);
    worker.tick();
    assert_eq!(
        source.asked().len(),
        1,
        "a row is handed to the source once"
    );
}

#[test]
fn a_candidate_the_edge_does_not_accept_leaves_the_miss_to_publish() {
    let scratch = Scratch::new();
    let clock = Fake(Rc::new(Cell::new(NOW)));
    let weak = c7::ProposeRequest {
        confidence: Some(0.2),
        ..hours()
    };
    let source = Scripted::new(vec![Answer::Finds(vec![weak])]);
    let (mut server, mut worker) = scratch.daemon(&clock, &source);
    search(&mut server, QUESTION);

    let fake = inbox(&scratch.inbox_socket());
    worker.tick();
    assert_eq!(fake.join().unwrap()["packet"]["kind"], "garden_miss");
    let digest: c7::DigestResponse =
        serde_json::from_value(call(&mut server, "digest", &json!({}))).unwrap();
    assert_eq!(
        digest.items.len(),
        1,
        "the weak candidate waits for the owner"
    );
    assert_eq!(digest.items[0].candidate.source_ref, URL);
    assert_eq!(search(&mut server, QUESTION).facts.len(), 0);
}

#[test]
fn a_query_that_looks_secret_is_never_handed_to_the_source() {
    let scratch = Scratch::new();
    let clock = Fake(Rc::new(Cell::new(NOW)));
    let source = Scripted::new(vec![Answer::Finds(vec![hours()])]);
    let (mut server, mut worker) = scratch.daemon(&clock, &source);
    search(&mut server, "wifi password: hunter2x");

    let fake = inbox(&scratch.inbox_socket());
    worker.tick();
    fake.join().unwrap();
    assert_eq!(source.asked().len(), 0);
}

/// Set in the child run of the test below, which re-runs it in a process of its own.
const FAILED_LOOKUP_CHILD: &str = "ULEI_GARDEN_TEST_FAILED_LOOKUP_CHILD";

#[test]
fn a_failed_lookup_is_one_stderr_line_with_its_class_and_the_miss_is_published() {
    let name = "a_failed_lookup_is_one_stderr_line_with_its_class_and_the_miss_is_published";
    if std::env::var_os(FAILED_LOOKUP_CHILD).is_none() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(FAILED_LOOKUP_CHILD, "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(out.status.success(), "{stderr}");
        assert_eq!(stderr, "garden: second level: unreachable\n");
        return;
    }
    let scratch = Scratch::new();
    let clock = Fake(Rc::new(Cell::new(NOW)));
    let source = Scripted::new(vec![Answer::Fails("unreachable")]);
    let (mut server, mut worker) = scratch.daemon(&clock, &source);
    search(&mut server, QUESTION);

    let fake = inbox(&scratch.inbox_socket());
    worker.tick();
    let body = fake.join().unwrap();
    assert_eq!(source.asked(), vec![QUESTION.to_owned()]);
    assert_eq!(body["packet"]["kind"], "garden_miss");
    assert_eq!(unpublished(&mut server), 0);
}

const RENEWED_URL: &str = "https://cafe-lumen.example.org/hours-autumn";
const RENEWED: &str = "Cafe Lumen opening hours are 9 to 17 on weekdays.";

/// The café's hours as the page states them `at` a later visit, under a name the source chose.
fn renewed(at: i64) -> c7::ProposeRequest {
    c7::ProposeRequest {
        fact: RENEWED.into(),
        entity: Some("Lumen".into()),
        source_ref: RENEWED_URL.into(),
        observed_at: Some(at as f64),
        ..hours()
    }
}

fn search_as_of(server: &mut Server<Fake>, q: &str, at: i64) -> c7::SearchResponse {
    serde_json::from_value(call(server, "search", &json!({ "q": q, "as_of": at }))).unwrap()
}

fn texts(found: &c7::SearchResponse) -> Vec<(String, Option<String>, Option<f64>)> {
    found
        .facts
        .iter()
        .map(|f| (f.fact.clone(), f.fact_id.clone(), f.expires_at))
        .collect()
}

/// The ids of the rechecks the store holds.
fn queued(scratch: &Scratch) -> Vec<FactId> {
    Store::open(&scratch.store())
        .unwrap()
        .rechecks(10)
        .unwrap()
        .into_iter()
        .map(|f| f.id)
        .collect()
}

/// When the café's hours of [`hours`] expire.
fn hours_expiry() -> i64 {
    TtlTable::default()
        .expiry(TtlKind::OpeningHours, OBSERVED)
        .unwrap()
}

/// The id of the fact `candidate` is written as over C7: `propose` into the seedbed, then
/// `seedbed_done` of its record.
fn propose(server: &mut Server<Fake>, candidate: &c7::ProposeRequest) -> FactId {
    let body = serde_json::to_value(candidate).unwrap();
    let written: c7::ProposeResponse =
        serde_json::from_value(call(server, "propose", &body)).unwrap();
    assert_eq!(written.status, c7::ProposalStatus::Seeded);
    let items = json!({"items": [{"ref": written.proposal_id}]});
    let mut done: c7::SeedbedDoneResponse =
        serde_json::from_value(call(server, "seedbed_done", &items)).unwrap();
    let answer = done.items.remove(0).outcomes.remove(0);
    assert_eq!(answer.status, c7::ProposalStatus::Accepted);
    FactId(answer.fact_id.unwrap())
}

#[test]
fn an_expired_internet_fact_comes_back_as_a_new_version_from_the_source_and_leaves_rechecks() {
    let scratch = Scratch::new();
    let clock = Fake(Rc::new(Cell::new(NOW)));
    let expires = hours_expiry();
    let looked = expires + 30;
    let source = Scripted::new(vec![Answer::Finds(vec![renewed(looked)])]);
    let (mut server, mut worker) = scratch.daemon(&clock, &source);
    let old = propose(&mut server, &hours());
    assert_eq!(worker.tick(), expires, "the request's wake");
    let before = texts(&search_as_of(&mut server, QUESTION, NOW));
    assert_eq!(before.len(), 1);

    clock.0.set(expires + 60);
    let new_expiry = TtlTable::default()
        .expiry(TtlKind::OpeningHours, looked)
        .unwrap();
    assert_eq!(
        worker.tick(),
        new_expiry,
        "with the queue empty the next deadline is the new version's expiry"
    );
    let original = "Cafe Lumen opening hours are 8 to 18 on weekdays.";
    assert_eq!(source.asked(), vec![original.to_owned()]);
    assert_eq!(queued(&scratch), Vec::<FactId>::new());

    let found = search(&mut server, QUESTION);
    assert_eq!(found.facts.len(), 1, "{found:?}");
    assert_eq!(found.facts[0].fact, RENEWED);
    assert_eq!(found.facts[0].source_ref, RENEWED_URL);
    assert_eq!(found.facts[0].expires_at, Some(new_expiry as f64));
    let store = Store::open(&scratch.store()).unwrap();
    let id = FactId(found.facts[0].fact_id.clone().unwrap());
    let new = store.fact(&id).unwrap().unwrap();
    let previous = store.fact(&old).unwrap().unwrap();
    assert_eq!(
        new.entity, previous.entity,
        "the same entity, not one named Lumen"
    );
    assert_eq!(new.provenance.kind, SourceKind::Internet);
    assert_eq!(new.provenance.observed_at, looked);
    assert_eq!(new.recorded_at, expires + 60);
    assert_eq!(
        texts(&search_as_of(&mut server, QUESTION, NOW)),
        before,
        "an earlier as_of answers as it did"
    );
}

#[test]
fn a_recheck_that_finds_nothing_stays_queued_and_is_asked_again_after_the_recheck_interval() {
    let scratch = Scratch::new();
    let clock = Fake(Rc::new(Cell::new(NOW)));
    let expires = hours_expiry();
    let weak_at = expires + SCHEDULE.recheck_interval;
    let weak = c7::ProposeRequest {
        confidence: Some(0.2),
        ..renewed(weak_at)
    };
    let source = Scripted::new(vec![Answer::Finds(Vec::new()), Answer::Finds(vec![weak])]);
    let (mut server, mut worker) = scratch.daemon(&clock, &source);
    let old = propose(&mut server, &hours());
    assert_eq!(worker.tick(), expires, "the request's wake");
    clock.0.set(expires);
    assert_eq!(
        worker.tick(),
        weak_at,
        "the next batch is the next deadline"
    );
    assert_eq!(source.asked().len(), 1);
    assert_eq!(queued(&scratch), vec![old.clone()]);

    clock.0.set(weak_at - 1);
    assert_eq!(worker.tick(), weak_at);
    assert_eq!(source.asked().len(), 1, "not sooner than the interval");

    clock.0.set(weak_at);
    assert_eq!(worker.tick(), weak_at + SCHEDULE.recheck_interval);
    assert_eq!(source.asked().len(), 2);
    assert_eq!(
        queued(&scratch),
        vec![old],
        "a candidate the edge only queues does not clear the recheck"
    );
    let digest: c7::DigestResponse =
        serde_json::from_value(call(&mut server, "digest", &json!({}))).unwrap();
    assert_eq!(digest.items.len(), 1);
    assert_eq!(search(&mut server, QUESTION).facts.len(), 0);
}

#[test]
fn a_batch_hands_at_most_recheck_batch_facts_to_the_source() {
    let scratch = Scratch::new();
    let clock = Fake(Rc::new(Cell::new(NOW)));
    let source = Scripted::default();
    let schedule = Schedule {
        recheck_batch: 2,
        ..SCHEDULE
    };
    let (mut server, mut worker) = scratch.daemon_with(&clock, &source, schedule);
    for name in ["Cafe Amber", "Cafe Birch", "Cafe Cedar"] {
        let candidate = c7::ProposeRequest {
            fact: format!("{name} opening hours are 8 to 18 on weekdays."),
            entity: Some(name.into()),
            ..hours()
        };
        propose(&mut server, &candidate);
    }
    worker.tick();
    clock.0.set(hours_expiry());
    worker.tick();
    assert_eq!(source.asked().len(), 2);
    assert_eq!(queued(&scratch).len(), 3);
}

/// Set in the child run of the test below, which re-runs it in a process of its own.
const FAILED_RECHECK_CHILD: &str = "ULEI_GARDEN_TEST_FAILED_RECHECK_CHILD";

#[test]
fn a_failed_recheck_is_one_stderr_line_with_its_class_and_stays_queued() {
    let name = "a_failed_recheck_is_one_stderr_line_with_its_class_and_stays_queued";
    if std::env::var_os(FAILED_RECHECK_CHILD).is_none() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(FAILED_RECHECK_CHILD, "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(out.status.success(), "{stderr}");
        assert_eq!(stderr, "garden: second level: unreachable\n");
        return;
    }
    let scratch = Scratch::new();
    let clock = Fake(Rc::new(Cell::new(NOW)));
    let source = Scripted::new(vec![Answer::Fails("unreachable")]);
    let (mut server, mut worker) = scratch.daemon(&clock, &source);
    let old = propose(&mut server, &hours());
    let expires = hours_expiry();
    assert_eq!(worker.tick(), expires, "the request's wake");
    clock.0.set(expires);
    assert_eq!(worker.tick(), expires + SCHEDULE.recheck_interval);
    assert_eq!(source.asked().len(), 1);
    assert_eq!(queued(&scratch), vec![old]);
}
