//! The worker thread of `ulei-garden serve` (ADR-0047 section 6): on its own connection to the
//! store it runs the passes that no request runs, and between them sleeps on a condition
//! variable until its nearest deadline or a wake from a request.

use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::model::UnixTime;
use crate::store::{Store, StoreError};

/// The `garden.schedule` intervals, in seconds above 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// `garden.schedule.expire_interval`: least seconds between two expiry passes.
    pub expire_interval: UnixTime,
    /// `garden.schedule.maintain_interval`: seconds between two maintenance passes.
    pub maintain_interval: UnixTime,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            expire_interval: 60,
            maintain_interval: 604_800,
        }
    }
}

/// The worker's time source.
pub trait Clock {
    /// Seconds since the Unix epoch.
    fn now(&self) -> UnixTime;
}

/// The system's wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> UnixTime {
        let secs = |d: Duration| UnixTime::try_from(d.as_secs()).unwrap_or(UnixTime::MAX);
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(after) => secs(after),
            Err(before) => -secs(before.duration()),
        }
    }
}

/// The signal a request sends the worker; a signal sent while the worker is busy is kept for
/// its next sleep.
#[derive(Debug, Default)]
pub struct Wake {
    signalled: Mutex<bool>,
    cond: Condvar,
}

impl Wake {
    /// Signals the worker.
    pub fn wake(&self) {
        *self
            .signalled
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        self.cond.notify_one();
    }

    /// Blocks until a signal or until `timeout` has passed and consumes the signal; true when
    /// signalled.
    pub fn wait(&self, timeout: Duration) -> bool {
        let end = Instant::now().checked_add(timeout);
        let mut signalled = self
            .signalled
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if *signalled {
                *signalled = false;
                return true;
            }
            signalled = match end {
                None => self
                    .cond
                    .wait(signalled)
                    .unwrap_or_else(PoisonError::into_inner),
                Some(end) => {
                    let left = end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return false;
                    }
                    self.cond
                        .wait_timeout(signalled, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
    }
}

/// The passes a worker has run since it was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Passes {
    /// Expiry passes that committed.
    pub expiry: u64,
    /// Maintenance passes that committed.
    pub maintenance: u64,
    /// Passes, and reads of the next expiry, that failed.
    pub failed: u64,
}

/// The background work of the daemon on its own store connection.
pub struct Worker<C> {
    store: Store,
    schedule: Schedule,
    clock: C,
    last_expiry: Option<UnixTime>,
    expiry: Option<UnixTime>,
    maintenance: UnixTime,
    passes: Passes,
}

impl<C: Clock> Worker<C> {
    /// A worker on `store` whose first expiry pass is due at the earliest expiry and whose
    /// first maintenance pass is due one `maintain_interval` from now.
    ///
    /// # Errors
    ///
    /// The store cannot tell its earliest expiry.
    pub fn new(store: Store, schedule: Schedule, clock: C) -> Result<Self, StoreError> {
        let maintenance = clock.now().saturating_add(schedule.maintain_interval);
        Ok(Self {
            expiry: store.next_expiry()?,
            store,
            schedule,
            clock,
            last_expiry: None,
            maintenance,
            passes: Passes::default(),
        })
    }

    /// The passes run so far.
    pub fn passes(&self) -> Passes {
        self.passes
    }

    /// Runs the passes due at the clock's now and returns the next deadline. A failed pass is
    /// one stderr line `garden: worker: <class>` and is due again after its interval.
    pub fn tick(&mut self) -> UnixTime {
        let now = self.clock.now();
        if self.expiry.is_some_and(|due| due <= now) {
            self.last_expiry = Some(now);
            match self.store.mark_expired(now) {
                Ok(_) => self.passes.expiry += 1,
                Err(e) => self.fail(&e),
            }
        }
        if self.maintenance <= now {
            self.maintenance = now.saturating_add(self.schedule.maintain_interval);
            match self.store.maintain() {
                Ok(()) => self.passes.maintenance += 1,
                Err(e) => self.fail(&e),
            }
        }
        let floor = self
            .last_expiry
            .map(|last| last.saturating_add(self.schedule.expire_interval));
        self.expiry = match self.store.next_expiry() {
            Ok(earliest) => earliest.map(|x| floor.map_or(x, |f| x.max(f))),
            Err(e) => {
                self.fail(&e);
                Some(now.saturating_add(self.schedule.expire_interval))
            }
        };
        self.expiry
            .map_or(self.maintenance, |x| x.min(self.maintenance))
    }

    fn fail(&mut self, e: &StoreError) {
        self.passes.failed += 1;
        let _ = writeln!(io::stderr(), "garden: worker: {}", e.class());
    }

    /// Ticks, then sleeps on `wake` until the deadline the tick returned, forever.
    pub fn run(mut self, wake: &Wake) -> ! {
        loop {
            let deadline = self.tick();
            let left = deadline.saturating_sub(self.clock.now()).max(0);
            wake.wait(Duration::from_secs(left.unsigned_abs()));
        }
    }
}

/// Starts `worker` on a thread of its own, woken by `wake`.
///
/// # Errors
///
/// The thread cannot be created.
pub fn spawn<C: Clock + Send + 'static>(worker: Worker<C>, wake: Arc<Wake>) -> io::Result<()> {
    thread::Builder::new()
        .name("garden-worker".into())
        .spawn(move || worker.run(&wake))
        .map(drop)
}
