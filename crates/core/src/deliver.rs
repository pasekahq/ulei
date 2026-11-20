// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The owner inbox delivery loop of `ulei-inbox serve --deliver` (ADR-0036 sections 1 to 3 and
//! 7): the single consumer of a root, taking each claimable event into the engine's owner
//! session over C6 and settling it from the turn's report.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::Value;
use ulei_contracts::generated::c1::AckOutcome;
use ulei_contracts::generated::c6::{
    Delivery, InjectRequest, InjectResponse, StatusRequest, TurnState,
};

use crate::c6::{self, Client};
use crate::inbox::{self, EventRow, Inbox, LEASE_TTL, RETRY_DELAY};
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
    }))
}

impl Consumer {
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
                Wait::Due(None) => published.recv().is_ok(),
                Wait::Due(Some(left)) => !matches!(
                    published.recv_timeout(left),
                    Err(RecvTimeoutError::Disconnected)
                ),
            };
            if !open {
                return;
            }
        }
    }

    /// One step of `Bridge.tick`: follow the turn in flight, or replay an unanswered
    /// `inject`, or recover, check `ENABLED`, the engine's owner session, claim and inject.
    fn step(&mut self) -> Result<Wait, Failed> {
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
        if !self.engine.is_idle().is_ok_and(|status| status.idle) {
            return Ok(Wait::Tick);
        }
        match self.inbox.claim(now()?, self.pace.lease_ttl)? {
            Some(row) => self.inject(claimed(&row)?),
            None => Ok(Wait::Now),
        }
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
        let outcome = match (report.state, report.delivery) {
            (TurnState::Queued | TurnState::Running, _) => return Ok(Wait::Tick),
            (TurnState::Ended, None) => {
                flight.ended = true;
                return Ok(Wait::Tick);
            }
            (TurnState::Ended, Some(delivery)) => ack_outcome(delivery, &flight.kind),
            (TurnState::Dropped | TurnState::Unknown, _) => AckOutcome::Uncertain,
        };
        self.inbox
            .ack(&flight.id, &flight.lease, outcome.as_str(), now()?)?;
        self.flight = None;
        Ok(Wait::Now)
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
