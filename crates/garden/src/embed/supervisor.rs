//! The helper of the running daemon (ADR-0047 sections 2 and 6): ready or down, restarted by the
//! worker after a pause of 1, 2, 4 and up to 60 seconds, and the time of the last query that the
//! worker's passages give way to.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use super::lock;
use super::pipe::{Helper, StartError};
use crate::model::UnixTime;
use crate::worker::Wake;

/// Seconds before the first restart of a helper that went down.
pub const FIRST_PAUSE: UnixTime = 1;
/// Most seconds between two restarts.
pub const MAX_PAUSE: UnixTime = 60;

type Start = dyn Fn() -> Result<Helper, StartError> + Send + Sync;

/// The daemon's helper, shared by the request path and the worker.
pub struct Supervisor {
    start: Box<Start>,
    wake: Arc<Wake>,
    state: Mutex<State>,
    last_query: Mutex<Option<UnixTime>>,
}

enum State {
    Up(Arc<Helper>),
    /// `retry_at` is set by the worker's next look at the helper, `pause` seconds after it.
    Down {
        reason: String,
        pause: UnixTime,
        retry_at: Option<UnixTime>,
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
        Ok(Self {
            start: Box::new(start),
            wake,
            state: Mutex::new(State::Up(Arc::new(helper))),
            last_query: Mutex::new(None),
        })
    }

    /// The ready helper, or why the dense side is down.
    ///
    /// # Errors
    ///
    /// The reason the dense side is down.
    pub fn helper(&self) -> Result<Arc<Helper>, String> {
        match &*self.checked() {
            State::Up(helper) => Ok(Arc::clone(helper)),
            State::Down { reason, .. } => Err(reason.clone()),
        }
    }

    /// The state with a failed ready helper turned down: one stderr line
    /// `garden: embedder: down: <reason>` and a wake of the worker.
    fn checked(&self) -> MutexGuard<'_, State> {
        let mut state = lock(&self.state);
        if let State::Up(helper) = &*state
            && let Some(reason) = helper.fault()
        {
            let _ = writeln!(io::stderr(), "garden: embedder: down: {reason}");
            *state = State::Down {
                reason,
                pause: FIRST_PAUSE,
                retry_at: None,
            };
            self.wake.wake();
        }
        state
    }

    /// The worker's look at the helper at `now`; a helper down without a restart time gets one
    /// its pause after `now`.
    pub fn due(&self, now: UnixTime) -> Due {
        let mut state = self.checked();
        match &mut *state {
            State::Up(helper) => Due::Ready(Arc::clone(helper)),
            State::Down {
                retry_at: Some(at), ..
            } if *at > now => Due::At(*at),
            State::Down {
                retry_at: Some(_), ..
            } => Due::Restart,
            State::Down {
                retry_at: at @ None,
                pause,
                ..
            } => {
                let next = now.saturating_add(*pause);
                *at = Some(next);
                Due::At(next)
            }
        }
    }

    /// Starts the helper again, without holding the state while it loads: ready again with one
    /// stderr line `garden: embedder: back`, else down with the start's reason and twice the
    /// pause, at most [`MAX_PAUSE`].
    pub fn restart(&self) {
        let started = (self.start)();
        let mut state = lock(&self.state);
        match started {
            Ok(helper) => {
                *state = State::Up(Arc::new(helper));
                let _ = writeln!(io::stderr(), "garden: embedder: back");
            }
            Err(e) => {
                if let State::Down {
                    reason,
                    pause,
                    retry_at,
                } = &mut *state
                {
                    *reason = format!("restart: {e}");
                    *pause = pause.saturating_mul(2).min(MAX_PAUSE);
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
}
