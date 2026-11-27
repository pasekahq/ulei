// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The owner inbox delivery loop of `ulei-inbox serve` with `inbox.deliver` (ADR-0036 sections
//! 1 to 3 and 7): the single consumer of a root, taking each claimable event into the engine's
//! owner session over C6 and settling it from the turn's report. After a C1 `pull` it also
//! publishes the engine's ended background jobs as `job_result` events (ADR-0102).

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use rustix::fs::inotify::{self, CreateFlags, WatchFlags};

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c1::{AckOutcome, EventTrust, Kind};
use ulei_contracts::generated::c6::{
    Delivery, InjectRequest, InjectResponse, JobResult, StatusRequest, TurnOutcome, TurnReport,
    TurnState,
};

use crate::c6::{self, Client};
use crate::inbox::{self, DB_FILE, EventRow, Inbox, LEASE_TTL, RETRY_DELAY};
use crate::prompt::model_text;
use crate::serve::now;

/// Default period of the loop while a turn is in flight or a claimable event waits for a free
/// session.
pub const TICK: Duration = Duration::from_secs(2);
/// The file under the root whose presence lets the loop claim.
pub const ENABLED: &str = "ENABLED";
/// The file under the root whose exclusive `flock` makes the holder the root's one consumer.
pub const CONSUMER_LOCK: &str = "consumer.lock";
/// The stderr line of a failed step; it never carries a payload, lease or error text.
pub const TICK_FAILED: &str = "ulei-inbox: owner inbox tick failed; durable event retained";
/// The prefix of the stderr line written once when the engine's `is_idle` starts failing (or
/// fails in another way); the error's category follows, never its text.
pub const ENGINE_UNREACHABLE: &str = "ulei-inbox: engine unreachable: ";
/// The stderr line written once when `is_idle` answers again after [`ENGINE_UNREACHABLE`].
pub const ENGINE_REACHABLE: &str = "ulei-inbox: engine reachable again";
/// Job results asked for at once after a `pull`.
pub const JOB_RESULTS_LIMIT: u64 = 16;
/// The prefix of the event id of a job's result, before the job id.
pub const JOB_EVENT_PREFIX: &str = "engine:job:";
/// Period of the loop's fallback poll when the root's database cannot be watched (ADR-0084):
/// rows other writers commit without a C1 publish are taken within it.
pub const POLL: Duration = Duration::from_secs(5);

/// The periods of the loop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pace {
    /// Period of the loop while a turn is in flight or a claimable event waits.
    pub tick: Duration,
    /// Seconds of the lease that `claim` and `renew` set.
    pub lease_ttl: f64,
    /// Seconds a `retry` puts an event back before it is claimable again.
    pub retry_delay: f64,
}

impl Pace {
    /// The deployed pace: [`TICK`], [`LEASE_TTL`] and [`RETRY_DELAY`].
    pub const DEFAULT: Self = Self {
        tick: TICK,
        lease_ttl: LEASE_TTL,
        retry_delay: RETRY_DELAY,
    };
}

/// `recover` moves a lease only once `lease_until < now`, so the wait ends just after it.
const DUE_MARGIN: Duration = Duration::from_millis(1);

/// Why the loop does not run on a root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Off {
    /// `ENABLED` is not a file at start.
    NotEnabled,
    /// Another process holds `consumer.lock`.
    Locked,
}

impl Off {
    /// The fixed stderr line naming the reason.
    pub const fn line(self) -> &'static str {
        match self {
            Self::NotEnabled => "ulei-inbox: owner inbox delivery off; ENABLED is missing",
            Self::Locked => {
                "ulei-inbox: owner inbox delivery off; consumer.lock is held by another consumer"
            }
        }
    }
}

/// The consumer of one root: its store connection, the engine client, the held lock and its
/// pace.
pub struct Consumer {
    root: PathBuf,
    pace: Pace,
    inbox: Inbox,
    engine: Client,
    _lock: File,
    replay: Option<Claimed>,
    flight: Option<Flight>,
    poll: Option<Duration>,
    unreachable: Option<String>,
    pull: Option<Arc<AtomicBool>>,
}

/// A claimed event whose `inject` has no answer yet; its lease is held.
struct Claimed {
    request: InjectRequest,
    lease_until: f64,
}

/// An accepted event: its turn runs or ran in the engine and the row is `processing`.
struct Flight {
    id: String,
    lease: String,
    kind: String,
    status: StatusRequest,
    lease_until: f64,
    ended: bool,
}

/// What the loop waits for after a step.
enum Wait {
    /// Nothing: the next step runs at once.
    Now,
    /// One tick, whatever is published meanwhile.
    Tick,
    /// A publish, or this long when given.
    Due(Option<Duration>),
}

/// A step that failed; the row keeps its state and the loop goes on.
struct Failed;

impl From<inbox::Error> for Failed {
    fn from(_: inbox::Error) -> Self {
        Self
    }
}

impl From<std::time::SystemTimeError> for Failed {
    fn from(_: std::time::SystemTimeError) -> Self {
        Self
    }
}

/// Opens the consumer of `root` as the reference `start` does: `Off::NotEnabled` unless
/// `ENABLED` is a file, then a store connection and an exclusive non-blocking `flock` of
/// `consumer.lock` (created 0600), `Off::Locked` when another process holds it.
///
/// # Errors
/// The store refuses `root`, or `consumer.lock` cannot be opened or locked.
pub fn open(
    root: &Path,
    engine: Client,
    pace: Pace,
) -> Result<Result<Consumer, Off>, inbox::Error> {
    if !root.join(ENABLED).is_file() {
        return Ok(Err(Off::NotEnabled));
    }
    let inbox = Inbox::open(root)?;
    let lock = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(root.join(CONSUMER_LOCK))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(Err(Off::Locked)),
        Err(TryLockError::Error(e)) => return Err(e.into()),
    }
    Ok(Ok(Consumer {
        root: root.to_owned(),
        pace,
        inbox,
        engine,
        _lock: lock,
        replay: None,
        flight: None,
        poll: None,
        unreachable: None,
        pull: None,
    }))
}

impl Consumer {
    /// The consumer that also wakes every `every` while nothing is due, for a root whose
    /// database [`watch`] cannot watch (ADR-0084).
    #[must_use]
    pub fn polling(mut self, every: Duration) -> Self {
        self.poll = Some(every);
        self
    }

    /// The consumer that takes the engine's ended jobs at the step after `pull` was set.
    #[must_use]
    pub fn pulling(mut self, pull: Arc<AtomicBool>) -> Self {
        self.pull = Some(pull);
        self
    }

    /// Runs steps with the tick of its pace until every sender of `published` is gone; each
    /// sent `()` is a committed publish. A failed step writes [`TICK_FAILED`] and the loop goes
    /// on after a tick.
    pub fn run(mut self, published: &Receiver<()>) {
        loop {
            let wait = self.step().unwrap_or_else(|Failed| {
                let _ = writeln!(std::io::stderr(), "{TICK_FAILED}");
                Wait::Tick
            });
            let open = match wait {
                Wait::Now => true,
                Wait::Tick => pause(published, Instant::now() + self.pace.tick),
                Wait::Due(left) => match left.into_iter().chain(self.poll).min() {
                    None => published.recv().is_ok(),
                    Some(left) => !matches!(
                        published.recv_timeout(left),
                        Err(RecvTimeoutError::Disconnected)
                    ),
                },
            };
            if !open {
                return;
            }
        }
    }

    /// One step of `Bridge.tick`: follow the turn in flight, or replay an unanswered
    /// `inject`, or recover, check `ENABLED`, the engine's owner session, claim and inject.
    fn step(&mut self) -> Result<Wait, Failed> {
        if self
            .pull
            .as_ref()
            .is_some_and(|pull| pull.swap(false, Ordering::AcqRel))
        {
            self.take_jobs()?;
        }
        if self.flight.is_some() {
            return self.follow();
        }
        if let Some(claimed) = &mut self.replay {
            let at = now()?;
            let request = &claimed.request;
            self.inbox
                .renew(&request.event_id, &request.lease, at, self.pace.lease_ttl)?;
            claimed.lease_until = at + self.pace.lease_ttl;
        }
        if let Some(claimed) = self.replay.take() {
            return self.inject(claimed);
        }
        let at = now()?;
        self.inbox.recover(at)?;
        if !self.root.join(ENABLED).is_file() || !self.inbox.claimable(at)? {
            let left = match self.inbox.next_due(at)? {
                Some(due) => {
                    Some(Duration::try_from_secs_f64(due - at).map_err(|_| Failed)? + DUE_MARGIN)
                }
                None => None,
            };
            return Ok(Wait::Due(left));
        }
        let idle = self.engine.is_idle();
        self.reach(idle.as_ref().err());
        if !idle.is_ok_and(|status| status.idle) {
            return Ok(Wait::Tick);
        }
        match self.inbox.claim(now()?, self.pace.lease_ttl)? {
            Some(row) => self.inject(claimed(&row)?),
            None => Ok(Wait::Now),
        }
    }

    /// Publishes each ended job the engine holds as a `job_result` event signed with the
    /// producer key, then confirms them to the engine with `jobs_done`, until none is left. A
    /// result whose id the store holds with other content is confirmed, the stored event kept.
    fn take_jobs(&mut self) -> Result<(), Failed> {
        loop {
            let results = self
                .engine
                .job_results(JOB_RESULTS_LIMIT)
                .map_err(|_| Failed)?;
            let count = results.results.len() as u64;
            if count == 0 {
                return Ok(());
            }
            let mut done = Vec::with_capacity(results.results.len());
            for result in results.results {
                match self.inbox.publish_own(job_packet(&result), now()?) {
                    Ok(_) => {}
                    Err(inbox::Error::Conflict) => {
                        let _ = writeln!(
                            std::io::stderr(),
                            "ulei-inbox: the result of job {} conflicts with its stored event; the stored one stays",
                            result.job
                        );
                    }
                    Err(e) => return Err(e.into()),
                }
                done.push(result.job);
            }
            self.engine.jobs_done(done).map_err(|_| Failed)?;
            if count < JOB_RESULTS_LIMIT {
                return Ok(());
            }
        }
    }

    /// Writes [`ENGINE_UNREACHABLE`] with the category of `failed` when it differs from the
    /// last one written, and [`ENGINE_REACHABLE`] once the engine answers after one; a failing
    /// engine is otherwise only a busy one (ADR-0036 section 2).
    fn reach(&mut self, failed: Option<&c6::Error>) {
        let now = failed.map(category);
        if now == self.unreachable {
            return;
        }
        let _ = match &now {
            Some(category) => writeln!(std::io::stderr(), "{ENGINE_UNREACHABLE}{category}"),
            None => writeln!(std::io::stderr(), "{ENGINE_REACHABLE}"),
        };
        self.unreachable = now;
    }

    /// `inject` of a claimed event and the C1 call its answer needs (PLUGIN.md section 6).
    fn inject(&mut self, claimed: Claimed) -> Result<Wait, Failed> {
        let request = &claimed.request;
        match self.engine.inject(request) {
            Ok(InjectResponse {
                accepted: true,
                session_id: Some(session),
                turn: Some(turn),
                ..
            }) => {
                if !self
                    .inbox
                    .start(&request.event_id, &request.lease, &session)?
                {
                    return Err(Failed);
                }
                self.flight = Some(Flight {
                    id: request.event_id.clone(),
                    lease: request.lease.clone(),
                    kind: request.kind.clone(),
                    status: StatusRequest {
                        session,
                        turn: Some(turn),
                    },
                    lease_until: claimed.lease_until,
                    ended: false,
                });
                Ok(Wait::Tick)
            }
            Ok(_) => {
                self.inbox.retry(
                    &request.event_id,
                    &request.lease,
                    now()?,
                    self.pace.retry_delay,
                )?;
                Ok(Wait::Now)
            }
            Err(e) if replays(&e) => {
                self.replay = Some(claimed);
                Err(Failed)
            }
            Err(_) => {
                self.inbox.retry(
                    &request.event_id,
                    &request.lease,
                    now()?,
                    self.pace.retry_delay,
                )?;
                Err(Failed)
            }
        }
    }

    /// One tick of the turn in flight: `renew` until it is seen ended, then `status`, and the
    /// `ack` of section 7 once the report settles it.
    fn follow(&mut self) -> Result<Wait, Failed> {
        let Some(flight) = self.flight.as_mut() else {
            return Ok(Wait::Now);
        };
        let at = now()?;
        if !flight.ended {
            self.inbox
                .renew(&flight.id, &flight.lease, at, self.pace.lease_ttl)?;
            flight.lease_until = at + self.pace.lease_ttl;
        } else if at > flight.lease_until {
            // The delivery came too late: `recover` settles the row `expired-processing`.
            self.flight = None;
            return Ok(Wait::Now);
        }
        let Some(report) = self
            .engine
            .status(&flight.status)
            .ok()
            .and_then(|status| status.turn)
        else {
            return Ok(Wait::Tick);
        };
        let (outcome, until) = match (report.state, report.delivery) {
            (TurnState::Queued | TurnState::Running, _) => return Ok(Wait::Tick),
            (TurnState::Ended, None) => {
                flight.ended = true;
                return Ok(Wait::Tick);
            }
            (TurnState::Ended, Some(delivery)) => settle(&report, delivery, &flight.kind),
            (TurnState::Dropped | TurnState::Unknown, _) => (AckOutcome::Uncertain, None),
        };
        match until {
            Some(until) => self.inbox.defer(&flight.id, &flight.lease, until)?,
            None => self
                .inbox
                .ack(&flight.id, &flight.lease, outcome.as_str(), now()?)?,
        };
        self.flight = None;
        Ok(Wait::Now)
    }
}

/// An inotify watch of a root for the commits of every writer of its `events.sqlite3`: the
/// daemon's own C1 server, and the writers that open the database through the library without
/// a C1 publish (ADR-0084).
pub struct Watch(OwnedFd);

/// Watches `root` for writes of its database file.
///
/// # Errors
/// `inotify_init1` or `inotify_add_watch` fails, for one when the user's inotify limits are
/// spent.
pub fn watch(root: &Path) -> io::Result<Watch> {
    let fd = inotify::init(CreateFlags::CLOEXEC)?;
    inotify::add_watch(&fd, root, WatchFlags::MODIFY)?;
    Ok(Watch(fd))
}

impl Watch {
    /// Blocks on the watch and sends one `()` into `wake` for each batch of events that writes
    /// the database file, as a publish does; returns once the receiver is gone or the watch
    /// fails. Wakes only when a writer commits, so an idle root costs no wake-up.
    pub fn run(self, wake: &SyncSender<()>) -> io::Error {
        let mut buf = [MaybeUninit::<u8>::uninit(); 4096];
        let mut events = inotify::Reader::new(&self.0, &mut buf);
        let mut written = false;
        loop {
            match events.next() {
                Ok(event) => {
                    written |= event
                        .file_name()
                        .is_some_and(|name| name.to_bytes() == DB_FILE.as_bytes());
                }
                Err(e) => return e.into(),
            }
            if events.is_buffer_empty() && std::mem::take(&mut written) {
                // A full slot already holds a wake-up the loop has not taken.
                if let Err(TrySendError::Disconnected(())) = wake.try_send(()) {
                    return io::Error::from(io::ErrorKind::BrokenPipe);
                }
            }
        }
    }
}

/// The category of a failed C6 call for [`ENGINE_UNREACHABLE`]: the I/O error kind or the
/// error class, never a message.
fn category(e: &c6::Error) -> String {
    match e {
        c6::Error::Io {
            connected: false,
            error,
        } => format!("connect: {}", error.kind()),
        c6::Error::Io { error, .. } => format!("io: {}", error.kind()),
        c6::Error::Timeout => "timeout".to_owned(),
        c6::Error::PeerUid { .. } => "engine runs as another uid".to_owned(),
        c6::Error::RequestTooLarge(_) => "request too large".to_owned(),
        c6::Error::Malformed(_) => "malformed answer".to_owned(),
        c6::Error::Invalid(_) => "invalid answer".to_owned(),
        c6::Error::Remote { code, .. } => format!("error {}", code.as_str()),
    }
}

/// Waits until `until`, taking the publishes that arrive meanwhile; false once every sender
/// is gone.
fn pause(published: &Receiver<()>, until: Instant) -> bool {
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        match published.recv_timeout(left) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// The unsigned `job_result` packet of `result`: id [`JOB_EVENT_PREFIX`] and the job id, observed
/// at the job's end, its payload a JSON object of the result.
fn job_packet(result: &JobResult) -> Map<String, Value> {
    let payload = json!({
        "source": "engine",
        "job_id": result.job,
        "tool": result.tool,
        "outcome": result.outcome.as_str(),
        "started": result.started,
        "ended": result.ended,
        "output": result.output,
    });
    let mut packet = Map::new();
    packet.insert("version".to_owned(), Value::from(1));
    packet.insert("kind".to_owned(), Value::from(Kind::JobResult.as_str()));
    packet.insert(
        "id".to_owned(),
        Value::from(format!("{JOB_EVENT_PREFIX}{}", result.job)),
    );
    packet.insert("payload".to_owned(), Value::from(payload.to_string()));
    packet.insert("source_ts".to_owned(), Value::from(result.ended));
    packet.insert(
        "trust".to_owned(),
        Value::from(EventTrust::UntrustedSourceData.as_str()),
    );
    packet
}

/// The `inject` request of a claimed row: its id, the packet's `kind`, the model text, the
/// lease and the row's `attempts` as `attempt`.
fn claimed(row: &EventRow) -> Result<Claimed, Failed> {
    let packet: Value = serde_json::from_slice(&row.packet).map_err(|_| Failed)?;
    let kind = packet["kind"].as_str().ok_or(Failed)?.to_owned();
    Ok(Claimed {
        request: InjectRequest {
            event_id: row.id.clone(),
            kind,
            prompt: model_text(row)?,
            lease: row.lease.clone().ok_or(Failed)?,
            attempt: u64::try_from(row.attempts).map_err(|_| Failed)?,
        },
        lease_until: row.lease_until.ok_or(Failed)?,
    })
}

/// Whether a failed `inject` may have reached the engine, so the loop renews and replays it
/// with the same `attempt` instead of `retry` (PLUGIN.md section 6).
fn replays(e: &c6::Error) -> bool {
    match e {
        c6::Error::Io { connected, .. } => *connected,
        c6::Error::Timeout | c6::Error::Malformed(_) | c6::Error::Invalid(_) => true,
        c6::Error::PeerUid { .. } | c6::Error::RequestTooLarge(_) | c6::Error::Remote { .. } => {
            false
        }
    }
}

/// The ack of the ended turn `report` the channel settled as `delivery`, for an event of `kind`,
/// with the time a `deferred` one is available again when it is not the default. A
/// `quota_exhausted` turn that ran no tool and consumed nothing waits for its quota's reset.
fn settle(report: &TurnReport, delivery: Delivery, kind: &str) -> (AckOutcome, Option<f64>) {
    let quota_wait = delivery == Delivery::NoAnswer
        && report.outcome == Some(TurnOutcome::QuotaExhausted)
        && report.tools_ran == Some(false);
    if !quota_wait {
        return (ack_outcome(delivery, kind), None);
    }
    #[allow(clippy::cast_precision_loss, reason = "a Unix time is exact in f64")]
    let until = report.resets_at.map(|at| at as f64);
    (AckOutcome::Deferred, until)
}

/// The ack of an ended turn's `delivery` for an event of `kind` (`Receipt.complete`).
fn ack_outcome(delivery: Delivery, kind: &str) -> AckOutcome {
    match delivery {
        Delivery::Sent => AckOutcome::Success,
        Delivery::SendFailed | Delivery::NoAnswer => AckOutcome::Uncertain,
        Delivery::SilentDone => AckOutcome::Silent,
        Delivery::Silent if kind == "weekly" => AckOutcome::Silent,
        Delivery::Silent => AckOutcome::Deferred,
    }
}
