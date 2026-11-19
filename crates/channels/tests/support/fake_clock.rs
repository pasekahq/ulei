//! Clock that records sleeps and advances virtual time instead of blocking.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use ulei_channels::bot_api::Clock;

/// Virtual time starting at creation; `jitter` is a fixed value.
pub(crate) struct FakeClock {
    now: Mutex<Instant>,
    slept: Mutex<Vec<Duration>>,
    jitter: Duration,
    /// While set, a sleep blocks until [`Self::release_sleeps`].
    hold: Mutex<bool>,
    released: Condvar,
}

impl FakeClock {
    /// A clock whose jitter is always `jitter`.
    pub(crate) fn new(jitter: Duration) -> Self {
        Self {
            now: Mutex::new(Instant::now()),
            slept: Mutex::new(Vec::new()),
            jitter,
            hold: Mutex::new(false),
            released: Condvar::new(),
        }
    }

    /// Makes every later sleep block until [`Self::release_sleeps`].
    pub(crate) fn hold_sleeps(&self) {
        *self.hold.lock().expect("clock") = true;
    }

    /// Lets the blocked sleeps and every later one go on.
    pub(crate) fn release_sleeps(&self) {
        *self.hold.lock().expect("clock") = false;
        self.released.notify_all();
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
        let held = self.hold.lock().expect("clock");
        drop(self.released.wait_while(held, |held| *held).expect("clock"));
        self.advance(wait);
    }

    fn jitter(&self) -> Duration {
        self.jitter
    }
}
