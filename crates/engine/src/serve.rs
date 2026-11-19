// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The C6 server of `ulei-engine serve` (ADR-0008, ADR-0028): HTTP/1.1 JSON over a Unix
//! socket, one request per connection, answered on the accepting thread except a held
//! `turn_events`, which waits on a thread of its own (ADR-0031). A session runs one turn at a
//! time on a thread of its own and queues the turns submitted meanwhile; turns of different
//! sessions run at the same time. `interrupt` stops the running turn at once; `inject` starts an
//! inbox event's turn in the free owner session; `set_route` stores the alias the session's
//! later turns run on.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use ulei_contracts::generated::c6::{
    Empty, Error as WireError, ErrorCode, IdleStatus, InjectRequest, InjectResponse,
    InterruptRequest, InterruptResponse, RouteSet, SessionStatus, SetRouteRequest, StatusRequest,
    SubmitTurnRequest, SubmitTurnResponse, TurnEvent, TurnEventType, TurnEventsPage,
    TurnEventsRequest, TurnId, TurnOutcome, TurnReport, TurnState,
};

use crate::gateway::{self, Conn, MAX_BODY, Malformed};
use crate::journal::{self, Journal};
use crate::policy::Sender;
use crate::tools::Interrupt;

/// Longest socket path in bytes: `sun_path` holds 108 bytes with the trailing NUL.
pub const MAX_SOCKET_PATH: usize = 107;
/// How long one connection may take, from `accept`, to send its whole request.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Most turns waiting in one session's queue (`_BUSY_QUEUE_MAX_PENDING`).
pub const MAX_QUEUE: usize = 32;
/// Ended turns of one session whose reports are kept.
pub const KEPT_REPORTS: usize = 64;
/// Inbox events whose last delivery `inject` keeps for a replay. A replay comes while the core
/// holds the claim's lease, before it records the accept, and the owner session takes one event
/// at a time, so it finds its record long before this many later events are delivered.
pub const KEPT_DELIVERIES: usize = 64;
/// Seconds after which a busy `inject` asks for the event again: the delay of the core's
/// `retry`, about the length of a short owner turn.
pub const RETRY_AFTER_S: u64 = 30;

/// Runs the turns the server starts.
pub trait Runner: Send + Sync {
    /// Runs and journals `input` as `ulei-engine run` does, ending it early when `interrupt`
    /// fires and handing `interim` the text of each interim assistant message, and says how
    /// it ended.
    fn run(&self, input: &Input, interrupt: &Interrupt, interim: &mut dyn FnMut(&str)) -> Ending;
}

/// A turn the server runs: a prompt `submit_turn` took, or an inbox event `inject` took.
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
    Inject,
    Interrupt,
    IsIdle,
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
    /// The last [`KEPT_DELIVERIES`] accepted inbox deliveries, oldest first.
    deliveries: VecDeque<Delivery>,
    /// `turn_events` requests so far; each request's number is its ticket.
    tickets: u64,
    /// The ticket of the latest `turn_events` request of each turn that one still waits for.
    holds: HashMap<TurnId, u64>,
}

impl Lanes {
    /// A fresh id: `<epoch>-<n>`, the `n`th turn accepted since the start.
    fn next_id(&mut self) -> TurnId {
        self.accepted += 1;
        format!("{}-{}", self.epoch, self.accepted)
    }

    /// Records `delivery` as the one of its event, forgetting the oldest past
    /// [`KEPT_DELIVERIES`].
    fn record(&mut self, delivery: Delivery) {
        self.deliveries.retain(|d| d.event_id != delivery.event_id);
        if self.deliveries.len() == KEPT_DELIVERIES {
            self.deliveries.pop_front();
        }
        self.deliveries.push_back(delivery);
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

/// The highest accepted delivery attempt of an inbox event and the turn it started.
struct Delivery {
    event_id: String,
    attempt: u64,
    turn: TurnId,
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
}

impl Running {
    fn new(turn: TurnId, since: f64) -> Self {
        Self {
            turn,
            since,
            interrupt: Interrupt::new(),
            events: vec![event(1, TurnEventType::Start)],
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
            },
            events,
        }
    }
}

/// An ended or dropped turn's report and events.
struct Kept {
    report: TurnReport,
    events: Vec<TurnEvent>,
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

    /// The report of the turn that ended last; a dropped turn never ran, so it is not one.
    fn last_turn(&self) -> Option<&TurnReport> {
        self.ended
            .iter()
            .rev()
            .map(|k| &k.report)
            .find(|r| r.state == TurnState::Ended)
    }
}

/// The server: the journal that holds the sessions' routes and their `last_turn` after a
/// restart, the sessions' turns and the settings read at its start.
pub struct Server {
    journal: RefCell<Journal>,
    shared: Arc<Shared>,
    runner: Arc<dyn Runner>,
    owner: String,
    route: String,
    uid: u32,
    timeout: Duration,
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
                    deliveries: VecDeque::new(),
                    tickets: 0,
                    holds: HashMap::new(),
                }),
                appended: Condvar::new(),
            }),
            runner,
            owner,
            route,
            uid,
            timeout,
        }
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
    /// times out is closed without an answer. A `turn_events` that has to wait is answered from
    /// a thread of its own, which holds the connection until then.
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
    /// none is a `turn_events` answered later on `stream`.
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
            Method::Inject => {
                let event: InjectRequest =
                    serde_json::from_value(input).map_err(|_| invalid_body())?;
                if !is_event_id(&event.event_id) || event.payload.is_empty() || event.attempt == 0 {
                    return Err(invalid_body());
                }
                serde_json::to_vec(&self.inject(event)?)
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
                serde_json::to_vec(&self.is_idle())
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

    fn is_idle(&self) -> IdleStatus {
        let lanes = lock(&self.shared.lanes);
        let lane = lanes.sessions.get(&self.owner);
        if lane.is_some_and(Lane::busy) {
            return IdleStatus {
                idle: false,
                since: None,
            };
        }
        IdleStatus {
            idle: true,
            since: Some(lane.and_then(last_end).unwrap_or(lanes.started)),
        }
    }

    /// The status `request` asks for; a journal read is given up at `deadline`.
    fn status(&self, request: StatusRequest, deadline: Instant) -> Result<SessionStatus, Reply> {
        let journal = self.journal.borrow();
        let failed = |_| error(ErrorCode::Internal, "journal read failed");
        let route = journal
            .route(
                &request.session,
                deadline.saturating_duration_since(Instant::now()),
            )
            .map_err(failed)?
            .unwrap_or_else(|| self.route.clone());
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
        self.start(&mut lanes, id.clone(), input)?;
        Ok(SubmitTurnResponse {
            accepted: true,
            turn: Some(id),
            position: Some(0),
            queue_depth: 0,
        })
    }

    /// Starts `input` as turn `id` of its session, which runs none, on a thread that then runs
    /// the turns the session queues meanwhile.
    fn start(&self, lanes: &mut Lanes, id: TurnId, input: Input) -> Result<(), Reply> {
        let lane = lanes
            .sessions
            .entry(input.request.session.clone())
            .or_default();
        let running = Running::new(id, now());
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

    /// Starts the turn of inbox event `event` in the owner session when that session runs and
    /// queues none, as an event's prompt; a recorded delivery of the same or a higher attempt
    /// answers its turn and starts nothing.
    fn inject(&self, event: InjectRequest) -> Result<InjectResponse, Reply> {
        let accepted = |turn: TurnId| InjectResponse {
            accepted: true,
            session_id: Some(self.owner.clone()),
            retry_after_s: None,
            turn: Some(turn),
        };
        let mut lanes = lock(&self.shared.lanes);
        if let Some(recorded) = lanes
            .deliveries
            .iter()
            .find(|d| d.event_id == event.event_id && d.attempt >= event.attempt)
        {
            return Ok(accepted(recorded.turn.clone()));
        }
        if lanes.sessions.get(&self.owner).is_some_and(Lane::busy) {
            return Ok(InjectResponse {
                accepted: false,
                session_id: None,
                retry_after_s: Some(RETRY_AFTER_S),
                turn: None,
            });
        }
        let id = lanes.next_id();
        let input = Input {
            request: SubmitTurnRequest {
                session: self.owner.clone(),
                prompt: event.payload,
                new: None,
                channel: None,
                context: None,
            },
            sender: Sender::Event,
        };
        self.start(&mut lanes, id.clone(), input)?;
        lanes.record(Delivery {
            event_id: event.event_id,
            attempt: event.attempt,
            turn: id.clone(),
        });
        Ok(accepted(id))
    }

    /// Stores `request.route` with the current session of its key, which a missing session
    /// creates; a journal lock is given up at `deadline`.
    fn set_route(&self, request: SetRouteRequest, deadline: Instant) -> Result<RouteSet, Reply> {
        self.journal
            .borrow_mut()
            .store_route(
                &request.session,
                &request.route,
                deadline.saturating_duration_since(Instant::now()),
            )
            .map_err(|_| error(ErrorCode::Internal, "journal write failed"))?;
        Ok(RouteSet {
            session: request.session,
            route: request.route,
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
/// Every event appended wakes the held `turn_events`.
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
        let ending = match runner.run(&input, &interrupt, &mut interim) {
            ending if interrupt.finish() => ending,
            _ => Ending {
                outcome: TurnOutcome::Interrupted,
                line: None,
                answer: None,
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
            let running = Running::new(id.clone(), ended);
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

/// Writes `reply` within `timeout`; a failed write is a client gone.
fn send(mut stream: &UnixStream, (status, body): &Reply, timeout: Duration) {
    if stream.set_write_timeout(Some(timeout)).is_ok() {
        let _ = stream.write_all(&response(*status, body));
    }
}

/// An event of `r#type` at `seq` without payload.
fn event(seq: u64, r#type: TurnEventType) -> TurnEvent {
    TurnEvent {
        seq,
        r#type,
        text: None,
        outcome: None,
        line: None,
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
        Some("inject") => Method::Inject,
        Some("interrupt") => Method::Interrupt,
        Some("is_idle") => Method::IsIdle,
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
