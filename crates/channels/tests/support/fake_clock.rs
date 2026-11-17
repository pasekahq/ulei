//! Clock that records sleeps and advances virtual time instead of blocking.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use ulei_channels::bot_api::Clock;

/// Virtual time starting at creation; `jitter` is a fixed value.
pub(crate) struct FakeClock {
    now: Mutex<Instant>,
    slept: Mutex<Vec<Duration>>,
    jitter: Duration,
}

impl FakeClock {
    /// A clock whose jitter is always `jitter`.
    pub(crate) fn new(jitter: Duration) -> Self {
        Self {
            now: Mutex::new(Instant::now()),
            slept: Mutex::new(Vec::new()),
            jitter,
        }
    }

    /// Every sleep so far, in order.
    pub(crate) fn slept(&self) -> Vec<Duration> {
        self.slept.lock().expect("clock").clone()
    }

    /// Moves virtual time forward without recording a sleep.
    pub(crate) fn advance(&self, by: Duration) {
        *self.now.lock().expect("clock") += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.now.lock().expect("clock")
    }

    fn sleep(&self, wait: Duration) {
        self.slept.lock().expect("clock").push(wait);
        self.advance(wait);
    }

    fn jitter(&self) -> Duration {
        self.jitter
    }
}
