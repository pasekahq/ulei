//! The owner call budget and the background jobs of owner turns (ADR-0102), an owner-designed
//! addition, not a port. A tool call of a top-level owner turn keeps the turn waiting at most the
//! budget: an offloadable call still running then goes on as a job and the turn gets an
//! `offloaded` result at once; any other call is stopped and gets a `timeout` result. A job's
//! end is kept in the journal until the owner inbox confirmed it, and every new end wakes the
//! inbox through C1 `pull`, so the inbox signs and publishes it as a `job_result` event.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ulei_contracts::generated::c1::PullResponse;
use ulei_contracts::generated::c4g::ToolCall;
use ulei_contracts::generated::c6::{JobOutcome, JobResult};
use ulei_contracts::generated::c8;

use crate::gateway;
use crate::journal::{self, Journal};
use crate::tools::{
    Context, Detached, INTERRUPTED_BEFORE_RUN, Interrupt, JobSpill, Output, Registry, json_object,
    json_str,
};
use crate::trace::now;

/// Default of `engine.owner_call_budget`.
pub const DEFAULT_OWNER_CALL_BUDGET: Duration = Duration::from_secs(45);
/// Most jobs running at once; past it an offloadable call is stopped at the budget.
pub const MAX_JOBS: usize = 4;
/// Most characters of a job's output in its result.
pub const JOB_OUTPUT_CHARS: usize = 8_000;
/// The wait for the inbox's confirmation after a `pull`, doubled after each one that brought
/// none, up to [`MAX_PULL_RETRY`].
pub const PULL_RETRY: Duration = Duration::from_secs(15);
/// The longest wait between two `pull`s while results wait.
pub const MAX_PULL_RETRY: Duration = Duration::from_secs(300);
/// The connect and every read and write of a `pull`.
const PULL_TIMEOUT: Duration = Duration::from_secs(5);
/// Most bytes of a `pull` answer.
const MAX_PULL_ANSWER: usize = 4096;
/// What the model is told to do with an offloaded call.
const OFFLOADED_NOTE: &str = "The call is still running as a background job and was not stopped. \
Its final result reaches the owner later as an inbox event naming this job_id. Do not run the \
call again and do not wait for it: tell the owner it goes on in the background, then end the turn.";

/// How long one call of an owner turn may keep the turn waiting, and the jobs an offloadable
/// call goes on in; without jobs no call is offloaded.
#[derive(Debug, Clone, Copy)]
pub struct OwnerBudget<'a> {
    /// The budget.
    pub limit: Duration,
    /// The engine's jobs.
    pub jobs: Option<&'a Jobs>,
}

/// The engine's background jobs: their journal, the inbox socket their ends wake and the jobs
/// running now.
#[derive(Debug, Clone)]
pub struct Jobs {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    journal: PathBuf,
    inbox: PathBuf,
    /// The start in microseconds, the first part of every job id.
    epoch: u128,
    state: Mutex<State>,
    /// Signalled when a result is stored or confirmed.
    changed: Condvar,
}

#[derive(Debug, Default)]
struct State {
    issued: u64,
    running: usize,
    /// Whether the thread calling `pull` runs.
    notifier: bool,
    /// Set when a result was stored or confirmed since the notifier last looked.
    dirty: bool,
}

impl Jobs {
    /// The jobs of the journal at `journal`, whose ends wake the inbox at `inbox`: every job the
    /// journal still holds running ends `lost`, since none survives the engine, and the inbox is
    /// woken when ended jobs wait for it.
    ///
    /// # Errors
    /// The journal could not be opened or written.
    pub fn start(journal: &Path, inbox: PathBuf) -> Result<Self, journal::Error> {
        let mut store = Journal::open(journal)?;
        store.lose_jobs(now())?;
        let waiting = store.has_ended_jobs()?;
        let jobs = Self {
            inner: Arc::new(Inner {
                journal: journal.to_owned(),
                inbox,
                epoch: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros(),
                state: Mutex::default(),
                changed: Condvar::new(),
            }),
        };
        if waiting {
            jobs.notify();
        }
        Ok(jobs)
    }

    /// At most `limit` ended jobs the inbox has not confirmed, oldest first, read within
    /// `within`.
    ///
    /// # Errors
    /// The journal could not be read.
    pub fn results(
        &self,
        limit: usize,
        within: Duration,
    ) -> Result<Vec<JobResult>, journal::Error> {
        let jobs = Journal::open(&self.inner.journal)?.ended_jobs(limit, within)?;
        Ok(jobs.into_iter().filter_map(result_of).collect())
    }

    /// Forgets the ended jobs `ids` names, written within `within`; the number forgotten.
    ///
    /// # Errors
    /// The journal could not be written.
    pub fn confirm(&self, ids: &[String], within: Duration) -> Result<usize, journal::Error> {
        let removed = Journal::open(&self.inner.journal)?.confirm_jobs(ids, within)?;
        self.lock().dirty = true;
        self.inner.changed.notify_all();
        Ok(removed)
    }

    /// A new job of `tool` started at Unix time `started`, committed running; `None` while
    /// [`MAX_JOBS`] run.
    fn admit(&self, tool: &str, started: f64) -> Result<Option<String>, journal::Error> {
        let mut state = self.lock();
        if state.running >= MAX_JOBS {
            return Ok(None);
        }
        let id = format!("{}-{}", self.inner.epoch, state.issued + 1);
        Journal::open(&self.inner.journal)?.start_job(id.as_str(), tool, started, PULL_TIMEOUT)?;
        state.issued += 1;
        state.running += 1;
        Ok(Some(id))
    }

    /// Commits the end of job `id`, the call `call` that returned `output`, its output capped
    /// by `spill`, and wakes the inbox. A failed write leaves the job running in the journal,
    /// so the next start ends it `lost`.
    fn end(&self, id: &str, call: &ToolCall, output: Output, spill: &JobSpill) {
        let outcome = if output.is_error {
            c8::JobOutcome::Error
        } else {
            c8::JobOutcome::Ok
        };
        let content = spill.cap(call, output, JOB_OUTPUT_CHARS);
        let ended = Journal::open(&self.inner.journal)
            .and_then(|mut store| store.end_job(id, now(), outcome, &content));
        if let Err(e) = ended {
            eprintln!("ulei-engine: could not record the end of job {id}: {e}");
        }
        self.lock().running -= 1;
        self.notify();
    }

    /// Marks a change and starts the thread that calls `pull` until no ended job waits, unless
    /// it runs.
    fn notify(&self) {
        let mut state = self.lock();
        state.dirty = true;
        self.inner.changed.notify_all();
        if state.notifier {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let spawned = thread::Builder::new()
            .name("job-pull".into())
            .spawn(move || notifier(&inner));
        match spawned {
            Ok(_) => state.notifier = true,
            Err(e) => eprintln!("ulei-engine: cannot start the owner inbox pull: {e}"),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.inner.state)
    }
}

/// Calls `pull` while ended jobs wait for the inbox: again at once after a new end, else after
/// [`PULL_RETRY`], doubling up to [`MAX_PULL_RETRY`] while the inbox confirms nothing. Ends,
/// and clears the notifier flag, when none waits and nothing changed meanwhile.
fn notifier(inner: &Inner) {
    let mut delay = PULL_RETRY;
    let mut failing = false;
    loop {
        lock(&inner.state).dirty = false;
        let waiting = Journal::open(&inner.journal).and_then(|store| store.has_ended_jobs());
        if let Ok(false) = waiting {
            let mut state = lock(&inner.state);
            if !state.dirty {
                state.notifier = false;
                return;
            }
            continue;
        }
        let failure = match waiting
            .map_err(|e| e.to_string())
            .and_then(|_| pull(&inner.inbox))
        {
            Ok(true) => None,
            Ok(false) => Some("the owner inbox runs without inbox.deliver".to_owned()),
            Err(e) => Some(e),
        };
        match failure {
            Some(e) if !failing => {
                eprintln!(
                    "ulei-engine: owner inbox pull failed ({e}); job results wait in the journal"
                );
                failing = true;
            }
            Some(_) => {}
            None => failing = false,
        }
        let state = lock(&inner.state);
        let (state, waited) = inner
            .changed
            .wait_timeout_while(state, delay, |state| !state.dirty)
            .unwrap_or_else(PoisonError::into_inner);
        drop(state);
        delay = if waited.timed_out() {
            (delay * 2).min(MAX_PULL_RETRY)
        } else {
            PULL_RETRY
        };
    }
}

/// C1 `pull` on the inbox at `socket`: whether it was taken.
fn pull(socket: &Path) -> Result<bool, String> {
    let client = gateway::Client::new(socket, PULL_TIMEOUT, PULL_TIMEOUT);
    let (status, body) = client
        .exchange("POST /v1/pull", Some(b"{}"), None, MAX_PULL_ANSWER)
        .map_err(|e| e.to_string())?;
    if status != 200 {
        return Err(format!("status {status}"));
    }
    let answer: PullResponse =
        serde_json::from_slice(&body).map_err(|_| "answer is not a PullResponse".to_owned())?;
    Ok(answer.ok)
}

/// The C6 result of an ended journal job; `None` for one still running.
fn result_of(job: c8::Job) -> Option<JobResult> {
    let outcome = match job.outcome? {
        c8::JobOutcome::Ok => JobOutcome::Ok,
        c8::JobOutcome::Error => JobOutcome::Error,
        c8::JobOutcome::Lost => JobOutcome::Lost,
    };
    Some(JobResult {
        job: job.id,
        tool: job.tool,
        outcome,
        started: job.started,
        ended: job.ended?,
        output: job.output?,
    })
}

/// Runs `call` of an owner turn under `budget`, its result back by `deadline` plus the time a
/// stopped call takes to end: inline when it ends by then, else `offloaded` when it went on as a
/// job, else a `timeout` result with its work stopped. The turn's interrupt stops the call
/// until it is offloaded.
pub(crate) fn owner_call(
    tools: &Registry,
    call: &ToolCall,
    ctx: Context<'_>,
    deadline: Instant,
    budget: OwnerBudget<'_>,
) -> Output {
    if Instant::now() >= deadline {
        return over_budget(&call.name, budget.limit, Stop::NotStarted);
    }
    let interrupt = Interrupt::new();
    let Some(link) = ctx.interrupt.link(&interrupt) else {
        return Output {
            content: INTERRUPTED_BEFORE_RUN.into(),
            is_error: true,
        };
    };
    let detached = budget
        .jobs
        .and_then(|jobs| Some((jobs, tools.detach(&call.name, &call.input, ctx.cwd)?)));
    let output = match detached {
        Some((jobs, work)) => {
            let unlink = || ctx.interrupt.unlink(&link);
            match offloadable(
                tools,
                call,
                &interrupt,
                work,
                jobs,
                (deadline, budget.limit),
                unlink,
            ) {
                Ok(output) => output,
                Err(()) => stoppable(tools, call, ctx.cwd, &interrupt, deadline, budget.limit),
            }
        }
        None => stoppable(tools, call, ctx.cwd, &interrupt, deadline, budget.limit),
    };
    ctx.interrupt.unlink(&link);
    output
}

/// Runs `work` of `call` on a thread of its own and waits for it until `deadline`; past it the
/// call goes on as a job of `jobs`, with `unlink` cutting it from the turn's interrupt, or is
/// stopped when no job can be admitted. `Err` when the thread did not start.
fn offloadable(
    tools: &Registry,
    call: &ToolCall,
    interrupt: &Interrupt,
    work: Detached,
    jobs: &Jobs,
    (deadline, limit): (Instant, Duration),
    unlink: impl FnOnce(),
) -> Result<Output, ()> {
    let begun = Instant::now();
    let started = now();
    let slot = Arc::new(Slot::default());
    let (done, job_jobs, spill, job_call, job_interrupt) = (
        Arc::clone(&slot),
        jobs.clone(),
        tools.job_spill(),
        call.clone(),
        interrupt.clone(),
    );
    thread::Builder::new()
        .name("job".into())
        .spawn(move || {
            if let Some((id, output)) = done.deliver(work(&job_interrupt)) {
                job_jobs.end(&id, &job_call, output, &spill);
            }
        })
        .map_err(|_| ())?;
    let mut state = slot.wait_until(deadline);
    if let Some(output) = state.take() {
        return Ok(output);
    }
    let stop = match jobs.admit(&call.name, started) {
        Ok(Some(id)) => {
            unlink();
            *state = SlotState::Offloaded(id.clone());
            drop(state);
            return Ok(offloaded(&id, &call.name, begun.elapsed()));
        }
        Ok(None) => Stop::JobsFull,
        Err(e) => {
            eprintln!("ulei-engine: could not record a background job: {e}");
            Stop::Stopped
        }
    };
    drop(state);
    interrupt.interrupt();
    slot.wait_until_done();
    Ok(over_budget(&call.name, limit, stop))
}

/// Runs `call` in `cwd` under `interrupt` on a scoped thread and waits for it until `deadline`;
/// past it the interrupt stops the call, whose thread is waited for and whose result is dropped.
fn stoppable(
    tools: &Registry,
    call: &ToolCall,
    cwd: &Path,
    interrupt: &Interrupt,
    deadline: Instant,
    limit: Duration,
) -> Output {
    let ctx = Context { cwd, interrupt };
    thread::scope(|scope| {
        let (send, receive) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("call".into())
            .spawn_scoped(scope, move || {
                let _ = send.send(tools.execute(&call.name, &call.input, ctx));
            });
        if spawned.is_err() {
            return tools.execute(&call.name, &call.input, ctx);
        }
        match receive.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(output) => output,
            Err(RecvTimeoutError::Timeout) => {
                interrupt.interrupt();
                over_budget(&call.name, limit, Stop::Stopped)
            }
            // The thread panicked; the scope carries the panic on.
            Err(RecvTimeoutError::Disconnected) => Output::error("no result"),
        }
    })
}

/// Where the result of a call that may be offloaded goes.
#[derive(Debug, Default)]
struct Slot {
    state: Mutex<SlotState>,
    done: Condvar,
}

#[derive(Debug, Default)]
enum SlotState {
    #[default]
    Waiting,
    /// The call ended before it was offloaded.
    Done(Output),
    /// The call goes on as this job.
    Offloaded(String),
}

impl SlotState {
    /// The result of a call that ended, taken.
    fn take(&mut self) -> Option<Output> {
        match std::mem::take(self) {
            Self::Done(output) => Some(output),
            other => {
                *self = other;
                None
            }
        }
    }
}

impl Slot {
    /// Hands over the result of the ended call: back with the job id when the call was
    /// offloaded, for the job to record, else to the waiter.
    fn deliver(&self, output: Output) -> Option<(String, Output)> {
        let mut state = lock(&self.state);
        if let SlotState::Offloaded(id) = &*state {
            return Some((id.clone(), output));
        }
        *state = SlotState::Done(output);
        self.done.notify_all();
        None
    }

    /// The state once the call ended or `deadline` passed, locked.
    fn wait_until(&self, deadline: Instant) -> MutexGuard<'_, SlotState> {
        let mut state = lock(&self.state);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if matches!(*state, SlotState::Done(_)) || left.is_zero() {
                return state;
            }
            state = self
                .done
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Waits until the call ended.
    fn wait_until_done(&self) {
        let state = lock(&self.state);
        drop(
            self.done
                .wait_while(state, |state| !matches!(state, SlotState::Done(_)))
                .unwrap_or_else(PoisonError::into_inner),
        );
    }
}

/// Why a call ended at the budget without its result.
#[derive(Debug, Clone, Copy)]
enum Stop {
    /// It ran past the budget and was stopped.
    Stopped,
    /// It could have gone on as a job, but [`MAX_JOBS`] ran.
    JobsFull,
    /// The budget ran out before it started.
    NotStarted,
}

/// The `timeout` result of a call of `tool` that ended at the budget `limit` for `stop`.
fn over_budget(tool: &str, limit: Duration, stop: Stop) -> Output {
    let (reason, error) = match stop {
        Stop::Stopped => (
            "stopped",
            "The call ran past the owner call budget and was stopped before it finished; \
             whatever it did before it stopped stays done. Tell the owner it did not finish."
                .to_owned(),
        ),
        Stop::JobsFull => (
            "jobs_full",
            format!(
                "The call ran past the owner call budget while {MAX_JOBS} background jobs \
                 already ran, so it could not go on as one and was stopped before it finished. \
                 Tell the owner it did not finish."
            ),
        ),
        Stop::NotStarted => (
            "not_started",
            "The owner call budget ran out before this call started; it was not run.".to_owned(),
        ),
    };
    Output {
        content: json_object([
            ("status", json_str("timeout")),
            ("tool", json_str(tool)),
            ("budget_s", seconds(limit)),
            ("reason", json_str(reason)),
            ("error", json_str(&error)),
        ]),
        is_error: true,
    }
}

/// The `offloaded` result of a call of `tool` that went on as job `id` after `elapsed`.
fn offloaded(id: &str, tool: &str, elapsed: Duration) -> Output {
    Output::ok(json_object([
        ("status", json_str("offloaded")),
        ("job_id", json_str(id)),
        ("tool", json_str(tool)),
        ("elapsed_s", seconds(elapsed)),
        ("note", json_str(OFFLOADED_NOTE)),
    ]))
}

/// `duration` in seconds with one decimal.
fn seconds(duration: Duration) -> String {
    format!("{:.1}", duration.as_secs_f64())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
