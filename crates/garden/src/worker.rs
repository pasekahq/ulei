//! The worker thread of `ulei-garden serve` (ADR-0047 section 6): on its own connection to the
//! store it runs the passes, lookups, helper restarts, passage embeddings and updates of the
//! resident matrix that no request runs, the periodic content check of the model file (ADR-0086
//! open question 15), and between them sleeps on a condition variable until its nearest deadline
//! or a wake from a request.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::edge::{Edge, EdgeConfig, EdgeError, Outcome};
use crate::embed::check::{Check, FileId, Look, Verdict};
use crate::embed::supervisor::{Due, Supervisor};
use crate::embed::{EmbedError, Embedder};
#[cfg(doc)]
use crate::matrix::Matrix;
use crate::miss::{self, Inbox};
use crate::model::{Fact, FactId, Sensitivity, UnixTime};
use crate::profile::Profile;
use crate::second::Source;
use crate::store::{Store, StoreError};
use crate::wire::c7;

/// The `garden.schedule` settings: intervals in seconds above 0 and a batch size above 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// `garden.schedule.expire_interval`: least seconds between two expiry passes.
    pub expire_interval: UnixTime,
    /// `garden.schedule.recheck_interval`: least seconds between two recheck batches.
    pub recheck_interval: UnixTime,
    /// `garden.schedule.recheck_batch`: most rechecks handed to the second level per batch.
    pub recheck_batch: usize,
    /// `garden.schedule.maintain_interval`: seconds between two maintenance passes.
    pub maintain_interval: UnixTime,
    /// `garden.schedule.verify_interval`: seconds between two content checks of the encoder
    /// engine's weight file.
    pub verify_interval: UnixTime,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            expire_interval: 60,
            recheck_interval: 3_600,
            recheck_batch: 8,
            maintain_interval: 604_800,
            verify_interval: 86_400,
        }
    }
}

/// Seconds from a content check that did not pass (corrupted, unusable or without a result) to
/// the next one; each further such check doubles it up to [`CHECK_RETRY_MAX`], never past
/// `verify_interval`, and a passed one starts it again (review E13r, finding 3).
pub const CHECK_RETRY: UnixTime = 60;

/// Most seconds between two content checks while none passes.
pub const CHECK_RETRY_MAX: UnixTime = 3_600;

/// SIGKILLs from outside in a row, each while the same passage was in flight, after which that
/// passage goes behind the others (review E13r, finding 5). It is never skipped for them: such a
/// kill is not the text's failure (ADR-0086 E12a, F3).
pub const DEFER_KILLS: u32 = 3;

/// Seconds a passage put behind the others waits before it is tried again; each kill on a
/// later try doubles it, up to [`DEFER_MAX`].
pub const DEFER_FIRST: UnixTime = 300;

/// Most seconds a deferred passage waits.
pub const DEFER_MAX: UnixTime = 21_600;

/// The daemon's time source, read by the worker and the request path alike.
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

/// Seconds without a `search` or `pointers` before the worker sends a passage to the helper.
pub const QUIET: UnixTime = 2;

/// The passes a worker has run since it was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Passes {
    /// Expiry passes that committed.
    pub expiry: u64,
    /// Maintenance passes that committed.
    pub maintenance: u64,
    /// Content checks of the model file that reached a verdict, good or corrupted.
    pub checks: u64,
    /// Passes, second-level lookups, and reads of the next expiry or miss retry, that failed on
    /// the store or the edge.
    pub failed: u64,
}

/// The background work of the daemon on its own store connection.
pub struct Worker<C> {
    store: Store,
    schedule: Schedule,
    clock: C,
    inbox: Inbox,
    source: Option<Box<dyn Source + Send>>,
    edge: EdgeConfig,
    helper: Option<Arc<Supervisor>>,
    profile: Option<Arc<Profile>>,
    last_expiry: Option<UnixTime>,
    last_recheck: Option<UnixTime>,
    expiry: Option<UnixTime>,
    maintenance: UnixTime,
    passes: Passes,
    /// Seconds from the next content check that does not pass to the one after it.
    check_retry: UnixTime,
    /// The passage of the last call a SIGKILL from outside ended, and how many such calls in a
    /// row it had.
    kills: Option<(String, u32)>,
    /// The passages put behind the others for those kills, by fact id, under `deferred_model`.
    deferred: HashMap<String, Deferral>,
    deferred_model: Option<String>,
    /// Which file the model path names, looked at before and after each check.
    look: Option<Box<Look>>,
    /// [`Store::revision`] when the matrix last matched the store.
    synced: Option<(i64, u64)>,
    /// The content check of the model file and when it is next due.
    check: Option<(Box<Check>, UnixTime)>,
    /// The start of the last passed content check: no vector gets a `computed_at` before it,
    /// whatever the wall clock says, so a failed check drops every vector computed since
    /// (ADR-0086 E12a, F4).
    checked_from: UnixTime,
}

impl<C: Clock> Worker<C> {
    /// A worker on `store` that asks `source` about every new miss and every queued recheck, writes
    /// what it finds into the seedbed with the edge's refusal screen of `edge` and publishes the
    /// misses left to `inbox`; without a source it publishes every miss and rechecks nothing. Its
    /// first expiry pass is due at the earliest expiry, its first recheck batch when a recheck
    /// waits, and its first maintenance pass one `maintain_interval` after the last pass the store
    /// records, else after now.
    ///
    /// # Errors
    ///
    /// The store cannot tell its earliest expiry or its last maintenance pass.
    pub fn new(
        store: Store,
        schedule: Schedule,
        clock: C,
        inbox: Inbox,
        source: Option<Box<dyn Source + Send>>,
        edge: EdgeConfig,
    ) -> Result<Self, StoreError> {
        let maintenance = store
            .last_maintenance()?
            .unwrap_or_else(|| clock.now())
            .saturating_add(schedule.maintain_interval);
        Ok(Self {
            expiry: store.next_expiry()?,
            store,
            schedule,
            clock,
            inbox,
            source,
            edge,
            helper: None,
            profile: None,
            last_expiry: None,
            last_recheck: None,
            maintenance,
            passes: Passes::default(),
            check_retry: CHECK_RETRY,
            kills: None,
            deferred: HashMap::new(),
            deferred_model: None,
            look: None,
            synced: None,
            check: None,
            checked_from: 0,
        })
    }

    /// The worker that also runs `check`, the content check of the helper's model file, at its
    /// first tick and every `verify_interval` after (ADR-0086 open question 15), and at once
    /// after a restart of the helper refused a corrupted file ([`Worker::tick`]).
    #[must_use]
    pub fn with_check(mut self, check: impl Fn() -> Verdict + Send + 'static) -> Self {
        let due = self.clock.now();
        self.checked_from = self
            .store
            .model_checks()
            .ok()
            .and_then(|c| c.passed)
            .map_or(0, |(at, _)| at);
        self.check = Some((Box::new(check), due));
        self
    }

    /// The worker that also looks, with `look`, which file the model path names before and after
    /// each content check: a check across a change has no result, and a passed one restarts a
    /// ready helper that maps another file, and holds a restarted one that does, until a check
    /// passes on its file (review E13r, finding 6).
    #[must_use]
    pub fn with_check_file(mut self, look: impl Fn() -> Option<FileId> + Send + 'static) -> Self {
        self.look = Some(Box::new(look));
        self
    }

    /// The worker that also restarts `helper` when it is down, embeds with it every fact
    /// without a vector of its model, current versions before closed ones, and keeps the
    /// helper's resident matrix ([`Worker::tick`]).
    #[must_use]
    pub fn with_helper(mut self, helper: Arc<Supervisor>) -> Self {
        self.helper = Some(helper);
        self
    }

    /// The worker that renders `profile` at the end of every tick and also wakes when a pinned
    /// version starts, ends or expires ([`Store::next_pinned_change`]).
    #[must_use]
    pub fn with_profile(mut self, profile: Arc<Profile>) -> Self {
        self.profile = Some(profile);
        self
    }

    /// The passes run so far.
    pub fn passes(&self) -> Passes {
        self.passes
    }

    /// Runs the passes due at the clock's now, the recheck batch when due, asks the source
    /// about the new misses, publishes the misses due then ([`miss::publish_due`]), restarts
    /// the helper or embeds one passage or reconciles one fact with it, runs the model file's
    /// content check when due (`Worker::check_model`), brings the resident matrix to the store
    /// (`Worker::refresh`), renders the core profile and returns the next deadline. A failed pass is one stderr line `garden: worker: <class>` and is due again
    /// after its interval; an inbox that does not store a miss leaves its row for the retry
    /// the outbox sets.
    pub fn tick(&mut self) -> UnixTime {
        let now = self.clock.now();
        if self.expiry.is_some_and(|due| due <= now) {
            self.last_expiry = Some(now);
            match self.store.mark_expired(now) {
                Ok(_) => self.passes.expiry += 1,
                Err(e) => self.fail(e.class()),
            }
        }
        if self.maintenance <= now {
            self.maintenance = now.saturating_add(self.schedule.maintain_interval);
            match self.store.maintain(now) {
                Ok(()) => self.passes.maintenance += 1,
                Err(e) => self.fail(e.class()),
            }
        }
        if let Err(e) = self.recheck(now) {
            self.fail(e.class());
        }
        if let Err(e) = self.ask(now) {
            self.fail(e.class());
        }
        let embed = self.embed(now);
        let retry = match miss::next_attempt(&self.store) {
            Ok(Some(due)) if due <= now => miss::publish_due(&mut self.store, &self.inbox, now)
                .and_then(|()| miss::next_attempt(&self.store)),
            other => other,
        };
        let retry = retry.unwrap_or_else(|e| {
            self.fail(e.class());
            Some(now.saturating_add(miss::FIRST_RETRY))
        });
        let floor = self
            .last_expiry
            .map(|last| last.saturating_add(self.schedule.expire_interval));
        self.expiry = match self.store.next_expiry() {
            Ok(earliest) => earliest.map(|x| floor.map_or(x, |f| x.max(f))),
            Err(e) => {
                self.fail(e.class());
                Some(now.saturating_add(self.schedule.expire_interval))
            }
        };
        let recheck = self.next_recheck(now).unwrap_or_else(|e| {
            self.fail(e.class());
            Some(now.saturating_add(self.schedule.recheck_interval))
        });
        let (checked, embed) = match self.check_model(now) {
            Some(next) => (Some(next.0), if next.1 { Some(now) } else { embed }),
            None => (None, embed),
        };
        self.refresh(now);
        let pinned = self.render(now);
        let deferred = self.next_deferral(now);
        [
            self.expiry,
            retry,
            recheck,
            embed,
            pinned,
            checked,
            deferred,
        ]
        .into_iter()
        .flatten()
        .fold(self.maintenance, UnixTime::min)
    }

    /// Runs the content check of the model file when it is due at `now`, or at once when a
    /// restart of the helper was refused for a corrupted file, and acts on its verdict; the
    /// next check's due time, and whether the helper's state changed so that the next tick is
    /// due at once. Nothing without a check.
    ///
    /// - Good: the store records the passed check with its digest; a helper held down for a
    ///   corrupted file is restarted (its vectors computed since the last passed check were
    ///   dropped and are embedded again), and a ready helper of another digest, the file
    ///   replaced under it, is restarted on the new file (another model id: the vectors of the
    ///   old one go, [`Store::drop_other_vectors`]).
    /// - Corrupted: the helper is stopped and held down, so `search` and `pointers` answer
    ///   lexical-only and no passage is embedded, and the store records the failure and drops
    ///   the vectors computed since the last passed check in one unit of work
    ///   ([`Store::model_check_failed`]). Two stderr lines: `garden: embedder: down: <reason>`
    ///   and how many vectors went.
    /// - Unusable (a missing, unreadable or older-format file): the helper is stopped and held
    ///   down like a corrupted one, with the reason in `health`; no vector is dropped. One stderr
    ///   line, `garden: embedder: down: <reason>`.
    /// - Failed (no verdict): one stderr line `garden: embedder: check: <reason>`, nothing else
    ///   changes.
    /// - Not applicable (the model directory holds the ONNX model, no weight file of the
    ///   engine): a helper held down by an earlier verdict is restarted on what is there, and
    ///   `health` shows no check; the next one is due `verify_interval` later.
    ///
    /// Every check is recorded for `health`. After a passed check the next one is due
    /// `verify_interval` later; after any other, [`CHECK_RETRY`] later, doubling with each further
    /// one up to [`CHECK_RETRY_MAX`] and never past `verify_interval`, so a held dense side
    /// comes back within minutes of the file verifying again (review E13r, finding 3). A
    /// corrupted verdict while already held drops nothing more and prints the count line only
    /// when it drops a vector.
    ///
    /// A store failure is one stderr line `garden: worker: <class>` and the check is due again
    /// after `expire_interval`.
    fn check_model(&mut self, now: UnixTime) -> Option<(UnixTime, bool)> {
        let (_, due) = self.check.as_ref()?;
        let supervisor = self.helper.clone();
        let suspect = supervisor.as_ref().is_some_and(|s| s.take_suspect());
        if *due > now && !suspect {
            return Some((*due, false));
        }
        let before = self.look.as_ref().map(|look| look());
        let verdict = self.check.as_ref().map(|(run, _)| run())?;
        let after = self.look.as_ref().map(|look| look());
        // Another file came to the path while the check hashed it: the verdict may be about a
        // file no longer there (review E13r, finding 6).
        let verdict = if before == after {
            verdict
        } else {
            Verdict::Failed("model file replaced while it was checked".into())
        };
        if let Some(supervisor) = &supervisor {
            supervisor.record_check(model_check(now, &verdict));
            if !matches!(verdict, Verdict::Good(_) | Verdict::Failed(_)) {
                supervisor.verified(None);
            }
        }
        let mut next = if matches!(verdict, Verdict::Good(_) | Verdict::NotApplicable) {
            self.check_retry = CHECK_RETRY;
            now.saturating_add(self.schedule.verify_interval)
        } else {
            let retry = self.check_retry.min(self.schedule.verify_interval);
            self.check_retry = self.check_retry.saturating_mul(2).min(CHECK_RETRY_MAX);
            now.saturating_add(retry)
        };
        let mut changed = false;
        match verdict {
            Verdict::Good(digest) => {
                self.passes.checks += 1;
                self.checked_from = self.checked_from.max(now);
                if let Err(e) = self.store.model_check_passed(&digest, now) {
                    self.fail(e.class());
                    next = now.saturating_add(self.schedule.expire_interval);
                }
                if let Some(supervisor) = &supervisor {
                    changed = passed(supervisor, &digest, after.flatten());
                }
            }
            Verdict::Corrupted(reason) => {
                self.passes.checks += 1;
                let held = supervisor.as_ref().is_some_and(|s| s.is_corrupted());
                if let Some(supervisor) = &supervisor {
                    supervisor.corrupt(&reason);
                }
                match self.store.model_check_failed(&reason, now) {
                    // Checked again while held: nothing was embedded since, so nothing went.
                    Ok(0) if held => {}
                    Ok(dropped) => {
                        let _ = writeln!(
                            io::stderr(),
                            "garden: embedder: model file corrupted: {dropped} vectors computed \
                             since the last passed check dropped; lexical-only until it verifies \
                             again"
                        );
                    }
                    Err(e) => {
                        self.fail(e.class());
                        next = now.saturating_add(self.schedule.expire_interval);
                    }
                }
            }
            // No helper can start on what is there now (a missing, unreadable or older-format
            // file): dense goes down with the reason until a check passes again, instead of the
            // running helper serving a model that is no longer on disk while `health` says
            // ready (ADR-0086 E12a, F5). Its vectors came from the file it mapped at start, which
            // the replacement did not change, so none are dropped.
            Verdict::Unusable(reason) => {
                self.passes.checks += 1;
                if let Some(supervisor) = &supervisor {
                    supervisor.corrupt(&reason);
                }
            }
            Verdict::Failed(reason) => {
                let _ = writeln!(io::stderr(), "garden: embedder: check: {reason}");
            }
            // The directory now runs the ONNX backend: a dense side held for the engine's file
            // comes back on what is there (review E13r, finding 9).
            Verdict::NotApplicable => {
                if let Some(supervisor) = &supervisor
                    && supervisor.recover()
                {
                    let _ = writeln!(
                        io::stderr(),
                        "garden: embedder: no weight file of the engine to check any more; \
                         restarting the helper"
                    );
                    changed = true;
                }
            }
        }
        if let Some((_, due)) = &mut self.check {
            *due = next;
        }
        Some((next, changed))
    }

    /// Renders the core profile at `now` and returns when its set of pinned current versions
    /// next changes without a write; nothing without a profile.
    fn render(&mut self, now: UnixTime) -> Option<UnixTime> {
        let profile = Arc::clone(self.profile.as_ref()?);
        profile.update(&self.store, now);
        self.store.next_pinned_change(now).unwrap_or_else(|e| {
            self.fail(e.class());
            Some(now.saturating_add(self.schedule.expire_interval))
        })
    }

    /// Restarts the helper when it is down and its pause is over, else, once no `search` or
    /// `pointers` has arrived for [`QUIET`] seconds, does one piece of [`Worker::work`]: embeds
    /// one passage or reconciles one new fact with the facts most like it
    /// ([`Edge::reconcile_similar`]); the deadline of the next restart, passage or
    /// reconciliation. A passage the helper refuses is one stderr line `garden: worker: embed`;
    /// a refused one or one the helper falls on during its call is counted
    /// ([`Store::passage_failed`]), one that never reached a helper already down is not; a
    /// counted one goes behind the passages that failed less, and its
    /// [`PASSAGE_TRIES`](crate::store::PASSAGE_TRIES)th failure is one stderr line
    /// `garden: worker: passage <fact id> skipped` without its text.
    fn embed(&mut self, now: UnixTime) -> Option<UnixTime> {
        let supervisor = Arc::clone(self.helper.as_ref()?);
        let mut now = now;
        let helper = loop {
            match supervisor.due(now) {
                Due::Ready(helper) => break helper,
                Due::At(at) => return Some(at),
                Due::Off => return None,
                Due::Restart => {
                    supervisor.restart(&self.clock);
                    now = self.clock.now();
                }
            }
        };
        let work = match self.work(helper.model_id()) {
            Ok(Some(work)) => work,
            Ok(None) => return None,
            Err(e) => {
                self.fail(e.class());
                return None;
            }
        };
        if let Some(quiet) = supervisor.last_query().map(|at| at.saturating_add(QUIET))
            && now < quiet
        {
            return Some(quiet);
        }
        let (fact, text) = match work {
            Work::Passage(fact, text) => (fact, text),
            Work::Reconcile(fact) => {
                let edge = self.edge;
                return match Edge::new(&mut self.store, edge).reconcile_similar(
                    &fact,
                    helper.model_id(),
                    now,
                ) {
                    Ok(()) => Some(now),
                    Err(e) => {
                        self.fail(e.class());
                        None
                    }
                };
            }
        };
        let model = helper.model_id();
        let (vector, fell, killed) = match helper.embed_passages_traced(&[text.as_str()]) {
            Ok(mut vectors) if vectors.len() == 1 => (vectors.pop(), false, false),
            Err((EmbedError::Down(_), killed)) => (None, false, killed),
            Ok(_) | Err(_) => (None, true, false),
        };
        self.note_kill(&fact, killed, vector.is_some() || fell, now);
        let Some(vector) = vector else {
            let counted = !fell || self.passage_fell(&fact, model);
            // An uncounted refusal would come straight back to the same passage.
            return match supervisor.due(now) {
                Due::At(at) => Some(at),
                Due::Off => None,
                Due::Ready(_) => {
                    self.fail("embed");
                    counted.then_some(now)
                }
                Due::Restart => Some(now),
            };
        };
        let computed_at = self.clock.now().max(self.checked_from);
        match self.store.put_vector(&fact, model, &vector, computed_at) {
            Ok(()) => Some(now),
            Err(e) => {
                self.fail(e.class());
                None
            }
        }
    }

    /// Keeps count of the SIGKILLs from outside that end the passage of `fact` (`killed`): the
    /// [`DEFER_KILLS`]th in a row puts it behind the other passages for [`DEFER_FIRST`] seconds,
    /// and a kill on a later try for twice its last wait, at most [`DEFER_MAX`], with one stderr
    /// line `garden: worker: passage <fact id> deferred for <n> s` without its text. A call that
    /// reached the helper and ended otherwise (`reached`: a vector or a counted failure) ends the
    /// count and the deferral; one that never reached it changes nothing.
    fn note_kill(&mut self, fact: &FactId, killed: bool, reached: bool, now: UnixTime) {
        if !killed {
            if reached {
                if self.kills.as_ref().is_some_and(|(f, _)| *f == fact.0) {
                    self.kills = None;
                }
                if self.deferred.remove(&fact.0).is_some() {
                    self.report_deferred();
                }
            }
            return;
        }
        let pause = if let Some(last) = self.deferred.get(&fact.0) {
            last.pause.saturating_mul(2).min(DEFER_MAX)
        } else {
            let n = match self.kills.take() {
                Some((f, n)) if f == fact.0 => n + 1,
                _ => 1,
            };
            if n < DEFER_KILLS {
                self.kills = Some((fact.0.clone(), n));
                return;
            }
            DEFER_FIRST
        };
        let _ = writeln!(
            io::stderr(),
            "garden: worker: passage {} deferred for {pause} s: the helper was killed embedding it",
            fact.0
        );
        self.deferred.insert(
            fact.0.clone(),
            Deferral {
                until: now.saturating_add(pause),
                pause,
            },
        );
        self.report_deferred();
    }

    /// Tells the supervisor, for `health`, how many passages are deferred.
    fn report_deferred(&self) {
        if let Some(supervisor) = &self.helper {
            supervisor.set_deferred(self.deferred.len());
        }
    }

    /// The deferred passages not to try at `now`, after forgetting every deferral when the
    /// helper's model is no longer the one they were made under.
    fn waiting(&mut self, model: &str, now: UnixTime) -> HashSet<String> {
        if self.deferred_model.as_deref() != Some(model) {
            self.deferred_model = Some(model.to_owned());
            self.kills = None;
            if !self.deferred.is_empty() {
                self.deferred.clear();
                self.report_deferred();
            }
        }
        self.deferred
            .iter()
            .filter(|(_, d)| d.until > now)
            .map(|(fact, _)| fact.clone())
            .collect()
    }

    /// When the next deferred passage may be tried again, if one waits after `now`.
    fn next_deferral(&self, now: UnixTime) -> Option<UnixTime> {
        self.deferred
            .values()
            .map(|d| d.until)
            .filter(|&until| until > now)
            .min()
    }

    /// Counts a failure of the passage of `fact` under `model`, with one stderr line when it is
    /// skipped from now on; false when the store fails.
    fn passage_fell(&mut self, fact: &FactId, model: &str) -> bool {
        match self.store.passage_failed(fact, model) {
            Ok(skipped) => {
                if skipped {
                    let _ = writeln!(io::stderr(), "garden: worker: passage {} skipped", fact.0);
                }
                true
            }
            Err(e) => {
                self.fail(e.class());
                false
            }
        }
    }

    /// Brings the helper's resident matrix to the store at `now`: when the ready helper's model
    /// is not the matrix's, deletes the vectors of every other model
    /// ([`Store::drop_other_vectors`]) and rebuilds the matrix; else, when the store changed
    /// since the matrix last matched it, syncs the matrix ([`Matrix::sync`]). A failure is one
    /// stderr line `garden: worker: <class>` and is retried at the next tick.
    fn refresh(&mut self, now: UnixTime) {
        let Some(supervisor) = self.helper.clone() else {
            return;
        };
        let matrix = supervisor.matrix();
        let model = supervisor.model_id().filter(|m| !matrix.is_of(m));
        let dropped = match &model {
            Some(model) => self.store.drop_other_vectors(model).map(drop),
            None => Ok(()),
        };
        // Read before the matrix reads the store: a commit landing during the read changes the
        // revision again, so the next tick syncs once more.
        let result = dropped
            .and_then(|()| self.store.revision())
            .and_then(|revision| {
                match &model {
                    Some(model) => matrix.rebuild(&self.store, model, now)?,
                    None if self.synced == Some(revision) => {}
                    None => matrix.sync(&self.store, now)?,
                }
                Ok(revision)
            });
        self.synced = match result {
            Ok(revision) => Some(revision),
            Err(e) => {
                self.fail(e.class());
                None
            }
        };
    }

    /// The next piece of work for `model`: the passage of a current fact without a vector,
    /// else a reconciliation, else the passage of a closed version without one.
    fn work(&mut self, model: &str) -> Result<Option<Work>, EdgeError> {
        let waiting = self.waiting(model, self.clock.now());
        if let Some((fact, text)) = self.store.missing_vector_skipping(model, false, &waiting)? {
            return Ok(Some(Work::Passage(fact, text)));
        }
        if let Some(fact) = Edge::new(&mut self.store, self.edge).unreconciled(model)? {
            return Ok(Some(Work::Reconcile(fact)));
        }
        let closed = self.store.missing_vector_skipping(model, true, &waiting)?;
        Ok(closed.map(|(fact, text)| Work::Passage(fact, text)))
    }

    /// When the next recheck batch is due: none without a source or a waiting recheck.
    fn next_recheck(&self, now: UnixTime) -> Result<Option<UnixTime>, StoreError> {
        if self.source.is_none() || self.store.rechecks(1)?.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.last_recheck.map_or(now, |last| {
            last.saturating_add(self.schedule.recheck_interval)
        })))
    }

    /// Hands the text of each of the `recheck_batch` oldest rechecks to the source, at most once
    /// per `recheck_interval`, and writes its candidates into the seedbed ([`Edge::seed`]) as a
    /// new version of the expired fact; a recheck with a candidate stored as a record is
    /// cleared in the same unit of work, every other one stays queued for the next batch.
    fn recheck(&mut self, now: UnixTime) -> Result<(), EdgeError> {
        let Some(source) = &self.source else {
            return Ok(());
        };
        if self
            .last_recheck
            .is_some_and(|last| now < last.saturating_add(self.schedule.recheck_interval))
        {
            return Ok(());
        }
        let batch = self
            .store
            .rechecks(self.schedule.recheck_batch)
            .inspect_err(|_| self.last_recheck = Some(now))?;
        if batch.is_empty() {
            return Ok(());
        }
        self.last_recheck = Some(now);
        for expired in batch {
            let candidates = lookup(source.as_ref(), &expired.text, now);
            let edge = self.edge;
            self.store.atomically(|store| {
                let mut renewed = false;
                for candidate in &candidates {
                    let version = new_version(&expired, candidate);
                    let outcome = Edge::new(store, edge).seed(&version, now)?;
                    renewed |= matches!(outcome, Outcome::Seeded { .. });
                }
                if renewed {
                    store.clear_recheck(&expired.id)?;
                }
                Ok::<_, EdgeError>(())
            })?;
        }
        Ok(())
    }

    /// Hands the query of every new miss to the source and writes its candidates into the
    /// seedbed ([`Edge::seed`]); a row with a candidate stored as a record is settled
    /// unpublished, every other row is left to publish. A failed lookup is one stderr line
    /// `garden: second level: <class>`.
    fn ask(&mut self, now: UnixTime) -> Result<(), EdgeError> {
        for row in miss::fresh(&self.store)? {
            let candidates = match &self.source {
                Some(source) => lookup(source.as_ref(), &row.query, now),
                None => Vec::new(),
            };
            let edge = self.edge;
            self.store.atomically(|store| {
                let mut answered = false;
                for candidate in &candidates {
                    let outcome = Edge::new(store, edge).seed(candidate, now)?;
                    answered |= matches!(outcome, Outcome::Seeded { .. });
                }
                miss::asked(store, row.seq, answered)?;
                Ok::<_, EdgeError>(())
            })?;
        }
        Ok(())
    }

    fn fail(&mut self, class: &str) {
        self.passes.failed += 1;
        let _ = writeln!(io::stderr(), "garden: worker: {class}");
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

/// A passage put behind the others after SIGKILLs from outside (review E13r, finding 5).
struct Deferral {
    /// When it may be tried again.
    until: UnixTime,
    /// Seconds it waited last.
    pause: UnixTime,
}

/// The next piece of work for the helper's model.
enum Work {
    /// A current fact or a closed version without a vector and the passage to embed for it.
    Passage(FactId, String),
    /// A new fact waiting for its reconciliation.
    Reconcile(FactId),
}

/// Acts on a passed check of the model file `file` with content `digest`: records the file as
/// verified first, so a restart from here on maps it or is held ([`Supervisor::restart`]); then
/// restarts a helper held down by an earlier verdict, or a ready helper of another digest (the
/// file replaced under it) or of another file with the same content (renamed over it, review
/// E13r, finding 6), on the verified file. True when the helper's state changed.
fn passed(supervisor: &Supervisor, digest: &str, file: Option<FileId>) -> bool {
    supervisor.verified(file);
    if supervisor.recover() {
        let _ = writeln!(
            io::stderr(),
            "garden: embedder: model file verified again (content sha256 {digest}); restarting \
             the helper"
        );
    } else if supervisor.digest().is_some_and(|d| d != digest) {
        supervisor.renew(&format!(
            "model file replaced (content sha256 {digest}); restarting the helper"
        ));
    } else if let Some(file) = file
        && supervisor.maps_other_than(file)
    {
        supervisor.renew(&format!(
            "model file replaced by a file of the same content (sha256 {digest}); restarting \
             the helper on the file verified"
        ));
    } else {
        return false;
    }
    true
}

/// The `health` record of a content check started at `now` that found `verdict`; none when no
/// check applied.
fn model_check(now: UnixTime, verdict: &Verdict) -> Option<c7::ModelCheck> {
    let (result, detail) = match verdict {
        Verdict::NotApplicable => return None,
        Verdict::Good(_) => (c7::ModelCheckResult::Passed, None),
        Verdict::Corrupted(reason) => (c7::ModelCheckResult::Corrupted, Some(reason)),
        Verdict::Unusable(reason) => (c7::ModelCheckResult::Unusable, Some(reason)),
        Verdict::Failed(reason) => (c7::ModelCheckResult::NoResult, Some(reason)),
    };
    Some(c7::ModelCheck {
        at: u64::try_from(now).unwrap_or(0),
        result,
        detail: detail.filter(|d| !d.is_empty()).cloned(),
    })
}

/// The candidates `source` finds for `q` at `now`; a failed lookup finds none and is one stderr
/// line `garden: second level: <class>`, never `q`.
fn lookup(source: &(dyn Source + Send), q: &str, now: UnixTime) -> Vec<c7::ProposeRequest> {
    source.find(q, now).unwrap_or_else(|e| {
        let _ = writeln!(io::stderr(), "garden: second level: {}", e.class());
        Vec::new()
    })
}

/// `candidate` as a new version of `expired`: of its entity and attribute, in its domain when
/// the candidate names none, and as private or secret as it was.
fn new_version(expired: &Fact, candidate: &c7::ProposeRequest) -> c7::ProposeRequest {
    let (private, secret_ref) = match &expired.sensitivity {
        Sensitivity::Public => (candidate.private, candidate.secret_ref.clone()),
        Sensitivity::Private => (Some(true), candidate.secret_ref.clone()),
        Sensitivity::Secret { vault_ref } => (candidate.private, Some(vault_ref.clone())),
    };
    c7::ProposeRequest {
        entity: Some(expired.entity.0.clone()),
        attribute: expired.attribute.clone(),
        domain: candidate.domain.clone().or_else(|| expired.domain.clone()),
        private,
        secret_ref,
        ..candidate.clone()
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
