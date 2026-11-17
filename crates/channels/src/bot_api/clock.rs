//! Time, sleeping and the stop signal, injected so delays are testable.

use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Source of time for retries, limits and cooldowns.
pub trait Clock: Send + Sync {
    /// Current monotonic time.
    fn now(&self) -> Instant;
    /// Blocks the calling thread for `wait`.
    fn sleep(&self, wait: Duration);
    /// A uniformly random delay in `[0, 1 s)` added to retry back-off.
    fn jitter(&self) -> Duration;
    /// Sleeps for `wait` or until `stop` is requested; returns whether it was.
    fn sleep_or_stop(&self, wait: Duration, stop: &Stop) -> bool {
        if stop.is_requested() {
            return true;
        }
        self.sleep(wait);
        stop.is_requested()
    }
}

/// The real clock: `Instant`, `thread::sleep`, and a stop that cuts sleeps short.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, wait: Duration) {
        std::thread::sleep(wait);
    }

    fn jitter(&self) -> Duration {
        // A randomly keyed hash is enough entropy for spreading retries; no RNG crate.
        let bits = RandomState::new().hash_one(Instant::now());
        Duration::from_nanos(bits % 1_000_000_000)
    }

    fn sleep_or_stop(&self, wait: Duration, stop: &Stop) -> bool {
        stop.wait(wait)
    }
}

/// Shared stop request of a poller; cloning shares the request.
#[derive(Clone, Debug, Default)]
pub struct Stop(Arc<(Mutex<bool>, Condvar)>);

impl Stop {
    /// A stop not yet requested.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests the stop and wakes every waiter.
    pub fn request(&self) {
        let (flag, cvar) = &*self.0;
        *flag.lock().unwrap_or_else(PoisonError::into_inner) = true;
        cvar.notify_all();
    }

    /// Whether the stop was requested.
    pub fn is_requested(&self) -> bool {
        *self.0.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait(&self, wait: Duration) -> bool {
        let (flag, cvar) = &*self.0;
        let guard = flag.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = cvar
            .wait_timeout_while(guard, wait, |stopped| !*stopped)
            .unwrap_or_else(PoisonError::into_inner);
        *guard
    }
}
