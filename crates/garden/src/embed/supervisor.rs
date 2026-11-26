//! The helper of the running daemon (ADR-0047 sections 2 and 6): ready or down, restarted by the
//! worker after a pause of 1, 2, 4 and up to 60 seconds, the time of the last query that the
//! worker's passages give way to, and the resident matrix of its model's vectors. A model file
//! found corrupted (ADR-0087 open question 15) holds it down without restarts until a content
//! check passes again.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use super::pipe::{Helper, StartError};
use super::{Embedder, lock};
use crate::matrix::Matrix;
use crate::model::UnixTime;
use crate::wire::c7;
use crate::worker::{Clock, Wake};

/// Seconds before the first restart of a helper that went down.
pub const FIRST_PAUSE: UnixTime = 1;
/// Most seconds between two restarts.
pub const MAX_PAUSE: UnixTime = 60;
/// Seconds a restarted helper stays up before its next fall pauses [`FIRST_PAUSE`] again: one
/// that falls sooner most likely falls for the same cause.
pub const STEADY: UnixTime = 60;

type Start = dyn Fn() -> Result<Helper, StartError> + Send + Sync;

/// The daemon's helper, shared by the request path and the worker.
pub struct Supervisor {
    start: Box<Start>,
    wake: Arc<Wake>,
    slot: Mutex<Slot>,
    last_query: Mutex<Option<UnixTime>>,
    matrix: Matrix,
    last_check: Mutex<Option<c7::ModelCheck>>,
    deferred: Mutex<usize>,
}

struct Slot {
    state: State,
    /// A restart was refused for a corrupted model file and no check has looked since.
    suspect: bool,
    /// Seconds from the next fall to the restart.
    pause: UnixTime,
    /// When a restart brought the helper up, until its next fall is paced.
    restarted_at: Option<UnixTime>,
}

enum State {
    Up(Arc<Helper>),
    /// `retry_at` is set by the worker's next look at the helper.
    Down {
        reason: String,
        retry_at: Option<UnixTime>,
    },
    /// The model file is corrupted: no restart until [`Supervisor::recover`].
    Corrupted {
        reason: String,
    },
}

/// What the worker does next about the helper.
pub enum Due {
    /// The helper is ready.
    Ready(Arc<Helper>),
    /// The helper is down until a restart at this time.
    At(UnixTime),
    /// The helper is down and its restart is due.
    Restart,
    /// The helper is held down for a corrupted model file; nothing is due.
    Off,
}

impl Supervisor {
    /// Starts the helper with `start`; the supervisor wakes the worker through `wake` when the
    /// helper goes down.
    ///
    /// # Errors
    ///
    /// The [`StartError`] of the first start.
    pub fn start(
        start: impl Fn() -> Result<Helper, StartError> + Send + Sync + 'static,
        wake: Arc<Wake>,
    ) -> Result<Self, StartError> {
        let helper = start()?;
        Ok(Self::with_state(start, wake, State::Up(Arc::new(helper))))
    }

    /// The supervisor of a helper whose model file is corrupted for `reason`: down, and not
    /// started with `start` until [`Supervisor::recover`].
    pub fn corrupted(
        start: impl Fn() -> Result<Helper, StartError> + Send + Sync + 'static,
        wake: Arc<Wake>,
        reason: String,
    ) -> Self {
        Self::with_state(start, wake, State::Corrupted { reason })
    }

    fn with_state(
        start: impl Fn() -> Result<Helper, StartError> + Send + Sync + 'static,
        wake: Arc<Wake>,
        state: State,
    ) -> Self {
        Self {
            start: Box::new(start),
            wake,
            slot: Mutex::new(Slot {
                state,
                suspect: false,
                pause: FIRST_PAUSE,
                restarted_at: None,
            }),
            last_query: Mutex::new(None),
            matrix: Matrix::new(),
            last_check: Mutex::new(None),
            deferred: Mutex::new(0),
        }
    }

    /// Holds the dense side down for a corrupted model file: stops the helper (a call in flight
    /// keeps it until it returns) and restarts it no more until [`Supervisor::recover`]; one
    /// stderr line `garden: embedder: down: <reason>` unless it was held for the same reason.
    pub fn corrupt(&self, reason: &str) {
        let mut slot = lock(&self.slot);
        if !matches!(&slot.state, State::Corrupted { reason: r } if r == reason) {
            let _ = writeln!(io::stderr(), "garden: embedder: down: {reason}");
        }
        slot.state = State::Corrupted {
            reason: reason.to_owned(),
        };
        slot.suspect = false;
    }

    /// Ends the hold of [`Supervisor::corrupt`] once the model file verified again: the helper
    /// is due for a restart at once, with the first pause again. False when it was not held.
    pub fn recover(&self) -> bool {
        let mut slot = lock(&self.slot);
        let State::Corrupted { reason } = &slot.state else {
            return false;
        };
        slot.state = State::Down {
            reason: reason.clone(),
            retry_at: Some(UnixTime::MIN),
        };
        slot.pause = FIRST_PAUSE;
        slot.restarted_at = None;
        drop(slot);
        self.wake.wake();
        true
    }

    /// Restarts a ready helper at once, for `reason` (its model file was replaced): one stderr
    /// line `garden: embedder: down: <reason>`. Nothing when it is not ready.
    pub fn renew(&self, reason: &str) {
        let mut slot = lock(&self.slot);
        if let State::Up(_) = &slot.state {
            let _ = writeln!(io::stderr(), "garden: embedder: down: {reason}");
            slot.state = State::Down {
                reason: reason.to_owned(),
                retry_at: Some(UnixTime::MIN),
            };
            slot.pause = FIRST_PAUSE;
            drop(slot);
            self.wake.wake();
        }
    }

    /// Whether the helper is held down for a corrupted model file.
    pub fn is_corrupted(&self) -> bool {
        matches!(lock(&self.slot).state, State::Corrupted { .. })
    }

    /// True once after a restart was refused for a corrupted model file: the worker then checks
    /// the file at once.
    pub fn take_suspect(&self) -> bool {
        std::mem::take(&mut lock(&self.slot).suspect)
    }

    /// The model file's digest the ready helper names (`model_sha256` of its ready line).
    pub fn digest(&self) -> Option<String> {
        match &lock(&self.slot).state {
            State::Up(helper) => Some(helper.identity().model_sha256.clone()),
            State::Down { .. } | State::Corrupted { .. } => None,
        }
    }

    /// The resident matrix the worker keeps for the helper's model.
    pub fn matrix(&self) -> &Matrix {
        &self.matrix
    }

    /// The ready helper, or why the dense side is down.
    ///
    /// # Errors
    ///
    /// The reason the dense side is down.
    pub fn helper(&self) -> Result<Arc<Helper>, String> {
        match &self.checked().state {
            State::Up(helper) => Ok(Arc::clone(helper)),
            State::Down { reason, .. } | State::Corrupted { reason } => Err(reason.clone()),
        }
    }

    /// The model id of the helper while it counts as up, without looking for a fault.
    pub fn model_id(&self) -> Option<String> {
        match &lock(&self.slot).state {
            State::Up(helper) => Some(helper.model_id().to_owned()),
            State::Down { .. } | State::Corrupted { .. } => None,
        }
    }

    /// The state with a failed ready helper turned down: one stderr line
    /// `garden: embedder: down: <reason>` and a wake of the worker.
    fn checked(&self) -> MutexGuard<'_, Slot> {
        let mut slot = lock(&self.slot);
        if let State::Up(helper) = &slot.state
            && let Some(reason) = helper.fault()
        {
            let _ = writeln!(io::stderr(), "garden: embedder: down: {reason}");
            slot.state = State::Down {
                reason,
                retry_at: None,
            };
            self.wake.wake();
        }
        slot
    }

    /// The worker's look at the helper at `now`; a helper down without a restart time gets one
    /// its pause after `now`. That pause is [`FIRST_PAUSE`] again when the helper fell
    /// [`STEADY`] seconds or more after a restart brought it up.
    pub fn due(&self, now: UnixTime) -> Due {
        let mut slot = self.checked();
        let slot = &mut *slot;
        match &mut slot.state {
            State::Up(helper) => Due::Ready(Arc::clone(helper)),
            State::Corrupted { .. } => Due::Off,
            State::Down {
                retry_at: Some(at), ..
            } if *at > now => Due::At(*at),
            State::Down {
                retry_at: Some(_), ..
            } => Due::Restart,
            State::Down {
                retry_at: at @ None,
                ..
            } => {
                if slot
                    .restarted_at
                    .take()
                    .is_some_and(|up| now.saturating_sub(up) >= STEADY)
                {
                    slot.pause = FIRST_PAUSE;
                }
                let next = now.saturating_add(slot.pause);
                *at = Some(next);
                Due::At(next)
            }
        }
    }

    /// Starts the helper again, without holding the state while it loads: ready again, up since
    /// `clock`'s time when the start returned, with one stderr line `garden: embedder: back`,
    /// else down with the start's reason, or held down as by [`Supervisor::corrupt`] when the
    /// helper refused a corrupted model file, which [`Supervisor::take_suspect`] then reports.
    /// Either way the pause doubles, at most [`MAX_PAUSE`].
    pub fn restart(&self, clock: &dyn Clock) {
        let started = (self.start)();
        let now = clock.now();
        let mut slot = lock(&self.slot);
        slot.pause = slot.pause.saturating_mul(2).min(MAX_PAUSE);
        match started {
            Ok(helper) => {
                slot.state = State::Up(Arc::new(helper));
                slot.restarted_at = Some(now);
                let _ = writeln!(io::stderr(), "garden: embedder: back");
            }
            Err(StartError::Corrupt(reason)) => {
                let _ = writeln!(io::stderr(), "garden: embedder: down: {reason}");
                slot.state = State::Corrupted { reason };
                slot.suspect = true;
            }
            Err(e) => {
                if let State::Down { reason, retry_at } = &mut slot.state {
                    *reason = format!("restart: {e}");
                    *retry_at = None;
                }
            }
        }
    }

    /// Records a query that arrived at `now`.
    pub fn queried(&self, now: UnixTime) {
        *lock(&self.last_query) = Some(now);
    }

    /// When the last query arrived.
    pub fn last_query(&self) -> Option<UnixTime> {
        *lock(&self.last_query)
    }

    /// Records the content check of the model file the worker ran, for `health`.
    pub fn record_check(&self, check: c7::ModelCheck) {
        *lock(&self.last_check) = Some(check);
    }

    /// The last content check of the model file since the daemon started.
    pub fn last_check(&self) -> Option<c7::ModelCheck> {
        lock(&self.last_check).clone()
    }

    /// Records how many passages the worker put behind the others after the helper was killed
    /// embedding them, for `health`.
    pub fn set_deferred(&self, passages: usize) {
        *lock(&self.deferred) = passages;
    }

    /// The passages the worker put behind the others, as last recorded.
    pub fn deferred(&self) -> usize {
        *lock(&self.deferred)
    }

    /// The model of the dense side: the ready helper's, else the one of the resident matrix, the
    /// model the helper had when it was last up; `None` before the dense side had one.
    pub fn dense_model(&self) -> Option<String> {
        self.model_id().or_else(|| self.matrix.model())
    }
}
