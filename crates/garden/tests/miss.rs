//! The miss outbox through a search on `ulei_garden::serve::Server`, the worker on an injected
//! clock and a fake C1 inbox on a Unix socket in the test. Keys and queries are synthetic.

#![allow(missing_docs, clippy::unwrap_used, clippy::cast_precision_loss)]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use ulei_garden::api::ApiConfig;
use ulei_garden::edge::EdgeConfig;
use ulei_garden::miss::{self, Inbox, ProducerKey};
use ulei_garden::model::TtlTable;
use ulei_garden::second::Provider;
use ulei_garden::serve::{Server, Settings, TIMEOUT, open};
use ulei_garden::store::{Store, starting_domains};
use ulei_garden::wire::c7;
use ulei_garden::worker::{Clock, Schedule, SystemClock, Wake, Worker};

const NOW: i64 = 1_800_000_000;
/// The worker's start, after every row a search writes at the server's real clock.
fn base() -> i64 {
    SystemClock.now() + 10
}
const KEY: [u8; 32] = [0x5a; 32];
const SCHEDULE: Schedule = Schedule {
    expire_interval: 60,
    maintain_interval: 1_000_000,
};

/// A clock the test sets.
#[derive(Clone)]
struct Fake(Rc<Cell<i64>>);

impl Fake {
    fn at(t: i64) -> Self {
        Self(Rc::new(Cell::new(t)))
    }
}

impl Clock for Fake {
    fn now(&self) -> i64 {
        self.0.get()
    }
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
        let dir = base.join(format!("ulei-garden-miss-{}-{n}", std::process::id()));
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
                .write_all(&KEY)
                .unwrap();
        }
        ProducerKey::read(&path, rustix::process::geteuid().as_raw()).unwrap()
    }

    /// The request side of the daemon on the store, and the worker on its own connection.
    fn daemon(&self, clock: &Fake) -> (Server, Arc<Wake>, Worker<Fake>) {
        let garden = open(&Settings {
            store: self.store(),
            socket: self.0.join("garden.sock"),
            producer_key: self.key(),
            inbox_socket: self.inbox_socket(),
            domains: starting_domains().unwrap(),
            ttl: TtlTable::default(),
            api: ApiConfig::default(),
            schedule: SCHEDULE,
            second_level: Provider::None,
        })
        .unwrap();
        let wake = Arc::new(Wake::default());
        let uid = rustix::process::geteuid().as_raw();
        let server = Server::new(garden, uid, TIMEOUT, Arc::clone(&wake), SystemClock);
        let inbox = Inbox::new(self.inbox_socket(), self.key());
        let worker = Worker::new(
            Store::open(&self.store()).unwrap(),
            SCHEDULE,
            clock.clone(),
            inbox,
            Provider::None.source(),
            EdgeConfig::default(),
        )
        .unwrap();
        (server, wake, worker)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// How the fake inbox answers one connection.
#[derive(Clone, Copy)]
enum Reply {
    /// `200` with the C1 `PublishResponse` of the packet's id.
    Stored,
    /// This status with a C1 `Error`.
    Status(u16),
    /// Reads the request and closes without an answer.
    Hangup,
}

/// A fake C1 inbox at `socket` that answers one connection per reply, then returns the
/// request bodies it read.
fn inbox(socket: &PathBuf, replies: Vec<Reply>) -> JoinHandle<Vec<Value>> {
    let _ = fs::remove_file(socket);
    let listener = UnixListener::bind(socket).unwrap();
    thread::spawn(move || {
        let mut bodies = Vec::new();
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let body = read_request(&mut stream);
            let answer = match reply {
                Reply::Stored => Some((200, json!({"id": body["packet"]["id"]}))),
                Reply::Status(status) => Some((
                    status,
                    json!({"code": "internal", "message": "refused by the test"}),
                )),
                Reply::Hangup => None,
            };
            if let Some((status, json)) = answer {
                let text = json.to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                )
                .unwrap();
            }
            bodies.push(body);
        }
        bodies
    })
}

/// The JSON body of one `POST /v1/publish` request.
fn read_request(stream: &mut UnixStream) -> Value {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head_end, length) = loop {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "request ended early");
        raw.extend_from_slice(&chunk[..n]);
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = std::str::from_utf8(&raw[..end]).unwrap();
            assert!(head.starts_with("POST /v1/publish HTTP/1.1\r\n"), "{head}");
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .unwrap()
                .parse::<usize>()
                .unwrap();
            break (end + 4, length);
        }
    };
    while raw.len() < head_end + length {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "body ended early");
        raw.extend_from_slice(&chunk[..n]);
    }
    serde_json::from_slice(&raw[head_end..]).unwrap()
}

/// True when the `mac` of `packet` verifies under [`KEY`] as the inbox checks it: HMAC-SHA256
/// of the JSON of the other fields with sorted keys and no spaces.
fn mac_verifies(packet: &Value) -> bool {
    let mut fields: BTreeMap<String, Value> = serde_json::from_value(packet.clone()).unwrap();
    let given = fields.remove("mac").unwrap();
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&KEY).unwrap();
    mac.update(serde_json::to_string(&fields).unwrap().as_bytes());
    let expected = mac
        .finalize()
        .into_bytes()
        .iter()
        .fold(String::new(), |mut hex, b| {
            write!(hex, "{b:02x}").unwrap();
            hex
        });
    given.as_str() == Some(expected.as_str())
}

/// The status and JSON body `server` answers to `method` with `body` on a socketpair.
fn call(server: &mut Server, method: &str, body: &Value) -> (u16, Value) {
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
    let status = std::str::from_utf8(&raw[9..12]).unwrap().parse().unwrap();
    (status, serde_json::from_slice(&raw[start..]).unwrap())
}

fn search(server: &mut Server, q: &str) -> c7::SearchResponse {
    let (status, body) = call(server, "search", &json!({ "q": q }));
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body).unwrap()
}

fn unpublished(server: &mut Server) -> u64 {
    let (status, body) = call(server, "health", &json!({}));
    assert_eq!(status, 200, "{body}");
    serde_json::from_value::<c7::Health>(body)
        .unwrap()
        .unpublished_misses
}

#[test]
fn a_search_miss_is_queued_at_once_and_the_worker_publishes_it_signed_as_the_inbox_checks() {
    let scratch = Scratch::new();
    let base = base();
    let clock = Fake::at(base);
    let (mut server, wake, mut worker) = scratch.daemon(&clock);

    let before = SystemClock.now();
    assert_eq!(
        search(&mut server, "opening hours of Cafe Lumen")
            .facts
            .len(),
        0
    );
    let after = SystemClock.now();
    assert!(wake.wait(Duration::ZERO), "the queued miss woke the worker");
    assert_eq!(unpublished(&mut server), 1);

    let fake = inbox(&scratch.inbox_socket(), vec![Reply::Stored]);
    assert_eq!(worker.tick(), base + 1_000_000, "no retry is due");
    let bodies = fake.join().unwrap();
    let packet = &bodies[0]["packet"];
    assert!(mac_verifies(packet), "{packet}");
    assert_eq!(packet["version"], 1);
    assert_eq!(packet["kind"], "garden_miss");
    assert_eq!(packet["trust"], "untrusted-source-data");
    let source_ts = packet["source_ts"].as_f64().unwrap();
    assert!(
        (before as f64..=after as f64).contains(&source_ts),
        "{source_ts}"
    );
    assert_eq!(
        packet["payload"],
        "The garden has nothing for: opening hours of Cafe Lumen"
    );
    let id = packet["id"].as_str().unwrap();
    assert!(id.len() <= 96, "{id}");
    let hash = id.strip_prefix(&format!("garden:miss:{base}:")).unwrap();
    assert!(
        hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{id}"
    );
    assert_eq!(unpublished(&mut server), 0);
}

#[test]
fn a_refused_or_unreachable_inbox_keeps_the_row_and_the_retry_sends_the_same_packet() {
    let scratch = Scratch::new();
    let base = base();
    let clock = Fake::at(base);
    let (mut server, _wake, mut worker) = scratch.daemon(&clock);
    search(&mut server, "tram timetable to the harbour");

    let fake = inbox(&scratch.inbox_socket(), vec![Reply::Status(500)]);
    assert_eq!(worker.tick(), base + 60);
    let refused = fake.join().unwrap();
    assert_eq!(unpublished(&mut server), 1);

    let fake = inbox(&scratch.inbox_socket(), vec![Reply::Hangup]);
    clock.0.set(base + 59);
    assert_eq!(worker.tick(), base + 60, "nothing is sent before the retry");
    clock.0.set(base + 60);
    assert_eq!(worker.tick(), base + 60 + 120);
    let hung_up = fake.join().unwrap();
    assert_eq!(unpublished(&mut server), 1);

    fs::remove_file(scratch.inbox_socket()).unwrap();
    clock.0.set(base + 180);
    assert_eq!(worker.tick(), base + 180 + 240, "no socket at all");
    assert_eq!(unpublished(&mut server), 1);

    let fake = inbox(&scratch.inbox_socket(), vec![Reply::Stored]);
    clock.0.set(base + 420);
    worker.tick();
    let stored = fake.join().unwrap();
    assert_eq!(unpublished(&mut server), 0);
    let id = refused[0]["packet"]["id"].as_str().unwrap();
    assert!(id.starts_with(&format!("garden:miss:{base}:")), "{id}");
    assert_eq!(refused, hung_up, "a retry is the same packet");
    assert_eq!(refused, stored, "a retry is the same packet");
    assert!(mac_verifies(&stored[0]["packet"]));
}

#[test]
fn retries_back_off_one_two_four_and_up_to_sixty_minutes() {
    let scratch = Scratch::new();
    let base = base();
    let clock = Fake::at(base);
    let (mut server, _wake, mut worker) = scratch.daemon(&clock);
    search(&mut server, "pharmacy open late on Sunday");
    let mut gaps = Vec::new();
    let mut at = base;
    for _ in 0..8 {
        clock.0.set(at);
        let next = worker.tick();
        gaps.push(next - at);
        at = next;
    }
    assert_eq!(gaps, [60, 120, 240, 480, 960, 1_920, 3_600, 3_600]);
    assert_eq!(unpublished(&mut server), 1);
}

#[test]
fn a_repeated_query_is_queued_and_published_once_a_day() {
    let scratch = Scratch::new();
    let clock = Fake::at(base());
    let (mut server, wake, mut worker) = scratch.daemon(&clock);
    search(&mut server, "Best bakery in Fogtown?");
    assert!(wake.wait(Duration::ZERO));
    search(&mut server, "best   bakery in fogtown");
    assert!(!wake.wait(Duration::ZERO), "a repeat queues nothing");
    assert_eq!(unpublished(&mut server), 1);

    let fake = inbox(&scratch.inbox_socket(), vec![Reply::Stored]);
    worker.tick();
    assert_eq!(fake.join().unwrap().len(), 1);
    assert_eq!(unpublished(&mut server), 0);

    search(&mut server, "best bakery in Fogtown");
    assert!(
        !wake.wait(Duration::ZERO),
        "a published row still holds it back"
    );
    assert_eq!(unpublished(&mut server), 0);
    let fake = inbox(&scratch.inbox_socket(), vec![Reply::Stored]);
    let mut store = Store::open(&scratch.store()).unwrap();
    let later = SystemClock.now() + miss::COALESCE + 1;
    assert_eq!(
        miss::queue(&mut store, &scratch.key(), "best bakery in Fogtown", later).unwrap(),
        miss::Queued::Added
    );
    clock.0.set(later);
    worker.tick();
    assert_eq!(
        fake.join().unwrap().len(),
        1,
        "a day later it is a new miss"
    );
}

#[test]
fn a_secret_query_is_neither_in_the_outbox_row_nor_in_the_published_packet() {
    let scratch = Scratch::new();
    let clock = Fake::at(base());
    let (mut server, _wake, mut worker) = scratch.daemon(&clock);
    search(&mut server, "password: 7Q4X9Z");
    let fake = inbox(&scratch.inbox_socket(), vec![Reply::Stored]);
    worker.tick();
    let rows: Vec<String> = Store::open(&scratch.store())
        .unwrap()
        .conn()
        .prepare(
            "SELECT query_hash || '|' || payload || '|' || coalesce(event_id, '')
             FROM miss_outbox",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].contains("7Q4X9Z"), "{}", rows[0]);
    let body = fake.join().unwrap()[0].to_string();
    assert!(!body.contains("7Q4X9Z"), "{body}");
    assert!(body.contains("The garden has nothing for a query (not shown)."));
}

#[test]
fn a_pointers_miss_is_not_queued() {
    let scratch = Scratch::new();
    let (mut server, wake, _worker) = scratch.daemon(&Fake::at(base()));
    let (status, body) = call(
        &mut server,
        "pointers",
        &json!({"q": "where is the ferry pier"}),
    );
    assert_eq!(status, 200);
    assert_eq!(body["pointers"], json!([]));
    assert!(!wake.wait(Duration::ZERO));
    assert_eq!(unpublished(&mut server), 0);
}

#[test]
fn over_the_cap_the_oldest_unpublished_row_is_dropped() {
    let mut store = Store::open_in_memory().unwrap();
    let key = ProducerKey::from_bytes(&KEY).unwrap();
    for n in 0..miss::MAX_UNPUBLISHED {
        assert_eq!(
            miss::queue(&mut store, &key, &format!("cello teacher number {n}"), NOW).unwrap(),
            miss::Queued::Added
        );
    }
    assert_eq!(
        miss::queue(&mut store, &key, "cello teacher number 1000", NOW).unwrap(),
        miss::Queued::AddedOverCap
    );
    assert_eq!(
        miss::unpublished(&store).unwrap(),
        miss::MAX_UNPUBLISHED.unsigned_abs()
    );
    assert_eq!(
        miss::queue(&mut store, &key, "cello teacher number 0", NOW).unwrap(),
        miss::Queued::AddedOverCap,
        "the dropped oldest query no longer holds back a new miss"
    );
}

/// The `query_hash` of every outbox row, oldest first.
fn row_hashes(store: &Store) -> Vec<String> {
    store
        .conn()
        .prepare("SELECT query_hash FROM miss_outbox ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn the_same_query_keeps_its_row_and_id_hash_across_a_restart_with_the_same_key() {
    let scratch = Scratch::new();
    let base = base();
    let first_id = {
        let clock = Fake::at(base);
        let (mut server, _wake, mut worker) = scratch.daemon(&clock);
        search(&mut server, "ferry to the north island");
        let fake = inbox(&scratch.inbox_socket(), vec![Reply::Stored]);
        worker.tick();
        fake.join().unwrap()[0]["packet"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    let clock = Fake::at(base);
    let (mut server, wake, mut worker) = scratch.daemon(&clock);
    search(&mut server, "Ferry to the north island?");
    assert!(
        !wake.wait(Duration::ZERO),
        "after the restart the query still has its row"
    );
    let suffix = |id: &str| id.rsplit(':').next().unwrap().to_owned();
    let mut store = Store::open(&scratch.store()).unwrap();
    assert_eq!(row_hashes(&store), [suffix(&first_id)]);

    let later = SystemClock.now() + miss::COALESCE + 1;
    miss::queue(
        &mut store,
        &scratch.key(),
        "ferry to the north island",
        later,
    )
    .unwrap();
    let fake = inbox(&scratch.inbox_socket(), vec![Reply::Stored]);
    clock.0.set(later);
    worker.tick();
    let second_id = fake.join().unwrap()[0]["packet"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(first_id, second_id, "a day later it is a new miss");
    assert_eq!(suffix(&first_id), suffix(&second_id));
}

#[test]
fn a_different_key_gives_a_different_hash_for_the_same_query() {
    let hashes = |key: [u8; 32]| {
        let mut store = Store::open_in_memory().unwrap();
        let key = ProducerKey::from_bytes(&key).unwrap();
        miss::queue(&mut store, &key, "night bus to the airport", NOW).unwrap();
        row_hashes(&store)
    };
    assert_eq!(hashes(KEY), hashes(KEY), "the same key hashes alike");
    assert_ne!(hashes(KEY), hashes([0x3c; 32]));
}

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        write!(out, "{b:02x}").unwrap();
        out
    })
}

#[test]
fn a_pin_query_leaves_no_unkeyed_digest_in_the_row_or_the_published_id() {
    use sha2::Digest as _;
    let scratch = Scratch::new();
    let clock = Fake::at(base());
    let (mut server, _wake, mut worker) = scratch.daemon(&clock);
    let query = "PIN 4821";
    search(&mut server, query);
    let fake = inbox(&scratch.inbox_socket(), vec![Reply::Stored]);
    worker.tick();
    let id = fake.join().unwrap()[0]["packet"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let row: String = Store::open(&scratch.store())
        .unwrap()
        .conn()
        .query_row(
            "SELECT query_hash || '|' || payload || '|' || coalesce(event_id, '')
             FROM miss_outbox",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let fnv1a = |bytes: &[u8]| {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    };
    let normalised = ulei_garden::edge::normalise(query);
    let mut unkeyed = Vec::new();
    for text in [query, normalised.as_str()] {
        unkeyed.push(format!("{:016x}", fnv1a(text.as_bytes())));
        unkeyed.push(hex(&Sha256::digest(text.as_bytes())));
    }
    for digest in &unkeyed {
        assert!(!row.contains(digest.as_str()), "{digest} in {row}");
        assert!(!id.contains(digest.as_str()), "{digest} in {id}");
    }
    assert!(!row.contains("4821") && !id.contains("4821"), "{row} {id}");
}
