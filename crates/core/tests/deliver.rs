//! The delivery loop against a fake C6 engine: the gate, the lock and the publish signal through
//! `ulei-inbox serve` with `inbox.deliver`; each ack-table row, each `inject` error class and
//! the due-time wake through `ulei_core::deliver` with short client timeouts. Every test reads
//! the C1 row back.

mod common;

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::daemon::{Daemon, Started, call, launch, status_field};
use common::{Scratch, hex, sign, unix_now, write_mode};
use serde_json::{Value, json};
use ulei_contracts::generated::c1::PublishResponse;
use ulei_core::c6::Client;
use ulei_core::deliver::{
    self, ENGINE_REACHABLE, ENGINE_UNREACHABLE, Off, Pace, TICK, TICK_FAILED,
};
use ulei_core::inbox::{EventRow, Inbox, RETRY_DELAY, own_uid};
use ulei_core::prompt::model_text;

const INBOX: &str = env!("CARGO_BIN_EXE_ulei-inbox");
const KEY: [u8; 32] = [7; 32];
/// Bounds every wait of the tests and of the fake engine.
const WAIT: Duration = Duration::from_secs(10);
/// Connect and call timeout of the in-process consumer's client.
const CALL: Duration = Duration::from_secs(1);
/// A loop tick that outlasts every wait of the tests, so a wake seen within [`WAIT`] is no tick.
const NO_TICK: Duration = Duration::from_secs(3_600);
const SESSION: &str = "agent:main:telegram:dm:42";
const TURN: &str = "owner:1";

/// How the fake engine answers one call.
#[derive(Clone)]
enum Reply {
    /// `200` with this body.
    Ok(Value),
    /// The status with a C6 `Error` of this code.
    Error(u16, &'static str),
    /// These bytes verbatim.
    Raw(&'static [u8]),
    /// Nothing; the connection stays open until the client gives up.
    Silent,
    /// The connection closes with the request body unread.
    Close,
    /// `200` with this body after the socket path is removed, so the next connect fails.
    OkThenUnlink(Value),
}

/// One call the fake engine read.
struct Call {
    method: String,
    body: Value,
    at: Instant,
}

#[derive(Default)]
struct State {
    script: HashMap<String, VecDeque<Reply>>,
    calls: Vec<Call>,
}

/// A scripted C6 engine on a Unix socket. Each method answers its queued replies in order and
/// repeats the last one; a method without a script answers `404 not_found`.
struct FakeC6 {
    path: PathBuf,
    shared: Arc<(Mutex<State>, Condvar)>,
}

impl FakeC6 {
    fn start(path: &Path) -> Self {
        let listener = UnixListener::bind(path).expect("bind the fake engine");
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let state = Arc::clone(&shared);
        let socket = path.to_owned();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let state = Arc::clone(&state);
                let socket = socket.clone();
                thread::spawn(move || answer(stream, &state, &socket));
            }
        });
        Self {
            path: path.to_owned(),
            shared,
        }
    }

    /// Queues `reply` for `method`.
    fn script(&self, method: &str, reply: Reply) {
        let mut state = self.shared.0.lock().expect("fake engine");
        state
            .script
            .entry(method.to_owned())
            .or_default()
            .push_back(reply);
    }

    /// Replaces the replies of `method` with `reply`.
    fn set(&self, method: &str, reply: Reply) {
        let mut state = self.shared.0.lock().expect("fake engine");
        state
            .script
            .insert(method.to_owned(), VecDeque::from([reply]));
    }

    /// The bodies and arrival times of the calls of `method` so far.
    fn calls(&self, method: &str) -> Vec<(Value, Instant)> {
        let state = self.shared.0.lock().expect("fake engine");
        state
            .calls
            .iter()
            .filter(|c| c.method == method)
            .map(|c| (c.body.clone(), c.at))
            .collect()
    }

    /// Every call so far, of any method.
    fn count(&self) -> usize {
        self.shared.0.lock().expect("fake engine").calls.len()
    }

    /// Waits until `n` calls of `method` arrived and returns them.
    fn wait_calls(&self, method: &str, n: usize) -> Vec<(Value, Instant)> {
        let (state, changed) = &*self.shared;
        let deadline = Instant::now() + WAIT;
        let mut guard = state.lock().expect("fake engine");
        while guard.calls.iter().filter(|c| c.method == method).count() < n {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the engine never saw {n} {method} calls");
            guard = changed.wait_timeout(guard, left).expect("fake engine").0;
        }
        drop(guard);
        self.calls(method)
    }
}

impl Drop for FakeC6 {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn answer(mut stream: UnixStream, shared: &(Mutex<State>, Condvar), socket: &Path) {
    let _ = stream.set_read_timeout(Some(WAIT));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let method = head
        .split(' ')
        .nth(1)
        .and_then(|target| target.strip_prefix("/v1/"))
        .expect("C6 request line")
        .to_owned();
    let length: usize = head
        .split("\r\n")
        .find_map(|l| l.strip_prefix("Content-Length: ")?.parse().ok())
        .expect("Content-Length");
    let reply = {
        let mut state = shared.0.lock().expect("fake engine");
        let queue = state.script.get_mut(&method);
        match queue {
            Some(queue) if queue.len() > 1 => queue.pop_front(),
            Some(queue) => queue.front().cloned(),
            None => None,
        }
    };
    let body = if matches!(reply, Some(Reply::Close)) {
        Value::Null
    } else {
        while buf.len() < end + length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        serde_json::from_slice(&buf[end..]).expect("C6 request body")
    };
    {
        let (state, changed) = shared;
        state.lock().expect("fake engine").calls.push(Call {
            method,
            body,
            at: Instant::now(),
        });
        changed.notify_all();
    }
    let bytes = match reply {
        Some(Reply::Ok(body)) => response(200, &body),
        Some(Reply::Error(status, code)) => response(
            status,
            &json!({"code": code, "message": "scripted refusal"}),
        ),
        Some(Reply::Raw(bytes)) => bytes.to_vec(),
        Some(Reply::Silent) => {
            while matches!(stream.read(&mut chunk), Ok(n) if n > 0) {}
            return;
        }
        Some(Reply::Close) => return,
        Some(Reply::OkThenUnlink(body)) => {
            fs::remove_file(socket).expect("unlink the engine socket");
            response(200, &body)
        }
        None => response(404, &json!({"code": "not_found", "message": "no script"})),
    };
    let _ = stream.write_all(&bytes);
}

fn response(status: u16, body: &Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} Scripted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn idle(idle: bool) -> Reply {
    Reply::Ok(json!({"idle": idle, "channel": true}))
}

fn accepted() -> Reply {
    Reply::Ok(json!({"accepted": true, "session_id": SESSION, "turn": TURN}))
}

fn not_accepted() -> Reply {
    Reply::Ok(json!({"accepted": false, "retry_after_s": 15}))
}

/// The `status` answer whose turn is `state`, with `delivery` when given.
fn status(state: &str, delivery: Option<&str>) -> Reply {
    let mut turn = json!({"turn": TURN, "state": state});
    match state {
        "ended" => {
            turn["outcome"] = json!("completed");
            turn["ended"] = json!(2.0);
        }
        "dropped" => turn["ended"] = json!(2.0),
        _ => {}
    }
    if let Some(delivery) = delivery {
        turn["delivery"] = json!(delivery);
    }
    Reply::Ok(json!({
        "session": SESSION, "running": false, "since": 1.0, "queue_depth": 0, "route": "main",
        "turn": turn,
    }))
}

/// The `status` answer of a `quota_exhausted` turn the channel reported `no_answer`, with
/// `tools_ran` and `resets_at` when given.
fn quota(tools_ran: Option<bool>, resets_at: Option<u64>) -> Reply {
    let mut turn = json!({
        "turn": TURN, "state": "ended", "outcome": "quota_exhausted", "ended": 2.0,
        "delivery": "no_answer",
    });
    if let Some(tools_ran) = tools_ran {
        turn["tools_ran"] = json!(tools_ran);
    }
    if let Some(resets_at) = resets_at {
        turn["resets_at"] = json!(resets_at);
    }
    Reply::Ok(json!({
        "session": SESSION, "running": false, "since": 1.0, "queue_depth": 0, "route": "main",
        "turn": turn,
    }))
}

/// A signed packet of `kind` with id `id` and `payload`.
fn packet(id: &str, kind: &str, payload: &str) -> Value {
    sign(
        &KEY,
        &json!({
            "version": 1, "kind": kind, "id": id, "payload": payload,
            "source_ts": 1_763_290_000, "trust": "untrusted-source-data",
        }),
    )
}

/// An owner-only root with the test key and `ENABLED`.
fn enabled_root(scratch: &Scratch) -> PathBuf {
    let root = scratch.root_with_key(&KEY);
    write_mode(&root.join(deliver::ENABLED), b"", 0o600);
    root
}

/// Polls the row of `id` through `inbox` until `done` holds, at most [`WAIT`].
fn wait_row(inbox: &Inbox, id: &str, done: impl Fn(&EventRow) -> bool) -> EventRow {
    let deadline = Instant::now() + WAIT;
    loop {
        let row = inbox.get(id).expect("get").expect("row");
        if done(&row) {
            return row;
        }
        assert!(Instant::now() < deadline, "row {id} stayed {row:?}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn settled(row: &EventRow) -> bool {
    !matches!(row.status.as_str(), "pending" | "leased" | "processing")
}

/// A consumer on its own thread over an enabled root and a fake engine; the test publishes
/// through its own store connection and signals the loop as the server does.
struct Run {
    inbox: Inbox,
    engine: FakeC6,
    published: Option<SyncSender<()>>,
    thread: Option<JoinHandle<()>>,
    scratch: Scratch,
}

impl Run {
    fn start() -> Self {
        Self::with(own_uid().expect("uid"), Pace::DEFAULT)
    }

    /// A consumer with `pace` whose client requires the engine to run as `uid`.
    fn with(uid: u32, pace: Pace) -> Self {
        Self::polling(uid, pace, None)
    }

    /// [`Run::with`], polling every `poll` when given.
    fn polling(uid: u32, pace: Pace, poll: Option<Duration>) -> Self {
        let scratch = Scratch::new();
        let root = enabled_root(&scratch);
        let engine = FakeC6::start(&scratch.socket());
        let inbox = Inbox::open(&root).expect("inbox");
        let client = Client::with_limits(scratch.socket(), CALL, CALL, uid);
        let mut consumer = deliver::open(&root, client, pace)
            .expect("open")
            .expect("consumer");
        if let Some(poll) = poll {
            consumer = consumer.polling(poll);
        }
        let (published, wake) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || consumer.run(&wake));
        Self {
            inbox,
            engine,
            published: Some(published),
            thread: Some(thread),
            scratch,
        }
    }

    fn signal(&self) {
        let _ = self.published.as_ref().expect("sender").try_send(());
    }

    /// Publishes event `id` of `kind` and signals the loop; the time of the publish.
    fn publish(&mut self, id: &str, kind: &str, payload: &str) -> f64 {
        let at = unix_now();
        self.inbox
            .publish(&packet(id, kind, payload), at)
            .expect("publish");
        self.signal();
        at
    }

    fn row(&self, id: &str) -> EventRow {
        self.inbox.get(id).expect("get").expect("row")
    }

    fn wait_row(&self, id: &str, done: impl Fn(&EventRow) -> bool) -> EventRow {
        wait_row(&self.inbox, id, done)
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        drop(self.published.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// `(status, outcome, attempts)` of `row`.
fn state(row: &EventRow) -> (&str, Option<&str>, i64) {
    (row.status.as_str(), row.outcome.as_deref(), row.attempts)
}

/// Publishes one event of `kind`, has the engine accept it and answer `status` with `report`,
/// and returns its settled row with the time of the publish.
fn settle(kind: &str, report: Reply) -> (EventRow, f64) {
    let mut run = Run::start();
    run.engine.script("is_idle", idle(true));
    run.engine.script("inject", accepted());
    run.engine.script("status", report);
    let at = run.publish("evt:1", kind, "The nightly backup finished.");
    (run.wait_row("evt:1", settled), at)
}

/// Asserts that `row` went back to `pending` for the retry delay after one claim.
fn assert_retried(row: &EventRow, at: f64) {
    assert_eq!(state(row), ("pending", None, 1));
    assert_eq!(row.lease, None);
    assert!(
        (at + 15.0..at + 15.0 + WAIT.as_secs_f64()).contains(&row.available),
        "available {} after publish {at}",
        row.available
    );
}

#[test]
fn a_sent_delivery_acks_success() {
    let (row, _) = settle("task_result", status("ended", Some("sent")));
    assert_eq!(state(&row), ("done", Some("success"), 1));
    assert_eq!(row.available, 0.0);
    assert_eq!(row.session_id.as_deref(), Some(SESSION));
}

#[test]
fn no_answer_and_a_failed_send_ack_uncertain() {
    for delivery in ["no_answer", "send_failed"] {
        let (row, _) = settle("task_result", status("ended", Some(delivery)));
        assert_eq!(
            state(&row),
            ("uncertain", Some("uncertain"), 1),
            "{delivery}"
        );
        assert_eq!(row.available, 0.0);
    }
}

#[test]
fn a_step_free_quota_exhausted_turn_acks_deferred_until_its_reset() {
    let (row, _) = settle("task_result", quota(Some(false), Some(1_893_474_000)));
    assert_eq!(state(&row), ("deferred", Some("deferred"), 1));
    assert_eq!(row.available, 1_893_474_000.0);
}

#[test]
fn a_step_free_quota_exhausted_turn_without_a_reset_acks_deferred_for_an_hour() {
    let (row, at) = settle("task_result", quota(Some(false), None));
    assert_eq!(state(&row), ("deferred", Some("deferred"), 1));
    assert!(
        (at + 3600.0..at + 3600.0 + WAIT.as_secs_f64()).contains(&row.available),
        "available {} after publish {at}",
        row.available
    );
}

#[test]
fn a_quota_exhausted_turn_that_ran_a_tool_or_lacks_the_facts_acks_uncertain() {
    for (tools_ran, resets_at) in [
        (Some(true), Some(1_893_474_000)),
        (Some(true), None),
        (None, Some(1_893_474_000)),
        (None, None),
    ] {
        let (row, _) = settle("task_result", quota(tools_ran, resets_at));
        assert_eq!(
            state(&row),
            ("uncertain", Some("uncertain"), 1),
            "{tools_ran:?} {resets_at:?}"
        );
        assert_eq!(row.available, 0.0);
    }
}

#[test]
fn silent_done_of_a_task_acks_silent() {
    let (row, _) = settle("task_result", status("ended", Some("silent_done")));
    assert_eq!(state(&row), ("done", Some("silent"), 1));
    assert_eq!(row.available, 0.0);
}

#[test]
fn silent_of_a_task_acks_deferred_for_an_hour() {
    let (row, at) = settle("task_result", status("ended", Some("silent")));
    assert_eq!(state(&row), ("deferred", Some("deferred"), 1));
    assert!(
        (at + 3600.0..at + 3600.0 + WAIT.as_secs_f64()).contains(&row.available),
        "available {} after publish {at}",
        row.available
    );
}

#[test]
fn either_silence_of_a_weekly_event_acks_silent() {
    for delivery in ["silent", "silent_done"] {
        let (row, _) = settle("weekly", status("ended", Some(delivery)));
        assert_eq!(state(&row), ("done", Some("silent"), 1), "{delivery}");
        assert_eq!(row.available, 0.0);
    }
}

#[test]
fn a_dropped_or_unknown_turn_acks_uncertain() {
    for turn in ["dropped", "unknown"] {
        let (row, _) = settle("task_result", status(turn, None));
        assert_eq!(state(&row), ("uncertain", Some("uncertain"), 1), "{turn}");
        assert_eq!(row.available, 0.0);
    }
}

#[test]
fn an_inject_not_accepted_is_retried_after_15_s() {
    let mut run = Run::start();
    run.engine.script("is_idle", idle(true));
    run.engine.script("inject", not_accepted());
    let at = run.publish("evt:1", "task_result", "Card finished.");
    let row = run.wait_row("evt:1", |r| r.status == "pending" && r.attempts == 1);
    assert_retried(&row, at);
    assert_eq!(run.engine.calls("inject").len(), 1);
}

#[test]
fn the_turn_in_flight_is_renewed_every_tick_until_it_ends_and_then_acked() {
    let mut run = Run::start();
    run.engine.script("is_idle", idle(true));
    run.engine.script("inject", accepted());
    run.engine.script("status", status("running", None));
    run.engine.script("status", status("running", None));
    run.engine.script("status", status("ended", Some("sent")));
    run.publish("evt:1", "task_result", "Card finished.");
    let first = run.engine.wait_calls("status", 1);
    let renewed = run.row("evt:1").lease_until.expect("lease_until");
    run.engine.wait_calls("status", 2);
    assert!(run.row("evt:1").lease_until.expect("lease_until") > renewed);
    let row = run.wait_row("evt:1", settled);
    assert_eq!(state(&row), ("done", Some("success"), 1));
    let calls = run.engine.calls("status");
    assert_eq!(calls.len(), 3);
    for pair in calls.windows(2) {
        assert!(
            pair[1].1 - pair[0].1 >= TICK,
            "status calls closer than a tick"
        );
    }
    let asked = &first[0].0;
    assert_eq!(asked, &json!({"session": SESSION, "turn": TURN}));
    let before = run.engine.count();
    thread::sleep(TICK + Duration::from_millis(500));
    assert_eq!(
        run.engine.count(),
        before,
        "the idle loop called the engine"
    );
}

#[test]
fn a_turn_ended_without_delivery_is_no_longer_renewed_and_expires_uncertain() {
    let mut run = Run::start();
    run.engine.script("is_idle", idle(true));
    run.engine.script("inject", accepted());
    run.engine.script("status", status("ended", None));
    run.publish("evt:1", "task_result", "Card finished.");
    run.engine.wait_calls("status", 1);
    let lease_until = run.row("evt:1").lease_until.expect("lease_until");
    run.engine.wait_calls("status", 2);
    let row = run.row("evt:1");
    assert_eq!(row.lease_until, Some(lease_until));
    assert_eq!(state(&row), ("processing", None, 1));
    run.inbox.recover(lease_until + 1.0).expect("recover");
    let row = run.row("evt:1");
    assert_eq!(state(&row), ("uncertain", Some("expired-processing"), 1));
}

#[test]
fn a_busy_engine_gets_no_claim_until_it_is_idle() {
    let mut run = Run::start();
    run.engine.script("is_idle", idle(false));
    run.engine.script("is_idle", idle(false));
    run.engine.script("is_idle", idle(true));
    run.engine.script("inject", not_accepted());
    run.publish("evt:1", "task_result", "Card finished.");
    let checks = run.engine.wait_calls("is_idle", 2);
    assert!(
        checks[1].1 - checks[0].1 >= TICK,
        "busy check repeated within a tick"
    );
    let row = run.row("evt:1");
    assert_eq!(state(&row), ("pending", None, 0));
    let row = run.wait_row("evt:1", |r| r.attempts == 1 && r.status == "pending");
    assert_eq!(run.engine.calls("inject").len(), 1);
    assert_eq!(row.lease, None);
}

#[test]
fn a_failed_idle_check_gets_no_claim() {
    let mut run = Run::start();
    run.engine.script("is_idle", Reply::Error(500, "internal"));
    run.publish("evt:1", "task_result", "Card finished.");
    run.engine.wait_calls("is_idle", 2);
    assert_eq!(state(&run.row("evt:1")), ("pending", None, 0));
    assert_eq!(run.engine.calls("inject").len(), 0);
}

#[test]
fn an_engine_of_another_uid_gets_no_claim() {
    let mut run = Run::with(own_uid().expect("uid").wrapping_add(1), Pace::DEFAULT);
    run.engine.script("is_idle", idle(true));
    run.publish("evt:1", "task_result", "Card finished.");
    thread::sleep(TICK + Duration::from_millis(500));
    assert_eq!(state(&run.row("evt:1")), ("pending", None, 0));
    assert_eq!(run.engine.count(), 0);
}

/// `inject` answers `first`, the loop retries the event, and nothing else is injected.
fn assert_inject_retried(first: Reply) {
    let mut run = Run::start();
    run.engine.script("is_idle", idle(true));
    run.engine.script("inject", first);
    let at = run.publish("evt:1", "task_result", "Card finished.");
    let row = run.wait_row("evt:1", |r| r.status == "pending" && r.attempts == 1);
    assert_retried(&row, at);
    thread::sleep(TICK + Duration::from_millis(500));
    assert_eq!(run.engine.calls("inject").len(), 1);
}

#[test]
fn an_internal_refusal_of_inject_is_retried() {
    assert_inject_retried(Reply::Error(500, "internal"));
}

#[test]
fn a_client_refusal_of_inject_is_retried() {
    assert_inject_retried(Reply::Error(400, "bad_request"));
}

#[test]
fn an_inject_whose_connect_fails_is_retried() {
    let mut run = Run::start();
    run.engine.script(
        "is_idle",
        Reply::OkThenUnlink(json!({"idle": true, "channel": true})),
    );
    let at = run.publish("evt:1", "task_result", "Card finished.");
    let row = run.wait_row("evt:1", |r| r.status == "pending" && r.attempts == 1);
    assert_retried(&row, at);
    assert_eq!(run.engine.calls("inject").len(), 0);
}

#[test]
fn an_inject_over_the_body_limit_is_retried_without_a_call() {
    let run = Run::start();
    run.engine.script("is_idle", idle(true));
    run.engine.script("inject", accepted());
    // Only a row the store never checked has a payload this large.
    let packet = json!({
        "version": 1, "kind": "task_result", "id": "evt:1", "payload": "\u{1}".repeat(100_000),
        "source_ts": 1, "trust": "untrusted-source-data", "mac": "0".repeat(64),
    });
    let db = rusqlite::Connection::open(run.scratch.root().join("events.sqlite3")).expect("db");
    let at = unix_now();
    db.execute(
        "INSERT INTO events(id, packet, digest, created) VALUES ('evt:1', ?1, ?2, ?3)",
        rusqlite::params![packet.to_string().into_bytes(), hex(&[0; 32]), at],
    )
    .expect("insert");
    run.signal();
    let row = run.wait_row("evt:1", |r| r.status == "pending" && r.attempts == 1);
    assert_retried(&row, at);
    assert_eq!(run.engine.calls("inject").len(), 0);
}

/// `inject` answers `first` and then accepts: the loop renews the lease and replays the same
/// request, and the event settles from the replay's turn.
fn assert_inject_replayed(first: Reply, payload: &str) {
    let mut run = Run::start();
    run.engine.script("is_idle", idle(true));
    run.engine.script("inject", first);
    run.engine.script("inject", accepted());
    run.engine.script("status", status("ended", Some("sent")));
    run.publish("evt:1", "task_result", payload);
    let injects = run.engine.wait_calls("inject", 2);
    let row = run.wait_row("evt:1", settled);
    assert_eq!(state(&row), ("done", Some("success"), 1));
    let replay = &injects[1].0;
    assert_eq!(replay["event_id"], "evt:1");
    assert_eq!(replay["attempt"], 1);
    assert_eq!(replay["lease"], json!(row.lease));
    assert_eq!(replay["prompt"], model_text(&row).expect("model text"));
    if !injects[0].0.is_null() {
        assert_eq!(&injects[0].0, replay);
    }
    assert!(injects[1].1 - injects[0].1 >= TICK);
}

#[test]
fn an_inject_cut_after_the_connect_is_replayed() {
    // Larger than the socket buffer, so the request is still being written when it is cut.
    assert_inject_replayed(Reply::Close, &"\u{1}".repeat(65_536));
}

#[test]
fn an_inject_without_an_answer_in_time_is_replayed() {
    assert_inject_replayed(Reply::Silent, "Card finished.");
}

#[test]
fn a_malformed_inject_answer_is_replayed() {
    assert_inject_replayed(Reply::Raw(b"HTTP/1.1 200 OK\r\n\r\n"), "Card finished.");
}

#[test]
fn an_invalid_inject_answer_is_replayed() {
    assert_inject_replayed(Reply::Ok(json!({"accepted": true})), "Card finished.");
}

#[test]
fn an_expiring_lease_wakes_the_loop_without_a_publish() {
    let pace = Pace {
        tick: NO_TICK,
        ..Pace::DEFAULT
    };
    let mut run = Run::with(own_uid().expect("uid"), pace);
    run.engine.script("is_idle", idle(true));
    run.engine.script("inject", accepted());
    run.engine.script("status", status("running", None));
    let at = unix_now();
    run.inbox
        .publish(&packet("evt:1", "task_result", "Card finished."), at)
        .expect("publish");
    let leased = run.inbox.claim(at, 1.0).expect("claim").expect("row");
    run.signal();
    let injects = run.engine.wait_calls("inject", 1);
    let woke = unix_now() - (injects[0].1.elapsed().as_secs_f64());
    let lease_until = leased.lease_until.expect("lease_until");
    assert!(woke >= lease_until, "injected before the lease expired");
    assert_eq!(injects[0].0["attempt"], 2);
}

#[test]
fn a_retried_event_is_offered_again_when_it_is_due_without_a_publish() {
    let mut run = Run::start();
    run.engine.script("is_idle", idle(true));
    run.engine.script("inject", accepted());
    run.engine.script("status", status("running", None));
    let at = unix_now();
    run.inbox
        .publish(&packet("evt:1", "task_result", "Card finished."), at)
        .expect("publish");
    let row = run.inbox.claim(at, 120.0).expect("claim").expect("row");
    let lease = row.lease.expect("lease");
    assert!(run.inbox.retry("evt:1", &lease, at, 1.0).expect("retry"));
    run.signal();
    let injects = run.engine.wait_calls("inject", 1);
    let woke = unix_now() - (injects[0].1.elapsed().as_secs_f64());
    assert!(woke >= at + 1.0, "injected before the event was due");
    assert!(
        woke < at + 2.0,
        "woke {} s after it was due",
        woke - at - 1.0
    );
}

#[test]
fn without_a_watch_the_loop_polls_for_rows_written_past_the_socket() {
    let pace = Pace {
        tick: NO_TICK,
        ..Pace::DEFAULT
    };
    let poll = Duration::from_millis(300);
    let mut run = Run::polling(own_uid().expect("uid"), pace, Some(poll));
    run.engine.script("is_idle", idle(true));
    run.engine.script("inject", accepted());
    run.engine.script("status", status("running", None));
    // Let the loop settle into its wait before a writer that sends no publish commits.
    thread::sleep(Duration::from_millis(100));
    let written = Instant::now();
    run.inbox
        .publish(
            &packet("evt:1", "task_result", "Card finished."),
            unix_now(),
        )
        .expect("publish");
    let injects = run.engine.wait_calls("inject", 1);
    assert!(
        injects[0].1 - written < poll + Duration::from_millis(500),
        "taken {:?} after the write",
        injects[0].1 - written
    );
}

#[test]
fn a_watched_root_wakes_the_loop_for_rows_written_past_the_socket() {
    let pace = Pace {
        tick: NO_TICK,
        ..Pace::DEFAULT
    };
    let mut run = Run::with(own_uid().expect("uid"), pace);
    run.engine.script("is_idle", idle(true));
    run.engine.script("inject", accepted());
    run.engine.script("status", status("running", None));
    let watch = deliver::watch(&run.scratch.root()).expect("watch");
    let wake = run.published.clone().expect("sender");
    thread::spawn(move || watch.run(&wake));
    thread::sleep(Duration::from_millis(100));
    run.inbox
        .publish(
            &packet("evt:1", "task_result", "Card finished."),
            unix_now(),
        )
        .expect("publish");
    let injects = run.engine.wait_calls("inject", 1);
    assert_eq!(injects[0].0["event_id"], "evt:1");
    // The watch holds a sender until its next event, so the loop is left to the process exit.
    drop(run.thread.take());
}

/// `ulei-inbox serve <tail>` on `<root>`, `<scratch>/i` and the engine at `<scratch>/s`.
fn start(scratch: &Scratch, root: &Path, tail: &[&str]) -> Started {
    launch(
        INBOX,
        &[],
        root,
        &scratch.0.join("i"),
        Some(&scratch.socket()),
        tail,
    )
}

/// [`start`] without a tail, once ready.
fn daemon(scratch: &Scratch, root: &Path) -> Daemon {
    match start(scratch, root, &[]) {
        Started::Ready(daemon) => daemon,
        Started::Exited(status, stderr) => panic!("daemon exited {status}: {stderr}"),
    }
}

/// Publishes event `id` of `kind` through the daemon's C1 socket.
fn publish_c1(daemon: &Daemon, id: &str, kind: &str, payload: &str) {
    let body = json!({"packet": packet(id, kind, payload)});
    let out: PublishResponse = call(&daemon.socket, "publish", &body, 200);
    assert_eq!(out.id, id);
}

fn threads(daemon: &Daemon) -> String {
    status_field(daemon.child.id(), "Threads:")
}

#[test]
fn without_enabled_the_daemon_serves_c1_alone() {
    let scratch = Scratch::new();
    let root = scratch.root_with_key(&KEY);
    let engine = FakeC6::start(&scratch.socket());
    engine.script("is_idle", idle(true));
    let daemon = daemon(&scratch, &root);
    assert_eq!(threads(&daemon), "1");
    publish_c1(&daemon, "evt:1", "task_result", "Card finished.");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(engine.count(), 0);
    assert!(!root.join(deliver::CONSUMER_LOCK).exists());
    assert_eq!(daemon.stderr(), format!("{}\n", Off::NotEnabled.line()));
}

/// `consumer.lock` of `root`, locked by this test process.
fn hold_lock(root: &Path) -> File {
    let lock = OpenOptions::new()
        .append(true)
        .create(true)
        .open(root.join(deliver::CONSUMER_LOCK))
        .expect("consumer.lock");
    lock.try_lock().expect("lock");
    lock
}

#[test]
fn a_held_consumer_lock_leaves_the_daemon_serving_c1_alone() {
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let engine = FakeC6::start(&scratch.socket());
    engine.script("is_idle", idle(true));
    let _held = hold_lock(&root);
    let daemon = daemon(&scratch, &root);
    assert_eq!(threads(&daemon), "1");
    publish_c1(&daemon, "evt:1", "task_result", "Card finished.");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(engine.count(), 0);
    assert_eq!(daemon.stderr(), format!("{}\n", Off::Locked.line()));
}

#[test]
fn the_consumer_holds_an_owner_only_lock_on_a_deliver_thread() {
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let _engine = FakeC6::start(&scratch.socket());
    let daemon = daemon(&scratch, &root);
    assert_eq!(threads(&daemon), "3");
    let path = root.join(deliver::CONSUMER_LOCK);
    let mode = fs::metadata(&path).expect("lock").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let lock = File::open(&path).expect("consumer.lock");
    assert!(matches!(lock.try_lock(), Err(TryLockError::WouldBlock)));
    assert_eq!(daemon.stderr(), "");
}

#[test]
fn a_published_event_is_injected_at_once_with_its_model_text() {
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let engine = FakeC6::start(&scratch.socket());
    engine.script("is_idle", idle(true));
    engine.script("inject", accepted());
    engine.script("status", status("running", None));
    let daemon = daemon(&scratch, &root);
    let published = Instant::now();
    publish_c1(&daemon, "evt:1", "task_result", "Card finished.");
    let injects = engine.wait_calls("inject", 1);
    assert!(injects[0].1 - published < TICK, "offered only after a tick");
    let inbox = Inbox::open(&root).expect("inbox");
    let row = wait_row(&inbox, "evt:1", |r| r.status == "processing");
    assert_eq!(row.session_id.as_deref(), Some(SESSION));
    assert_eq!(
        injects[0].0,
        json!({
            "event_id": "evt:1", "kind": "task_result", "prompt": model_text(&row).expect("text"),
            "lease": row.lease, "attempt": 1,
        })
    );
}

#[test]
fn without_enabled_the_turn_in_flight_is_acked_and_nothing_new_is_claimed() {
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let engine = FakeC6::start(&scratch.socket());
    engine.script("is_idle", idle(true));
    engine.script("inject", accepted());
    engine.script("status", status("running", None));
    let daemon = daemon(&scratch, &root);
    let inbox = Inbox::open(&root).expect("inbox");
    publish_c1(&daemon, "evt:1", "task_result", "Card finished.");
    wait_row(&inbox, "evt:1", |r| r.status == "processing");
    fs::remove_file(root.join(deliver::ENABLED)).expect("remove ENABLED");
    publish_c1(&daemon, "evt:2", "task_result", "Second card finished.");
    engine.set("status", status("ended", Some("sent")));
    let row = wait_row(&inbox, "evt:1", settled);
    assert_eq!(state(&row), ("done", Some("success"), 1));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        state(&inbox.get("evt:2").expect("get").expect("row")),
        ("pending", None, 0)
    );
    assert_eq!(engine.calls("inject").len(), 1);
    write_mode(&root.join(deliver::ENABLED), b"", 0o600);
    publish_c1(&daemon, "evt:3", "task_result", "Third card finished.");
    let injects = engine.wait_calls("inject", 2);
    assert_eq!(injects[1].0["event_id"], "evt:2");
}

#[test]
fn a_row_written_past_the_socket_is_injected_without_a_publish() {
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let engine = FakeC6::start(&scratch.socket());
    engine.script("is_idle", idle(true));
    engine.script("inject", accepted());
    engine.script("status", status("running", None));
    let daemon = daemon(&scratch, &root);
    // A writer of the library: its own connection to the root's database and no C1 publish.
    let mut inbox = Inbox::open(&root).expect("inbox");
    let written = Instant::now();
    inbox
        .publish(
            &packet("evt:1", "task_result", "Card finished."),
            unix_now(),
        )
        .expect("publish");
    let injects = engine.wait_calls("inject", 1);
    assert!(
        injects[0].1 - written < TICK,
        "taken {:?} after the write",
        injects[0].1 - written
    );
    wait_row(&inbox, "evt:1", |r| r.status == "processing");
    assert_eq!(daemon.stderr(), "");
}

#[test]
fn an_unreachable_engine_is_one_line_until_it_answers_again() {
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let daemon = match start(&scratch, &root, &["--deliver-tick-ms", "50"]) {
        Started::Ready(daemon) => daemon,
        Started::Exited(status, stderr) => panic!("daemon exited {status}: {stderr}"),
    };
    publish_c1(&daemon, "evt:1", "task_result", "Card finished.");
    // Many ticks against no engine socket.
    thread::sleep(Duration::from_millis(600));
    let engine = FakeC6::start(&scratch.socket());
    engine.script("is_idle", idle(false));
    engine.wait_calls("is_idle", 3);
    assert_eq!(
        daemon.stderr(),
        format!("{ENGINE_UNREACHABLE}connect: entity not found\n{ENGINE_REACHABLE}\n")
    );
}

#[test]
fn a_failed_step_writes_only_the_fixed_line() {
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let engine = FakeC6::start(&scratch.socket());
    engine.script("is_idle", idle(true));
    engine.script("inject", Reply::Error(500, "internal"));
    let daemon = daemon(&scratch, &root);
    publish_c1(&daemon, "evt:1", "task_result", "Synthetic payload text.");
    let inbox = Inbox::open(&root).expect("inbox");
    wait_row(&inbox, "evt:1", |r| {
        r.status == "pending" && r.attempts == 1
    });
    thread::sleep(Duration::from_millis(300));
    assert_eq!(daemon.stderr(), format!("{TICK_FAILED}\n"));
}

#[test]
fn a_processing_row_a_killed_consumer_left_expires_uncertain_without_a_publish() {
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let engine = FakeC6::start(&scratch.socket());
    engine.script("is_idle", idle(true));
    engine.script("inject", accepted());
    engine.script("status", status("running", None));
    let killed = daemon(&scratch, &root);
    publish_c1(&killed, "evt:1", "task_result", "Card finished.");
    let mut inbox = Inbox::open(&root).expect("inbox");
    wait_row(&inbox, "evt:1", |r| r.status == "processing");
    drop(killed);
    let calls = engine.count();
    let row = inbox.get("evt:1").expect("get").expect("row");
    // The killed consumer's last renew, short enough to wait out.
    let at = unix_now();
    assert!(
        inbox
            .renew("evt:1", row.lease.as_deref().expect("lease"), at, 1.0)
            .expect("renew")
    );
    let _restarted = daemon(&scratch, &root);
    let row = wait_row(&inbox, "evt:1", settled);
    let seen = unix_now();
    assert_eq!(state(&row), ("uncertain", Some("expired-processing"), 1));
    assert!(seen >= at + 1.0, "expired before its lease");
    assert!(
        seen < at + 2.0,
        "expired {} s after its lease",
        seen - at - 1.0
    );
    assert_eq!(
        engine.count(),
        calls,
        "the restarted loop called the engine"
    );
}

#[test]
fn each_pace_flag_takes_its_range_and_only_with_inbox_deliver() {
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let _engine = FakeC6::start(&scratch.socket());
    let (refused, usage) = match start(&scratch, &root, &["--unknown", "x"]) {
        Started::Exited(status, stderr) => (status, stderr),
        Started::Ready(_) => panic!("serve with an unknown flag started"),
    };
    let refused = refused.code();
    let flags = [
        ("--deliver-tick-ms", ["10", "2000"], ["9", "2001", "x"]),
        (
            "--lease-ttl-ms",
            ["4000", "120000"],
            ["3999", "120001", "x"],
        ),
        ("--retry-delay-ms", ["100", "15000"], ["99", "15001", "x"]),
    ];
    for (flag, edges, outside) in flags {
        for ms in edges {
            match start(&scratch, &root, &[flag, ms]) {
                Started::Ready(daemon) => assert_eq!(daemon.stderr(), "", "{flag} {ms}"),
                Started::Exited(status, stderr) => {
                    panic!("{flag} {ms} refused {status}: {stderr}")
                }
            }
        }
        for ms in outside {
            match start(&scratch, &root, &[flag, ms]) {
                Started::Exited(status, stderr) => {
                    assert_eq!(
                        (status.code(), stderr.as_str()),
                        (refused, usage.as_str()),
                        "{flag} {ms}"
                    );
                }
                Started::Ready(_) => panic!("{flag} {ms} accepted"),
            }
        }
        let without_deliver = launch(
            INBOX,
            &[],
            &root,
            &scratch.0.join("i"),
            None,
            &[flag, edges[0]],
        );
        match without_deliver {
            Started::Exited(status, stderr) => {
                assert_eq!(
                    (status.code(), stderr.as_str()),
                    (refused, usage.as_str()),
                    "{flag}"
                );
            }
            Started::Ready(_) => panic!("{flag} without inbox.deliver accepted"),
        }
    }
}

#[test]
fn with_a_short_lease_an_orphaned_processing_row_expires_uncertain_within_it() {
    const TTL: f64 = 4.0;
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let engine = FakeC6::start(&scratch.socket());
    engine.script("is_idle", idle(true));
    engine.script("inject", accepted());
    engine.script("status", status("running", None));
    let pace = ["--deliver-tick-ms", "50", "--lease-ttl-ms", "4000"];
    let started = |what: &str| match start(&scratch, &root, &pace) {
        Started::Ready(daemon) => daemon,
        Started::Exited(status, stderr) => panic!("{what} exited {status}: {stderr}"),
    };
    let killed = started("first daemon");
    publish_c1(&killed, "evt:1", "task_result", "Card finished.");
    let inbox = Inbox::open(&root).expect("inbox");
    wait_row(&inbox, "evt:1", |r| r.status == "processing");
    drop(killed);
    let at = unix_now();
    let lease_until = inbox
        .get("evt:1")
        .expect("get")
        .expect("row")
        .lease_until
        .expect("lease_until");
    assert!(
        (at - 0.5..=at).contains(&(lease_until - TTL)),
        "lease until {lease_until}, killed at {at}"
    );
    let _restarted = started("restarted daemon");
    // No publish reaches the restarted daemon and its idle loop has no tick, so only the
    // lease's due time can wake it within the wait.
    let row = wait_row(&inbox, "evt:1", settled);
    let seen = unix_now();
    assert_eq!(state(&row), ("uncertain", Some("expired-processing"), 1));
    assert!(seen >= lease_until, "expired before its lease");
    assert_eq!(
        row.lease_until,
        Some(lease_until),
        "the restarted daemon renewed it"
    );
}

#[test]
fn a_short_retry_delay_offers_a_refused_event_again_after_it() {
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let engine = FakeC6::start(&scratch.socket());
    engine.script("is_idle", idle(true));
    engine.script("inject", not_accepted());
    let daemon = match start(&scratch, &root, &["--retry-delay-ms", "1000"]) {
        Started::Ready(daemon) => daemon,
        Started::Exited(status, stderr) => panic!("daemon exited {status}: {stderr}"),
    };
    publish_c1(&daemon, "evt:1", "task_result", "Card finished.");
    let injects = engine.wait_calls("inject", 2);
    assert_eq!(
        (&injects[0].0["attempt"], &injects[1].0["attempt"]),
        (&json!(1), &json!(2))
    );
    let gap = injects[1].1 - injects[0].1;
    assert!(gap >= Duration::from_secs(1), "offered again after {gap:?}");
    assert!(
        gap < Duration::from_secs_f64(RETRY_DELAY),
        "offered again only after the default delay, {gap:?}"
    );
}

#[test]
fn status_calls_come_one_deliver_tick_apart() {
    const MS: u64 = 50;
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let engine = FakeC6::start(&scratch.socket());
    engine.script("is_idle", idle(true));
    engine.script("inject", accepted());
    for _ in 0..3 {
        engine.script("status", status("running", None));
    }
    engine.script("status", status("ended", Some("sent")));
    let daemon = match start(&scratch, &root, &["--deliver-tick-ms", &MS.to_string()]) {
        Started::Ready(daemon) => daemon,
        Started::Exited(status, stderr) => panic!("daemon exited {status}: {stderr}"),
    };
    publish_c1(&daemon, "evt:1", "task_result", "Card finished.");
    let inbox = Inbox::open(&root).expect("inbox");
    let row = wait_row(&inbox, "evt:1", settled);
    assert_eq!(state(&row), ("done", Some("success"), 1));
    let calls = engine.calls("status");
    assert_eq!(calls.len(), 4);
    for pair in calls.windows(2) {
        assert!(
            pair[1].1 - pair[0].1 >= Duration::from_millis(MS),
            "status calls closer than the tick"
        );
    }
    assert!(
        calls[3].1 - calls[0].1 < TICK,
        "three ticks took longer than one default tick"
    );
}

/// A C6 `JobResult` of job `job` that ended `ok` with `output`.
fn job_result(job: &str, output: &str) -> Value {
    json!({
        "job": job, "tool": "terminal", "outcome": "ok", "started": 1_763_289_900.5,
        "ended": 1_763_290_000.25, "output": output,
    })
}

#[test]
fn a_pull_publishes_each_ended_job_as_a_signed_job_result_confirms_it_and_delivers_it() {
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let engine = FakeC6::start(&scratch.socket());
    let result = job_result("1763289000000000-1", "{\"output\": \"built\"}");
    engine.script("job_results", Reply::Ok(json!({"results": [result]})));
    engine.script("job_results", Reply::Ok(json!({"results": []})));
    engine.script("jobs_done", Reply::Ok(json!({"ok": true})));
    engine.script("is_idle", idle(true));
    engine.script("inject", accepted());
    engine.script("status", status("running", None));
    let daemon = daemon(&scratch, &root);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        engine.count(),
        0,
        "the loop called the engine before a pull"
    );

    let pulled: Value = call(&daemon.socket, "pull", &json!({}), 200);
    assert_eq!(pulled, json!({"ok": true}));
    let done = engine.wait_calls("jobs_done", 1);
    assert_eq!(done[0].0, json!({"jobs": ["1763289000000000-1"]}));
    let id = "engine:job:1763289000000000-1";
    let injects = engine.wait_calls("inject", 1);
    assert_eq!(
        (&injects[0].0["event_id"], &injects[0].0["kind"]),
        (&json!(id), &json!("job_result"))
    );
    let inbox = Inbox::open(&root).expect("inbox");
    let row = inbox.get(id).expect("get").expect("the job's event");
    let stored: Value = serde_json::from_slice(&row.packet).expect("packet");
    let payload: Value =
        serde_json::from_str(stored["payload"].as_str().expect("payload")).expect("payload json");
    assert_eq!(
        payload,
        json!({
            "source": "engine", "job_id": "1763289000000000-1", "tool": "terminal",
            "outcome": "ok", "started": 1_763_289_900.5, "ended": 1_763_290_000.25,
            "output": "{\"output\": \"built\"}",
        })
    );
    let mut unsigned = stored.clone();
    unsigned.as_object_mut().expect("object").remove("mac");
    assert_eq!(
        sign(&KEY, &unsigned),
        stored,
        "signed with the producer key"
    );
    assert_eq!(stored["source_ts"], 1_763_290_000.25);
    assert_eq!(engine.calls("job_results").len(), 1);
}

#[test]
fn a_result_pulled_again_before_its_confirmation_stays_one_event() {
    let scratch = Scratch::new();
    let root = enabled_root(&scratch);
    let engine = FakeC6::start(&scratch.socket());
    let result = job_result("1763289000000000-2", "same");
    // The fake repeats its last reply: the engine hands the same result again.
    engine.script("job_results", Reply::Ok(json!({"results": [result]})));
    engine.script("jobs_done", Reply::Ok(json!({"ok": true})));
    engine.script("is_idle", idle(false));
    let daemon = daemon(&scratch, &root);
    let _: Value = call(&daemon.socket, "pull", &json!({}), 200);
    engine.wait_calls("jobs_done", 1);
    let _: Value = call(&daemon.socket, "pull", &json!({}), 200);
    engine.wait_calls("jobs_done", 2);
    let counts: Value = call(&daemon.socket, "counts", &json!({}), 200);
    assert_eq!(counts["pending"], 1, "{counts}");
    assert_eq!(daemon.stderr(), "");
}

#[test]
fn without_the_delivery_loop_a_pull_is_not_taken() {
    let scratch = Scratch::new();
    let root = scratch.root_with_key(&KEY);
    let engine = FakeC6::start(&scratch.socket());
    let daemon = daemon(&scratch, &root);
    let pulled: Value = call(&daemon.socket, "pull", &json!({}), 200);
    assert_eq!(pulled, json!({"ok": false}));
    thread::sleep(Duration::from_millis(200));
    assert_eq!(engine.count(), 0);
}
