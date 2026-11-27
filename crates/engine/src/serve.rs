// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The C6 server of `ulei-engine serve` (ADR-0008, ADR-0028): HTTP/1.1 JSON over a Unix
//! socket, one request per connection, answered on the accepting thread except a held
//! `turn_events` or `inbox_turns`, which waits on a thread of its own (ADR-0031, ADR-0036). A
//! session runs one turn at a time on a thread of its own and queues the turns submitted
//! meanwhile; turns of different sessions run at the same time. `interrupt` stops the running
//! turn at once; `inject` records an inbox event in the journal and starts its turn in the free
//! owner session, which `inbox_turns` hands over to the channel and `report_delivery` settles in
//! that record; `set_route` stores the alias the session's later turns run on, with `global`
//! also as the default route of every session that stores none (ADR-0094); `raw_sessions`
//! pages through the journal's raw dialogue (ADR-0051); `job_results` and `jobs_done` hand the
//! ended background jobs to the owner inbox (ADR-0102).

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::{self, DirBuilder};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use ulei_contracts::generated::c6::{
    Ack, Delivery, DeliveryReport, Empty, Error as WireError, ErrorCode, IdleStatus, InboxTurns,
    InboxTurnsRequest, InjectRequest, InjectResponse, InterruptRequest, InterruptResponse,
    JobResults, JobResultsRequest, JobsDone, RawMessage, RawSessionsPage, RawSessionsQuery,
    RawSkipped, RouteSet, SessionStatus, SetRouteRequest, StatusRequest, SubmitTurnRequest,
    SubmitTurnResponse, TurnEvent, TurnEventType, TurnEventsPage, TurnEventsRequest, TurnId,
    TurnOutcome, TurnReport, TurnState,
};
use ulei_contracts::generated::c8::{InboxDelivery, InboxDeliveryDelivery};

use crate::gateway::{self, Conn, MAX_BODY, Malformed};
use crate::jobs::Jobs;
use crate::journal::raw::{Raw, Window};
use crate::journal::{self, Journal};
use crate::policy::Sender;
use crate::tools::{Interrupt, Reaction};

/// Longest socket path in bytes: `sun_path` holds 108 bytes with the trailing NUL.
pub const MAX_SOCKET_PATH: usize = 107;
/// How long one connection may take, from `accept`, to send its whole request.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Most turns waiting in one session's queue (`_BUSY_QUEUE_MAX_PENDING`).
pub const MAX_QUEUE: usize = 32;
/// Ended turns of one session whose reports are kept.
pub const KEPT_REPORTS: usize = 64;
/// Seconds after which an `inject` refused by a session that is not free asks for the event
/// again: the C1 retry delay the core uses (`RETRY_DELAY`).
pub const RETRY_AFTER_S: u64 = 15;
/// Most messages of one `raw_sessions` page, C6 `RawSessionsQuery.limit` at its maximum.
pub const MAX_RAW_MESSAGES: u64 = 10_000;
/// Results of a `job_results` answer that names no `limit`.
pub const JOB_RESULTS_LIMIT: u64 = 16;
/// Largest `limit` of a `job_results` request and most jobs of a `jobs_done`.
pub const MAX_JOB_RESULTS: u64 = 64;

/// Runs the turns the server starts.
pub trait Runner: Send + Sync {
    /// Runs and journals `input` as `ulei-engine run` does, ending it early when `interrupt`
    /// fires and handing `interim` the text of each interim assistant message, and says how
    /// it ended.
    fn run(&self, input: &Input, interrupt: &Interrupt, interim: &mut dyn FnMut(&str)) -> Ending;

    /// Whether the model gateway the turns run on lists the route alias `_alias`, checked
    /// before a global `set_route` makes it the default of every session; `Err` with a short
    /// reason when the gateway cannot be asked. A runner without a gateway to ask lists every
    /// alias.
    ///
    /// # Errors
    /// The gateway could not be reached or answered off its contract.
    fn lists_route(&self, _alias: &str) -> Result<bool, String> {
        Ok(true)
    }
}

/// A turn the server runs: a prompt `submit_turn` took, or an inbox event `inject` took; the
/// turn of an [`Sender::Event`] input is an inbox turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    /// The session, the prompt, its channel and whether it starts a new session.
    pub request: SubmitTurnRequest,
    /// Who wrote the prompt: the owner for `submit_turn`, an event for `inject`.
    pub sender: Sender,
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending {
    /// Its C6 outcome.
    pub outcome: TurnOutcome,
    /// The one line it reports for `quota_exhausted`, `context_exhausted` and `failed`.
    pub line: Option<String>,
    /// The text of the message that ended a `completed`, `max_steps` or `halted` turn; none
    /// for the other outcomes.
    pub answer: Option<String>,
    /// The reactions its `react_to_message` calls recorded, in call order (ADR-0088).
    pub reactions: Vec<Reaction>,
    /// Unix time of the nearest quota reset the gateway named for a `quota_exhausted` turn.
    pub resets_at: Option<u64>,
    /// Whether the turn ran a tool call; none when the runner does not know.
    pub tools_ran: Option<bool>,
}

/// Why the socket could not be bound.
#[derive(Debug)]
pub enum BindError {
    /// The path is longer than [`MAX_SOCKET_PATH`] bytes.
    PathTooLong(usize),
    /// The socket's directory is not owned by the engine's uid or is open to other users.
    OpenDirectory(PathBuf),
    /// Something other than a stale socket exists at the path.
    Occupied,
    /// Filesystem or socket failure.
    Io(io::Error),
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathTooLong(len) => write!(
                f,
                "socket path has {len} bytes, at most {MAX_SOCKET_PATH} are allowed"
            ),
            Self::OpenDirectory(dir) => write!(
                f,
                "socket directory {} must belong to this user with mode 0700 or stricter",
                dir.display()
            ),
            Self::Occupied => f.write_str(
                "socket path exists and is not a stale socket (a live socket or another file)",
            ),
            Self::Io(e) => write!(f, "socket: {e}"),
        }
    }
}

impl std::error::Error for BindError {}

impl From<io::Error> for BindError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Binds and listens at `path` with mode 0600, in a directory of `uid` that no other user can
/// enter; a missing directory is created 0700. An existing path is replaced only when it is a
/// socket that refuses a connection (stale).
///
/// # Errors
/// [`BindError`].
pub fn bind(path: &Path, uid: u32) -> Result<UnixListener, BindError> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(BindError::PathTooLong(len));
    }
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let meta = fs::metadata(dir)?;
    if meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(BindError::OpenDirectory(dir.to_owned()));
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => match UnixStream::connect(path) {
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(path)?,
            _ => return Err(BindError::Occupied),
        },
        Ok(_) => return Err(BindError::Occupied),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Whether `text` is a qb route alias, C4G `TurnRequest.route`: `^[a-z][a-z0-9_-]{0,63}$`.
pub fn is_route_alias(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && text.len() <= 64
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// A C6 method the server serves.
#[derive(Debug, Clone, Copy)]
enum Method {
    InboxTurns,
    Inject,
    Interrupt,
    IsIdle,
    JobResults,
    JobsDone,
    RawSessions,
    ReportDelivery,
    SetRoute,
    Status,
    SubmitTurn,
    TurnEvents,
}

/// An HTTP status and its JSON body.
type Reply = (u16, Vec<u8>);

/// The sessions' turns and the condition a turn's thread signals when it appends an event.
struct Shared {
    lanes: Mutex<Lanes>,
    appended: Condvar,
}

/// The sessions' turns: running, queued and the reports and events of the ended ones.
struct Lanes {
    /// Unix time of the server's start.
    started: f64,
    /// The server's start in microseconds, the first part of every [`TurnId`] it gives.
    epoch: u128,
    /// Turns accepted so far.
    accepted: u64,
    sessions: HashMap<String, Lane>,
    /// `turn_events` and `inbox_turns` requests so far; each request's number is its ticket.
    tickets: u64,
    /// The ticket of the latest `turn_events` request of each turn that one still waits for.
    holds: HashMap<TurnId, u64>,
    /// The ticket of the `inbox_turns` request that waits for the owner session's next inbox
    /// turn on a connection still open.
    inbox_hold: Option<u64>,
}

impl Lanes {
    /// A fresh id: `<epoch>-<n>`, the `n`th turn accepted since the start.
    fn next_id(&mut self) -> TurnId {
        self.accepted += 1;
        format!("{}-{}", self.epoch, self.accepted)
    }

    /// The kept turn `turn` of `session` when it is a handed-over inbox turn without a
    /// delivery.
    fn unreported(&mut self, session: &str, turn: &str) -> Option<&mut Kept> {
        self.sessions
            .get_mut(session)?
            .ended
            .iter_mut()
            .find(|kept| {
                kept.report.turn == turn
                    && kept
                        .inbox
                        .as_ref()
                        .is_some_and(|inbox| inbox.handover == Handover::Done)
                    && kept.report.delivery.is_none()
            })
    }

    /// The answer of the `inbox_turns` request of `ticket` for session `owner`: no turn once a
    /// newer request released it, else the oldest inbox turn not handed over, then marked as
    /// being written and the hold ended; none while it has to wait.
    fn inbox_answer(&mut self, owner: &str, ticket: u64) -> Option<InboxTurns> {
        if self.inbox_hold != Some(ticket) {
            return Some(InboxTurns { turn: None });
        }
        let (turn, handover) = self
            .sessions
            .get_mut(owner)?
            .handovers()
            .find(|(_, handover)| **handover == Handover::Waiting)?;
        *handover = Handover::Writing;
        let turn = Some(turn.clone());
        self.inbox_hold = None;
        Some(InboxTurns { turn })
    }

    /// Marks inbox turn `turn` of `owner` handed over when its answer was `written`, else
    /// waiting for the next request.
    fn handed(&mut self, owner: &str, turn: &str, written: bool) {
        let handover = self
            .sessions
            .get_mut(owner)
            .and_then(|lane| lane.handovers().find(|(id, _)| *id == turn));
        if let Some((_, handover)) = handover {
            *handover = if written {
                Handover::Done
            } else {
                Handover::Waiting
            };
        }
    }

    /// The state of `turn` of `session` and its events; a queued or unknown turn has none.
    fn events(&self, session: &str, turn: &str) -> (TurnState, &[TurnEvent]) {
        match self.sessions.get(session) {
            Some(lane) => lane.events(turn),
            None => (TurnState::Unknown, &[]),
        }
    }

    /// The state of the turn `request` names and its events after `after`; none while the
    /// request of `ticket` is the turn's latest and the turn has no such event yet but may.
    fn page(
        &mut self,
        request: &TurnEventsRequest,
        ticket: u64,
    ) -> Option<(TurnState, Vec<TurnEvent>)> {
        let current = self.holds.get(&request.turn) == Some(&ticket);
        let (state, events) = self.events(&request.session, &request.turn);
        let after = usize::try_from(request.after).unwrap_or(usize::MAX);
        let fresh = events.get(after..).unwrap_or_default();
        let last = matches!(
            state,
            TurnState::Ended | TurnState::Dropped | TurnState::Unknown
        );
        if current && fresh.is_empty() && !last {
            return None;
        }
        let page = (state, fresh.to_vec());
        if current {
            self.holds.remove(&request.turn);
        }
        Some(page)
    }
}

/// The event of an inbox turn and how far the turn has been handed over.
struct Inbox {
    event_id: String,
    handover: Handover,
}

/// How far an inbox turn has been handed over to the channel that holds `inbox_turns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handover {
    Waiting,
    /// An `inbox_turns` answer naming it is being written.
    Writing,
    Done,
}

/// The turn a session runs.
struct Running {
    turn: TurnId,
    /// Unix time it started.
    since: f64,
    /// Fired by `interrupt`; one handle per turn, so a later turn starts uninterrupted.
    interrupt: Interrupt,
    /// Its events so far, from `start`.
    events: Vec<TurnEvent>,
    /// Its event and handover when it is an inbox turn.
    inbox: Option<Inbox>,
}

impl Running {
    /// The turn `turn` started at `since`; the inbox turn of `event_id` when one is given.
    fn new(turn: TurnId, since: f64, event_id: Option<String>) -> Self {
        Self {
            turn,
            since,
            interrupt: Interrupt::new(),
            events: vec![event(1, TurnEventType::Start)],
            inbox: event_id.map(|event_id| Inbox {
                event_id,
                handover: Handover::Waiting,
            }),
        }
    }

    /// Appends `interim` with `text` unless the turn's interrupt fired
    /// (`_run_still_current`).
    fn interim(&mut self, text: &str) {
        if self.interrupt.is_interrupted() {
            return;
        }
        let seq = self.events.len() as u64 + 1;
        self.events.push(TurnEvent {
            text: Some(text.to_owned()),
            ..event(seq, TurnEventType::Interim)
        });
    }

    /// The report and events of the turn ended with `ending` at `ended`: `final` with a
    /// non-empty answer (`_process_message_background` sends no empty response), then `end`.
    fn end(self, ending: Ending, ended: f64) -> Kept {
        let mut events = self.events;
        let next = |events: &Vec<TurnEvent>| events.len() as u64 + 1;
        for reaction in ending.reactions {
            events.push(TurnEvent {
                text: Some(reaction.emoji),
                message_id: Some(reaction.message_id),
                ..event(next(&events), TurnEventType::Reaction)
            });
        }
        if let Some(text) = ending.answer.filter(|text| !text.is_empty()) {
            events.push(TurnEvent {
                text: Some(text),
                ..event(next(&events), TurnEventType::Final)
            });
        }
        events.push(TurnEvent {
            outcome: Some(ending.outcome),
            line: ending.line.clone(),
            ..event(next(&events), TurnEventType::End)
        });
        Kept {
            report: TurnReport {
                turn: self.turn,
                state: TurnState::Ended,
                outcome: Some(ending.outcome),
                ended: Some(ended),
                line: ending.line,
                resets_at: ending.resets_at,
                tools_ran: ending.tools_ran,
                delivery: None,
            },
            events,
            inbox: self.inbox,
        }
    }
}

/// An ended or dropped turn's report and events.
struct Kept {
    report: TurnReport,
    events: Vec<TurnEvent>,
    /// Its event and handover when it is an inbox turn; a dropped turn never is one.
    inbox: Option<Inbox>,
}

/// One session key's turns.
#[derive(Default)]
struct Lane {
    running: Option<Running>,
    queue: VecDeque<(TurnId, Input)>,
    /// The last [`KEPT_REPORTS`] ended or dropped turns, oldest first.
    ended: VecDeque<Kept>,
}

impl Lane {
    /// Whether a turn runs or waits.
    fn busy(&self) -> bool {
        self.running.is_some() || !self.queue.is_empty()
    }

    /// The report of turn `id`; `unknown` when the lane does not hold it.
    fn report(&self, id: &str) -> TurnReport {
        match self.ended.iter().find(|k| k.report.turn == id) {
            Some(kept) => kept.report.clone(),
            None => bare_report(id.to_owned(), self.events(id).0),
        }
    }

    /// The state and events of turn `id`; a queued or unknown turn has none.
    fn events(&self, id: &str) -> (TurnState, &[TurnEvent]) {
        if let Some(running) = self.running.as_ref().filter(|r| r.turn == id) {
            (TurnState::Running, &running.events)
        } else if self.queue.iter().any(|(q, _)| q == id) {
            (TurnState::Queued, &[])
        } else if let Some(kept) = self.ended.iter().find(|k| k.report.turn == id) {
            (kept.report.state, &kept.events)
        } else {
            (TurnState::Unknown, &[])
        }
    }

    /// Keeps `kept`, forgetting the oldest kept turn past [`KEPT_REPORTS`].
    fn keep(&mut self, kept: Kept) {
        if self.ended.len() == KEPT_REPORTS {
            self.ended.pop_front();
        }
        self.ended.push_back(kept);
    }

    /// The handover of each inbox turn the lane holds, oldest first.
    fn handovers(&mut self) -> impl Iterator<Item = (&TurnId, &mut Handover)> {
        let ended = self
            .ended
            .iter_mut()
            .filter_map(|kept| Some((&kept.report.turn, &mut kept.inbox.as_mut()?.handover)));
        let running = self
            .running
            .iter_mut()
            .filter_map(|running| Some((&running.turn, &mut running.inbox.as_mut()?.handover)));
        ended.chain(running)
    }

    /// The report of the turn that ended last; a dropped turn never ran, so it is not one.
    fn last_turn(&self) -> Option<&TurnReport> {
        self.ended
            .iter()
            .rev()
            .map(|k| &k.report)
            .find(|r| r.state == TurnState::Ended)
    }
}

/// The server: the journal that holds the sessions' routes, their `last_turn` after a restart
/// and the inbox delivery records, the sessions' turns and the settings read at its start.
pub struct Server {
    journal: RefCell<Journal>,
    shared: Arc<Shared>,
    runner: Arc<dyn Runner>,
    owner: String,
    route: String,
    uid: u32,
    timeout: Duration,
    jobs: Option<Jobs>,
}

impl Server {
    /// A server whose turns `runner` runs, taking `inject` and answering `is_idle` for session
    /// key `owner`, with `route` as the route of a session that stores none, for peers of `uid`;
    /// it closes a connection whose request has not fully arrived within `timeout`.
    pub fn new(
        journal: Journal,
        runner: Arc<dyn Runner>,
        (owner, route): (String, String),
        uid: u32,
        timeout: Duration,
    ) -> Self {
        let start = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            journal: RefCell::new(journal),
            shared: Arc::new(Shared {
                lanes: Mutex::new(Lanes {
                    started: start.as_secs_f64(),
                    epoch: start.as_micros(),
                    accepted: 0,
                    sessions: HashMap::new(),
                    tickets: 0,
                    holds: HashMap::new(),
                    inbox_hold: None,
                }),
                appended: Condvar::new(),
            }),
            runner,
            owner,
            route,
            uid,
            timeout,
            jobs: None,
        }
    }

    /// The server that answers `job_results` and `jobs_done` from `jobs`; without jobs they
    /// hold none.
    #[must_use]
    pub fn with_jobs(mut self, jobs: Option<Jobs>) -> Self {
        self.jobs = jobs;
        self
    }

    /// Serves connections one at a time, blocking in `accept`; returns only on an accept
    /// failure other than an aborted connection.
    pub fn run(&self, listener: &UnixListener) -> io::Error {
        loop {
            match listener.accept() {
                Ok((stream, _)) => self.serve(&stream),
                Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {}
                Err(e) => return e,
            }
        }
    }

    /// Serves one accepted connection: checks the peer uid before reading, then reads one
    /// request, answers it with `Connection: close` and closes. A connection that fails or
    /// times out is closed without an answer. A `turn_events` or `inbox_turns` that has to wait
    /// is answered from a thread of its own, which holds the connection until then.
    pub fn serve(&self, stream: &UnixStream) {
        let answer = match rustix::net::sockopt::socket_peercred(stream) {
            Ok(cred) if cred.uid.as_raw() == self.uid => self.answer(stream),
            Ok(_) => Some(error(
                ErrorCode::Forbidden,
                "peer uid differs from the engine",
            )),
            Err(_) => Some(error(ErrorCode::Internal, "peer credentials unavailable")),
        };
        if let Some(reply) = answer {
            self.send(stream, &reply);
        }
    }

    /// Writes `reply` within the request timeout; a failed write is a client gone.
    fn send(&self, stream: &UnixStream, reply: &Reply) {
        send(stream, reply, self.timeout);
    }

    /// Answers `inbox_turns` at once when an inbox turn waits for its handover, else hands
    /// `stream` to a thread that waits for one; a new request releases the held one, and so does
    /// the hang-up of its connection.
    fn inbox_turns(&self, stream: &UnixStream, input: Value) -> Result<Option<Reply>, Reply> {
        let request: InboxTurnsRequest =
            serde_json::from_value(input).map_err(|_| invalid_body())?;
        if request.session != self.owner {
            return Err(error(
                ErrorCode::BadRequest,
                "session is not the owner session",
            ));
        }
        let (ticket, ready) = {
            let mut lanes = lock(&self.shared.lanes);
            lanes.tickets += 1;
            let ticket = lanes.tickets;
            lanes.inbox_hold = Some(ticket);
            self.shared.appended.notify_all();
            (ticket, lanes.inbox_answer(&self.owner, ticket))
        };
        if let Some(answer) = ready {
            hand_over(&self.shared, &self.owner, stream, answer, self.timeout);
            return Ok(None);
        }
        let failed = || {
            let mut lanes = lock(&self.shared.lanes);
            if lanes.inbox_hold == Some(ticket) {
                lanes.inbox_hold = None;
            }
            error(ErrorCode::Internal, "cannot hold the answer")
        };
        let held = stream.try_clone().map_err(|_| failed())?;
        let watched = stream.try_clone().map_err(|_| failed())?;
        watched.set_read_timeout(None).map_err(|_| failed())?;
        let shared = Arc::clone(&self.shared);
        std::thread::Builder::new()
            .spawn(move || watch_inbox(&shared, &watched, ticket))
            .map_err(|_| failed())?;
        let (shared, owner, timeout) = (Arc::clone(&self.shared), self.owner.clone(), self.timeout);
        std::thread::Builder::new()
            .spawn(move || {
                let answer = hold_inbox(&shared, &owner, ticket);
                hand_over(&shared, &owner, &held, answer, timeout);
                let _ = held.shutdown(Shutdown::Read);
            })
            .map_err(|_| {
                let _ = stream.shutdown(Shutdown::Read);
                failed()
            })?;
        Ok(None)
    }

    fn answer(&self, stream: &UnixStream) -> Option<Reply> {
        let deadline = Instant::now() + self.timeout;
        let mut conn = Conn::new(stream, Arc::new(AtomicBool::new(false)), self.timeout);
        conn.deadline = Some(deadline);
        let (line, length) = match conn.request_head() {
            Ok(head) => head,
            Err(e) => return refusal(&e),
        };
        let Some(length) = length else {
            return Some(error(ErrorCode::BadRequest, "Content-Length is required"));
        };
        if length > MAX_BODY {
            // Closing with the body unread would reset the client's read of the answer, and a
            // client still writing the body would not read it at all.
            self.send(stream, &error(ErrorCode::TooLarge, "body over 512 KiB"));
            let _ = stream.shutdown(Shutdown::Write);
            conn.discard(length);
            return None;
        }
        // The body is read before the method is known: a socket closed with unread bytes
        // resets the client's read of the answer.
        let body = match conn.body(Some(length), MAX_BODY) {
            Ok(body) => body,
            Err(e) => return refusal(&e),
        };
        method(&line)
            .and_then(|method| self.dispatch(method, &body, (deadline, stream)))
            .unwrap_or_else(Some)
    }

    /// Answers `turn_events` at once when its page is ready, else hands `stream` to a thread
    /// that waits for it; a request releases the held one of the same turn.
    fn turn_events(&self, stream: &UnixStream, input: Value) -> Result<Option<Reply>, Reply> {
        let request: TurnEventsRequest =
            serde_json::from_value(input).map_err(|_| invalid_body())?;
        if journal::check_key(&request.session).is_err() || !is_turn_id(&request.turn) {
            return Err(invalid_body());
        }
        let (ticket, ready) = {
            let mut lanes = lock(&self.shared.lanes);
            lanes.tickets += 1;
            let ticket = lanes.tickets;
            lanes.holds.insert(request.turn.clone(), ticket);
            self.shared.appended.notify_all();
            (ticket, lanes.page(&request, ticket))
        };
        if let Some(page) = ready {
            return page_reply(&request.turn, page).map(Some);
        }
        let turn = request.turn.clone();
        let failed = || {
            let mut lanes = lock(&self.shared.lanes);
            if lanes.holds.get(&turn) == Some(&ticket) {
                lanes.holds.remove(&turn);
            }
            error(ErrorCode::Internal, "cannot hold the answer")
        };
        let held = stream.try_clone().map_err(|_| failed())?;
        let (shared, timeout) = (Arc::clone(&self.shared), self.timeout);
        std::thread::Builder::new()
            .spawn(move || {
                let page = hold(&shared, &request, ticket);
                let reply = page_reply(&request.turn, page).unwrap_or_else(|e| e);
                send(&held, &reply, timeout);
            })
            .map_err(|_| failed())?;
        Ok(None)
    }

    /// Runs `method` on its input `body` before `deadline`; `Err` is a C6 `Error` reply and
    /// none is a `turn_events` or `inbox_turns` whose answer is written to `stream` elsewhere.
    fn dispatch(
        &self,
        method: Method,
        body: &[u8],
        (deadline, stream): (Instant, &UnixStream),
    ) -> Result<Option<Reply>, Reply> {
        let fields = match serde_json::from_slice::<Value>(body) {
            Ok(Value::Object(fields)) if !fields.values().any(Value::is_null) => fields,
            _ => return Err(invalid_body()),
        };
        let input = Value::Object(fields);
        let out = match method {
            Method::InboxTurns => return self.inbox_turns(stream, input),
            Method::Inject => {
                let event: InjectRequest =
                    serde_json::from_value(input).map_err(|_| invalid_body())?;
                if !is_event_id(&event.event_id) || event.prompt.is_empty() || event.attempt == 0 {
                    return Err(invalid_body());
                }
                serde_json::to_vec(&self.inject(event, deadline)?)
            }
            Method::Interrupt => {
                let request: InterruptRequest =
                    serde_json::from_value(input).map_err(|_| invalid_body())?;
                if journal::check_key(&request.session).is_err() {
                    return Err(invalid_body());
                }
                serde_json::to_vec(&self.interrupt(&request.session))
            }
            Method::IsIdle => {
                let Empty {} = serde_json::from_value(input).map_err(|_| invalid_body())?;
                serde_json::to_vec(&self.is_idle(deadline)?)
            }
            Method::JobResults => {
                let request: JobResultsRequest =
                    serde_json::from_value(input).map_err(|_| invalid_body())?;
                serde_json::to_vec(&self.job_results(&request, deadline)?)
            }
            Method::JobsDone => {
                let done: JobsDone = serde_json::from_value(input).map_err(|_| invalid_body())?;
                serde_json::to_vec(&self.jobs_done(&done, deadline)?)
            }
            Method::RawSessions => {
                let query: RawSessionsQuery =
                    serde_json::from_value(input).map_err(|_| invalid_body())?;
                return self.raw_sessions(&query, deadline).map(Some);
            }
            Method::ReportDelivery => {
                let report: DeliveryReport =
                    serde_json::from_value(input).map_err(|_| invalid_body())?;
                if journal::check_key(&report.session).is_err() || !is_turn_id(&report.turn) {
                    return Err(invalid_body());
                }
                serde_json::to_vec(&self.report_delivery(&report, deadline))
            }
            Method::SetRoute => {
                let request: SetRouteRequest =
                    serde_json::from_value(input).map_err(|_| invalid_body())?;
                if journal::check_key(&request.session).is_err() || !is_route_alias(&request.route)
                {
                    return Err(invalid_body());
                }
                serde_json::to_vec(&self.set_route(request, deadline)?)
            }
            Method::Status => {
                let request: StatusRequest =
                    serde_json::from_value(input).map_err(|_| invalid_body())?;
                if journal::check_key(&request.session).is_err()
                    || request.turn.as_deref().is_some_and(|t| !is_turn_id(t))
                {
                    return Err(invalid_body());
                }
                serde_json::to_vec(&self.status(request, deadline)?)
            }
            Method::SubmitTurn => {
                let turn: SubmitTurnRequest =
                    serde_json::from_value(input).map_err(|_| invalid_body())?;
                if journal::check_key(&turn.session).is_err()
                    || turn.prompt.is_empty()
                    || turn.channel.as_deref().is_some_and(str::is_empty)
                    || turn.context.as_deref().is_some_and(str::is_empty)
                {
                    return Err(invalid_body());
                }
                serde_json::to_vec(&self.submit(turn)?)
            }
            Method::TurnEvents => return self.turn_events(stream, input),
        }
        .map_err(|_| error(ErrorCode::Internal, "answer does not serialize"))?;
        Ok(Some((200, out)))
    }

    /// Whether the owner key has a session in the journal, read before `deadline`. A session
    /// is never removed, so the answer stays true once it is.
    fn owner_journaled(&self, deadline: Instant) -> Result<bool, Reply> {
        self.journal
            .borrow()
            .has_session(
                &self.owner,
                deadline.saturating_duration_since(Instant::now()),
            )
            .map_err(|_| error(ErrorCode::Internal, "journal read failed"))
    }

    /// Whether the owner session is free for an inbox turn: it runs and queues none, a channel
    /// holds `inbox_turns` for it and its key is `journaled` (`Bridge.tick`, `Bridge.blocked`).
    fn free(&self, lanes: &Lanes, journaled: bool) -> bool {
        journaled
            && lanes.inbox_hold.is_some()
            && !lanes.sessions.get(&self.owner).is_some_and(Lane::busy)
    }

    /// Whether a channel holds `inbox_turns` for the owner session or follows the inbox turn
    /// of that hold, which the session runs.
    fn channel(&self, lanes: &Lanes) -> bool {
        let follows = || {
            lanes
                .sessions
                .get(&self.owner)
                .and_then(|lane| lane.running.as_ref()?.inbox.as_ref())
                .is_some_and(|inbox| inbox.handover != Handover::Waiting)
        };
        lanes.inbox_hold.is_some() || follows()
    }

    fn is_idle(&self, deadline: Instant) -> Result<IdleStatus, Reply> {
        let journaled = self.owner_journaled(deadline)?;
        let lanes = lock(&self.shared.lanes);
        let channel = Some(self.channel(&lanes));
        if !self.free(&lanes, journaled) {
            return Ok(IdleStatus {
                idle: false,
                since: None,
                channel,
            });
        }
        let lane = lanes.sessions.get(&self.owner);
        Ok(IdleStatus {
            idle: true,
            since: Some(lane.and_then(last_end).unwrap_or(lanes.started)),
            channel,
        })
    }

    /// The `200` answer of `query`: the journal's raw messages after its cursor, read before
    /// `deadline`, at most its `limit` and as many as fit [`MAX_BODY`], with a cursor when more
    /// follow. A message that does not fit an answer alone is named in `skipped` without its
    /// bytes, so no message stalls the reader.
    fn raw_sessions(&self, query: &RawSessionsQuery, deadline: Instant) -> Result<Reply, Reply> {
        let bound = |t: Option<f64>| t.is_none_or(|t| t.is_finite() && t >= 0.0);
        let after = match query.cursor.as_deref() {
            None => Some(0),
            Some(c) if !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()) => c.parse().ok(),
            Some(_) => None,
        };
        let limit = query.limit.unwrap_or(MAX_RAW_MESSAGES);
        let (Some(after), true, true, 1..=MAX_RAW_MESSAGES) =
            (after, bound(query.since), bound(query.until), limit)
        else {
            return Err(invalid_body());
        };
        let unserializable = || error(ErrorCode::Internal, "answer does not serialize");
        let size_of = |page: &RawSessionsPage| {
            serde_json::to_vec(page)
                .map(|bytes| bytes.len())
                .map_err(|_| unserializable())
        };
        let mut page = RawSessionsPage {
            messages: Vec::new(),
            skipped: None,
            cursor: Some(u64::MAX.to_string()),
        };
        let base = size_of(&page)?;
        let skipped_field = size_of(&RawSessionsPage {
            skipped: Some(Vec::new()),
            ..page.clone()
        })? - base;
        let mut skipped = Vec::new();
        let mut size = base;
        let (mut last, mut more, mut failed) = (None, false, false);
        let window = Window {
            since: query.since,
            until: query.until,
        };
        let count = usize::try_from(limit).unwrap_or(usize::MAX);
        self.journal
            .borrow()
            .raw(
                after,
                window,
                count + 1,
                deadline.saturating_duration_since(Instant::now()),
                |raw| {
                    if page.messages.len() + skipped.len() == count {
                        more = true;
                        return false;
                    }
                    let n = raw.n;
                    let Ok((entry, bytes)) = raw_entry(raw, base) else {
                        failed = true;
                        return false;
                    };
                    size += bytes
                        + match &entry {
                            Ok(_) => usize::from(!page.messages.is_empty()),
                            Err(_) if skipped.is_empty() => skipped_field,
                            Err(_) => 1,
                        };
                    if size > MAX_BODY {
                        more = true;
                        return false;
                    }
                    match entry {
                        Ok(message) => page.messages.push(message),
                        Err(skip) => skipped.push(skip),
                    }
                    last = Some(n);
                    true
                },
            )
            .map_err(|_| error(ErrorCode::Internal, "journal read failed"))?;
        if failed {
            return Err(unserializable());
        }
        if more && last.is_none() {
            return Err(error(
                ErrorCode::Internal,
                "a skipped message's reference does not fit the body limit",
            ));
        }
        page.skipped = (!skipped.is_empty()).then_some(skipped);
        page.cursor = last.filter(|_| more).map(|n| n.to_string());
        Ok((
            200,
            serde_json::to_vec(&page).map_err(|_| unserializable())?,
        ))
    }

    /// Stores `report.delivery` when its turn is a handed-over inbox turn that has ended and
    /// has none yet: first in the turn's delivery record, committed before `deadline`, then on
    /// the turn's report. A failed write stores nothing.
    fn report_delivery(&self, report: &DeliveryReport, deadline: Instant) -> Ack {
        let refused = |message: Option<&str>| Ack {
            ok: false,
            message: message.map(str::to_owned),
        };
        let event_id = lock(&self.shared.lanes)
            .unreported(&report.session, &report.turn)
            .and_then(|kept| Some(kept.inbox.as_ref()?.event_id.clone()));
        let Some(event_id) = event_id else {
            return refused(None);
        };
        let settled = self.journal.borrow_mut().settle_delivery(
            &event_id,
            &report.turn,
            recorded(report.delivery),
            deadline.saturating_duration_since(Instant::now()),
        );
        match settled {
            Ok(true) => {}
            Ok(false) => return refused(Some("no delivery record names the turn")),
            Err(_) => return refused(Some("journal write failed")),
        }
        if let Some(kept) = lock(&self.shared.lanes).unreported(&report.session, &report.turn) {
            kept.report.delivery = Some(report.delivery);
        }
        Ack {
            ok: true,
            message: None,
        }
    }

    /// The ended jobs `request` asks for, read before `deadline`, oldest first and as many as
    /// fit [`MAX_BODY`].
    fn job_results(
        &self,
        request: &JobResultsRequest,
        deadline: Instant,
    ) -> Result<JobResults, Reply> {
        let limit = request.limit.unwrap_or(JOB_RESULTS_LIMIT);
        if !(1..=MAX_JOB_RESULTS).contains(&limit) {
            return Err(invalid_body());
        }
        let Some(jobs) = &self.jobs else {
            return Ok(JobResults {
                results: Vec::new(),
            });
        };
        let mut results = jobs
            .results(
                usize::try_from(limit).unwrap_or(usize::MAX),
                deadline.saturating_duration_since(Instant::now()),
            )
            .map_err(|_| error(ErrorCode::Internal, "journal read failed"))?;
        let mut size = 16;
        let fit = results
            .iter()
            .take_while(|result| {
                size += serde_json::to_vec(result).map_or(MAX_BODY, |bytes| bytes.len() + 1);
                size <= MAX_BODY
            })
            .count();
        results.truncate(fit);
        Ok(JobResults { results })
    }

    /// Forgets the ended jobs `done` names, written before `deadline`.
    fn jobs_done(&self, done: &JobsDone, deadline: Instant) -> Result<Ack, Reply> {
        let count = done.jobs.len() as u64;
        if !(1..=MAX_JOB_RESULTS).contains(&count) || !done.jobs.iter().all(|id| is_job_id(id)) {
            return Err(invalid_body());
        }
        let Some(jobs) = &self.jobs else {
            return Ok(Ack {
                ok: true,
                message: Some("no job is held".to_owned()),
            });
        };
        jobs.confirm(
            &done.jobs,
            deadline.saturating_duration_since(Instant::now()),
        )
        .map_err(|_| error(ErrorCode::Internal, "journal write failed"))?;
        Ok(Ack {
            ok: true,
            message: None,
        })
    }

    /// The status `request` asks for; a journal read is given up at `deadline`.
    fn status(&self, request: StatusRequest, deadline: Instant) -> Result<SessionStatus, Reply> {
        let journal = self.journal.borrow();
        let failed = |_| error(ErrorCode::Internal, "journal read failed");
        let within = || deadline.saturating_duration_since(Instant::now());
        let route = match journal.route(&request.session, within()).map_err(failed)? {
            Some(route) => route,
            None => journal
                .default_route(within())
                .map_err(failed)?
                .unwrap_or_else(|| self.route.clone()),
        };
        let mut status = {
            let lanes = lock(&self.shared.lanes);
            let lane = lanes.sessions.get(&request.session);
            let running = lane.and_then(|lane| lane.running.as_ref());
            SessionStatus {
                running: running.is_some(),
                since: running
                    .map(|r| r.since)
                    .or_else(|| lane.and_then(last_end))
                    .unwrap_or(lanes.started),
                queue_depth: lane.map_or(0, |lane| lane.queue.len() as u64),
                route,
                last_turn: lane.and_then(Lane::last_turn).cloned(),
                turn: request.turn.map(|id| match lane {
                    Some(lane) => lane.report(&id),
                    None => bare_report(id, TurnState::Unknown),
                }),
                session: request.session,
            }
        };
        if status.last_turn.is_none() {
            let stored = journal
                .last_turn(
                    &status.session,
                    deadline.saturating_duration_since(Instant::now()),
                )
                .map_err(failed)?;
            status.last_turn = stored.map(|turn| {
                bare_report(
                    format!("s{}-t{}", turn.session, turn.number),
                    TurnState::Ended,
                )
            });
        }
        Ok(status)
    }

    /// Starts `turn` when its session runs none, else queues it behind the session's turns
    /// (`_queue_or_replace_pending_event`, `_enqueue_fifo`); a full queue refuses it.
    fn submit(&self, turn: SubmitTurnRequest) -> Result<SubmitTurnResponse, Reply> {
        let mut lanes = lock(&self.shared.lanes);
        let busy = lanes
            .sessions
            .get(&turn.session)
            .is_some_and(|lane| lane.running.is_some());
        let depth = lanes
            .sessions
            .get(&turn.session)
            .map_or(0, |lane| lane.queue.len());
        if busy && depth >= MAX_QUEUE {
            return Ok(SubmitTurnResponse {
                accepted: false,
                turn: None,
                position: None,
                queue_depth: MAX_QUEUE as u64,
            });
        }
        let id = lanes.next_id();
        let input = Input {
            request: turn,
            sender: Sender::Owner,
        };
        if busy {
            let lane = lanes
                .sessions
                .entry(input.request.session.clone())
                .or_default();
            lane.queue.push_back((id.clone(), input));
            let depth = lane.queue.len() as u64;
            return Ok(SubmitTurnResponse {
                accepted: true,
                turn: Some(id),
                position: Some(depth),
                queue_depth: depth,
            });
        }
        self.start(&mut lanes, id.clone(), input, None)?;
        Ok(SubmitTurnResponse {
            accepted: true,
            turn: Some(id),
            position: Some(0),
            queue_depth: 0,
        })
    }

    /// Starts `input` as turn `id` of its session, which runs none, on a thread that then runs
    /// the turns the session queues meanwhile; with `event` it is that event's inbox turn.
    fn start(
        &self,
        lanes: &mut Lanes,
        id: TurnId,
        input: Input,
        event: Option<String>,
    ) -> Result<(), Reply> {
        let lane = lanes
            .sessions
            .entry(input.request.session.clone())
            .or_default();
        let running = Running::new(id, now(), event);
        let interrupt = running.interrupt.clone();
        lane.running = Some(running);
        let (shared, runner) = (Arc::clone(&self.shared), Arc::clone(&self.runner));
        let spawned = std::thread::Builder::new()
            .spawn(move || drive(&shared, runner.as_ref(), input, interrupt));
        if spawned.is_err() {
            lane.running = None;
            return Err(error(ErrorCode::Internal, "cannot start the turn"));
        }
        Ok(())
    }

    /// Delivers inbox event `event` (ADR-0037 section 3): a recorded turn of the event answers
    /// unless the request's attempt is higher and the recorded delivery is `silent`; else, when
    /// the owner session is free, the delivery is recorded and committed, then its turn starts
    /// as an event's prompt and wakes the held `inbox_turns`. Journal reads and writes are given
    /// up at `deadline` and never wait under the lanes' lock.
    fn inject(&self, event: InjectRequest, deadline: Instant) -> Result<InjectResponse, Reply> {
        let accepted = |turn: TurnId| InjectResponse {
            accepted: true,
            session_id: Some(self.owner.clone()),
            retry_after_s: None,
            turn: Some(turn),
        };
        let within = || deadline.saturating_duration_since(Instant::now());
        let recorded = self
            .journal
            .borrow()
            .delivery(&event.event_id, within())
            .map_err(|_| error(ErrorCode::Internal, "journal read failed"))?;
        if let Some(recorded) = recorded.filter(|r| {
            r.attempt >= event.attempt || r.delivery != Some(InboxDeliveryDelivery::Silent)
        }) {
            return Ok(accepted(recorded.turn));
        }
        let journaled = self.owner_journaled(deadline)?;
        // Only this thread adds turns, so the session stays free until the turn starts below.
        let id = {
            let mut lanes = lock(&self.shared.lanes);
            if !self.free(&lanes, journaled) {
                return Ok(InjectResponse {
                    accepted: false,
                    session_id: None,
                    retry_after_s: Some(RETRY_AFTER_S),
                    turn: None,
                });
            }
            lanes.next_id()
        };
        let record = InboxDelivery {
            event_id: event.event_id,
            attempt: event.attempt,
            turn: id.clone(),
            delivery: None,
        };
        self.journal
            .borrow_mut()
            .record_delivery(&record, within())
            .map_err(|_| error(ErrorCode::Internal, "journal write failed"))?;
        let input = Input {
            request: SubmitTurnRequest {
                session: self.owner.clone(),
                prompt: event.prompt,
                new: None,
                channel: None,
                context: None,
                images: None,
                message_id: None,
            },
            sender: Sender::Event,
        };
        let started = self.start(
            &mut lock(&self.shared.lanes),
            id.clone(),
            input,
            Some(record.event_id.clone()),
        );
        if let Err(reply) = started {
            // A record left behind names a turn that never ran, which reads `unknown`.
            let _ = self
                .journal
                .borrow_mut()
                .forget_delivery(&record.event_id, &id, within());
            return Err(reply);
        }
        self.shared.appended.notify_all();
        Ok(accepted(id))
    }

    /// Stores `request.route` with the current session of its key, which a missing session
    /// creates, and for a global request first as the default route, which an alias the
    /// gateway does not list never becomes and the configured alias clears; a journal lock is
    /// given up at `deadline`.
    fn set_route(&self, request: SetRouteRequest, deadline: Instant) -> Result<RouteSet, Reply> {
        let within = || deadline.saturating_duration_since(Instant::now());
        let failed = |_| error(ErrorCode::Internal, "journal write failed");
        let global = request.global.unwrap_or(false);
        if global {
            match self.runner.lists_route(&request.route) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(error(
                        ErrorCode::BadRequest,
                        &format!("unknown route alias {:?}", request.route),
                    ));
                }
                Err(reason) => {
                    return Err(error(
                        ErrorCode::Internal,
                        &format!("could not list the gateway's routes: {reason}"),
                    ));
                }
            }
            let stored = (request.route != self.route).then_some(request.route.as_str());
            self.journal
                .borrow_mut()
                .store_default_route(stored, within())
                .map_err(failed)?;
        }
        self.journal
            .borrow_mut()
            .store_route(&request.session, &request.route, within())
            .map_err(failed)?;
        Ok(RouteSet {
            session: request.session,
            route: request.route,
            global: Some(global),
        })
    }

    /// Hard-interrupts the running turn of `session` and drops its queue head
    /// (`_interrupt_and_clear_session`); answers before the turn has unwound. An idle session,
    /// or one whose running turn is interrupted or finished already, is left as it is.
    fn interrupt(&self, session: &str) -> InterruptResponse {
        let mut lanes = lock(&self.shared.lanes);
        let Some(lane) = lanes.sessions.get_mut(session) else {
            return InterruptResponse {
                interrupted: false,
                turn: None,
                dropped: None,
                queue_depth: 0,
            };
        };
        let fired = match &lane.running {
            Some(running) if running.interrupt.interrupt() => Some(running.turn.clone()),
            _ => None,
        };
        let dropped = fired.as_ref().and_then(|_| lane.queue.pop_front());
        let dropped = dropped.map(|(id, _)| {
            lane.keep(Kept {
                report: TurnReport {
                    ended: Some(now()),
                    ..bare_report(id.clone(), TurnState::Dropped)
                },
                events: vec![event(1, TurnEventType::Dropped)],
                inbox: None,
            });
            self.shared.appended.notify_all();
            id
        });
        InterruptResponse {
            interrupted: fired.is_some(),
            turn: fired,
            dropped,
            queue_depth: lane.queue.len() as u64,
        }
    }
}

/// Runs `input` under `interrupt` and then each turn its session queued meanwhile, each under a
/// handle of its own, in order, until the queue is empty (`_dequeue_pending_event`,
/// `_promote_queued_event`). A turn's outcome is decided as its handle is finished, under the
/// handle's lock: a turn an `interrupt` claimed first ends `interrupted`, and an `interrupt` that
/// comes later neither claims the turn nor drops the queue head. A turn appends its `interim`
/// events while it runs, and none and no `final` once claimed (`_is_session_run_current`).
/// A runner that panics ends its turn `failed` in unwind builds. Every event appended wakes the
/// held `turn_events`.
fn drive(shared: &Shared, runner: &dyn Runner, mut input: Input, mut interrupt: Interrupt) {
    loop {
        let session = input.request.session.clone();
        let mut interim = |text: &str| {
            let mut lanes = lock(&shared.lanes);
            let running = lanes
                .sessions
                .get_mut(&session)
                .and_then(|lane| lane.running.as_mut());
            if let Some(running) = running {
                running.interim(text);
                shared.appended.notify_all();
            }
        };
        let ran = panic::catch_unwind(AssertUnwindSafe(|| {
            runner.run(&input, &interrupt, &mut interim)
        }));
        let finished = interrupt.finish();
        let ending = match ran {
            Err(payload) => panicked(payload.as_ref()),
            Ok(ending) if finished => ending,
            // An interrupted turn still hands over the reactions it recorded, as the reference
            // set each one at its call, and whether its tools ran.
            Ok(ending) => Ending {
                outcome: TurnOutcome::Interrupted,
                line: None,
                answer: None,
                resets_at: None,
                ..ending
            },
        };
        let ended = now();
        let mut lanes = lock(&shared.lanes);
        let lane = lanes
            .sessions
            .entry(input.request.session.clone())
            .or_default();
        if let Some(running) = lane.running.take() {
            lane.keep(running.end(ending, ended));
        }
        let next = lane.queue.pop_front();
        if let Some((id, _)) = &next {
            let running = Running::new(id.clone(), ended, None);
            interrupt = running.interrupt.clone();
            lane.running = Some(running);
        }
        shared.appended.notify_all();
        let Some((_, queued)) = next else {
            return;
        };
        input = queued;
    }
}

/// The `failed` ending of a turn whose runner panicked with `payload`, its line also written to
/// stderr (`_handle_message_with_agent` logs the error and answers the turn as failed; the
/// `finally` of `_run_agent_inner` then frees the session).
fn panicked(payload: &(dyn Any + Send)) -> Ending {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message");
    let line = format!("ulei-engine: turn runner panicked: {message}").replace('\n', " ");
    eprintln!("{line}");
    Ending {
        outcome: TurnOutcome::Failed,
        line: Some(line),
        answer: None,
        reactions: Vec::new(),
        resets_at: None,
        tools_ran: None,
    }
}

/// Waits without a timer until the turn of `request` has a page for `ticket` and takes it.
fn hold(shared: &Shared, request: &TurnEventsRequest, ticket: u64) -> (TurnState, Vec<TurnEvent>) {
    let mut lanes = lock(&shared.lanes);
    loop {
        if let Some(page) = lanes.page(request, ticket) {
            return page;
        }
        lanes = shared
            .appended
            .wait(lanes)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// Waits without a timer until the `inbox_turns` request of `ticket` has its answer and takes
/// it.
fn hold_inbox(shared: &Shared, owner: &str, ticket: u64) -> InboxTurns {
    let mut lanes = lock(&shared.lanes);
    loop {
        if let Some(answer) = lanes.inbox_answer(owner, ticket) {
            return answer;
        }
        lanes = shared
            .appended
            .wait(lanes)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// Blocks in a read of the held `inbox_turns` connection `stream` until its end of stream or
/// failure, then releases the hold of `ticket` if it still waits: a peer that hung up or died
/// holds nothing. The holder's `Shutdown::Read` after its answer ends the read too.
fn watch_inbox(shared: &Shared, mut stream: &UnixStream, ticket: u64) {
    let mut sink = [0u8; 64];
    loop {
        match stream.read(&mut sink) {
            Ok(0) => break,
            Err(e) if e.kind() != io::ErrorKind::Interrupted => break,
            _ => {}
        }
    }
    let mut lanes = lock(&shared.lanes);
    if lanes.inbox_hold == Some(ticket) {
        lanes.inbox_hold = None;
        shared.appended.notify_all();
    }
}

/// Writes the `inbox_turns` `answer` on `stream`; the turn it names is handed over when the
/// write succeeds and waits for the next request when it fails.
fn hand_over(
    shared: &Shared,
    owner: &str,
    stream: &UnixStream,
    answer: InboxTurns,
    timeout: Duration,
) {
    let reply = match serde_json::to_vec(&answer) {
        Ok(body) => (200, body),
        Err(_) => error(ErrorCode::Internal, "answer does not serialize"),
    };
    let written = send(stream, &reply, timeout) && reply.0 == 200;
    if let Some(turn) = answer.turn {
        let mut lanes = lock(&shared.lanes);
        lanes.handed(owner, &turn, written);
        if !written {
            shared.appended.notify_all();
        }
    }
}

/// The `200` answer of `turn`'s `events` in `state`, as many from the first as fit
/// [`MAX_BODY`]; `500` when the first does not fit alone.
fn page_reply(
    turn: &str,
    (state, mut events): (TurnState, Vec<TurnEvent>),
) -> Result<Reply, Reply> {
    let unserializable = || error(ErrorCode::Internal, "answer does not serialize");
    let mut page = TurnEventsPage {
        turn: turn.to_owned(),
        state,
        events: Vec::new(),
    };
    let mut size = serde_json::to_vec(&page)
        .map_err(|_| unserializable())?
        .len();
    let mut fit = 0;
    for (n, event) in events.iter().enumerate() {
        let comma = usize::from(n > 0);
        size += serde_json::to_vec(event)
            .map_err(|_| unserializable())?
            .len()
            + comma;
        if size > MAX_BODY {
            break;
        }
        fit += 1;
    }
    if fit == 0 && !events.is_empty() {
        return Err(error(ErrorCode::Internal, "an event is over 512 KiB"));
    }
    events.truncate(fit);
    page.events = events;
    Ok((
        200,
        serde_json::to_vec(&page).map_err(|_| unserializable())?,
    ))
}

/// The C6 entry of `raw` and its serialized length: its message, or its skip when the message
/// does not fit an answer of `base` bytes alone.
fn raw_entry(raw: Raw, base: usize) -> serde_json::Result<(Result<RawMessage, RawSkipped>, usize)> {
    let len = raw.message.len() as u64;
    let message = raw_message(raw);
    let bytes = serde_json::to_vec(&message)?.len();
    if base + bytes <= MAX_BODY {
        return Ok((Ok(message), bytes));
    }
    let skip = RawSkipped {
        session_id: message.session_id,
        session_key: message.session_key,
        seq: message.seq,
        ts: message.ts,
        len,
    };
    let bytes = serde_json::to_vec(&skip)?.len();
    Ok((Err(skip), bytes))
}

/// The C6 message of the journal's raw message `raw`.
fn raw_message(raw: Raw) -> RawMessage {
    RawMessage {
        session_id: raw.session.to_string(),
        session_key: Some(raw.key),
        seq: Some(raw.seq),
        #[allow(clippy::cast_precision_loss)]
        ts: raw.at.map(|at| at as f64),
        role: raw.role.as_str().to_owned(),
        content: raw.content,
        message: Some(raw.message),
    }
}

/// Writes `reply` within `timeout`; false when the write fails, a client gone.
fn send(mut stream: &UnixStream, (status, body): &Reply, timeout: Duration) -> bool {
    stream.set_write_timeout(Some(timeout)).is_ok()
        && stream.write_all(&response(*status, body)).is_ok()
}

/// An event of `r#type` at `seq` without payload.
fn event(seq: u64, r#type: TurnEventType) -> TurnEvent {
    TurnEvent {
        seq,
        r#type,
        text: None,
        message_id: None,
        outcome: None,
        line: None,
    }
}

/// The C8 record value of `delivery`.
fn recorded(delivery: Delivery) -> InboxDeliveryDelivery {
    match delivery {
        Delivery::Sent => InboxDeliveryDelivery::Sent,
        Delivery::SendFailed => InboxDeliveryDelivery::SendFailed,
        Delivery::Silent => InboxDeliveryDelivery::Silent,
        Delivery::SilentDone => InboxDeliveryDelivery::SilentDone,
        Delivery::NoAnswer => InboxDeliveryDelivery::NoAnswer,
    }
}

/// The end of the lane's last ended turn.
fn last_end(lane: &Lane) -> Option<f64> {
    lane.last_turn().and_then(|report| report.ended)
}

fn bare_report(turn: TurnId, state: TurnState) -> TurnReport {
    TurnReport {
        turn,
        state,
        outcome: None,
        ended: None,
        line: None,
        resets_at: None,
        tools_ran: None,
        delivery: None,
    }
}

fn lock(lanes: &Mutex<Lanes>) -> MutexGuard<'_, Lanes> {
    lanes.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether `text` matches C6 `InjectRequest.event_id`: `^[a-zA-Z0-9:_-]{1,96}$`.
fn is_event_id(text: &str) -> bool {
    (1..=96).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-'))
}

/// Whether `text` matches C6 `JobId`: `^[a-zA-Z0-9_-]{1,64}$`.
fn is_job_id(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// Whether `text` matches C6 `TurnId`: `^[a-zA-Z0-9:._-]{1,128}$`.
fn is_turn_id(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'_' | b'-'))
}

/// The method of a request line `POST /v1/<name> HTTP/1.1`.
fn method(line: &str) -> Result<Method, Reply> {
    let mut parts = line.split(' ');
    let (Some(verb), Some(target), Some("HTTP/1.1"), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(error(ErrorCode::BadRequest, "malformed request line"));
    };
    if verb.is_empty() || target.is_empty() {
        return Err(error(ErrorCode::BadRequest, "malformed request line"));
    }
    let method = match target.strip_prefix("/v1/").filter(|_| verb == "POST") {
        Some("inbox_turns") => Method::InboxTurns,
        Some("inject") => Method::Inject,
        Some("interrupt") => Method::Interrupt,
        Some("is_idle") => Method::IsIdle,
        Some("job_results") => Method::JobResults,
        Some("jobs_done") => Method::JobsDone,
        Some("raw_sessions") => Method::RawSessions,
        Some("report_delivery") => Method::ReportDelivery,
        Some("set_route") => Method::SetRoute,
        Some("status") => Method::Status,
        Some("submit_turn") => Method::SubmitTurn,
        Some("turn_events") => Method::TurnEvents,
        _ => {
            return Err(error(ErrorCode::NotFound, "no such method or path"));
        }
    };
    Ok(method)
}

/// The answer to a request that could not be read: `413` for a head over its limit, `400`
/// for a malformed one, none for a timeout, a failure or the end of the stream.
fn refusal(e: &gateway::Error) -> Option<Reply> {
    match e {
        gateway::Error::Malformed(Malformed::HeadTooLarge) => {
            Some(error(ErrorCode::TooLarge, "headers over 8 KiB"))
        }
        gateway::Error::Malformed(what) => Some(error(ErrorCode::BadRequest, &what.to_string())),
        _ => None,
    }
}

fn invalid_body() -> Reply {
    error(
        ErrorCode::BadRequest,
        "body is not valid for the method input",
    )
}

/// The status and C6 `Error` body of `code`.
fn error(code: ErrorCode, message: &str) -> Reply {
    let status = match code {
        ErrorCode::BadRequest => 400,
        ErrorCode::Forbidden => 403,
        ErrorCode::NotFound => 404,
        ErrorCode::TooLarge => 413,
        ErrorCode::Internal => 500,
    };
    let body = WireError {
        code,
        message: message.to_owned(),
    };
    // Two string fields always serialize.
    (status, serde_json::to_vec(&body).unwrap_or_default())
}

fn response(status: u16, body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Content Too Large",
        _ => "Internal Server Error",
    };
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// Seconds since the unix epoch; a clock set before it reads 0.
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}
