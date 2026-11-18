// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The C6 server of `ulei-engine serve` (ADR-0008, ADR-0028): HTTP/1.1 JSON over a Unix
//! socket, one request per connection, answered on the accepting thread. A session runs one
//! turn at a time on a thread of its own and queues the turns submitted meanwhile; turns of
//! different sessions run at the same time.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use ulei_contracts::generated::c6::{
    Empty, Error as WireError, ErrorCode, IdleStatus, SessionStatus, StatusRequest,
    SubmitTurnRequest, SubmitTurnResponse, TurnId, TurnOutcome, TurnReport, TurnState,
};

use crate::gateway::{self, Conn, MAX_BODY, Malformed};
use crate::journal::{self, Journal};

/// Longest socket path in bytes: `sun_path` holds 108 bytes with the trailing NUL.
pub const MAX_SOCKET_PATH: usize = 107;
/// How long one connection may take, from `accept`, to send its whole request.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Most turns waiting in one session's queue (`_BUSY_QUEUE_MAX_PENDING`).
pub const MAX_QUEUE: usize = 32;
/// Ended turns of one session whose reports are kept.
pub const KEPT_REPORTS: usize = 64;

/// Runs the turns the server starts.
pub trait Runner: Send + Sync {
    /// Runs and journals `turn` as `ulei-engine run` does, and says how it ended.
    fn run(&self, turn: &SubmitTurnRequest) -> Ending;
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending {
    /// Its C6 outcome.
    pub outcome: TurnOutcome,
    /// The one line it reports for `quota_exhausted`, `context_exhausted` and `failed`.
    pub line: Option<String>,
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
    IsIdle,
    Status,
    SubmitTurn,
}

/// An HTTP status and its JSON body.
type Reply = (u16, Vec<u8>);

/// The sessions' turns: running, queued and the reports of the ended ones.
struct Lanes {
    /// Unix time of the server's start.
    started: f64,
    /// The server's start in microseconds, the first part of every [`TurnId`] it gives.
    epoch: u128,
    /// Turns accepted so far.
    accepted: u64,
    sessions: HashMap<String, Lane>,
}

impl Lanes {
    /// A fresh id: `<epoch>-<n>`, the `n`th turn accepted since the start.
    fn next_id(&mut self) -> TurnId {
        self.accepted += 1;
        format!("{}-{}", self.epoch, self.accepted)
    }
}

/// One session key's turns.
#[derive(Default)]
struct Lane {
    /// The running turn and its start.
    running: Option<(TurnId, f64)>,
    queue: VecDeque<(TurnId, SubmitTurnRequest)>,
    /// The reports of the last [`KEPT_REPORTS`] ended turns, oldest first.
    ended: VecDeque<TurnReport>,
}

impl Lane {
    /// The report of turn `id`; `unknown` when the lane does not hold it.
    fn report(&self, id: &str) -> TurnReport {
        let state = if self.running.as_ref().is_some_and(|(r, _)| r == id) {
            TurnState::Running
        } else if self.queue.iter().any(|(q, _)| q == id) {
            TurnState::Queued
        } else if let Some(ended) = self.ended.iter().find(|r| r.turn == id) {
            return ended.clone();
        } else {
            TurnState::Unknown
        };
        bare_report(id.to_owned(), state)
    }
}

/// The server: the journal it reads `last_turn` from after a restart, the sessions' turns and
/// the settings read at its start.
pub struct Server {
    journal: Journal,
    lanes: Arc<Mutex<Lanes>>,
    runner: Arc<dyn Runner>,
    owner: String,
    route: String,
    uid: u32,
    timeout: Duration,
}

impl Server {
    /// A server whose turns `runner` runs, answering `is_idle` for session key `owner` and
    /// `route` as the route of every session, for peers of `uid`; it closes a connection whose
    /// request has not fully arrived within `timeout`.
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
            journal,
            lanes: Arc::new(Mutex::new(Lanes {
                started: start.as_secs_f64(),
                epoch: start.as_micros(),
                accepted: 0,
                sessions: HashMap::new(),
            })),
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
    /// times out is closed without an answer.
    pub fn serve(&self, stream: &UnixStream) {
        let answer = match rustix::net::sockopt::socket_peercred(stream) {
            Ok(cred) if cred.uid.as_raw() == self.uid => self.answer(stream),
            Ok(_) => Some(error(
                ErrorCode::Forbidden,
                "peer uid differs from the engine",
            )),
            Err(_) => Some(error(ErrorCode::Internal, "peer credentials unavailable")),
        };
        if let Some((status, body)) = answer
            && stream.set_write_timeout(Some(self.timeout)).is_ok()
        {
            let mut stream = stream;
            let _ = stream.write_all(&response(status, &body));
        }
    }

    fn answer(&self, stream: &UnixStream) -> Option<Reply> {
        let mut conn = Conn::new(stream, Arc::new(AtomicBool::new(false)), self.timeout);
        conn.deadline = Some(Instant::now() + self.timeout);
        let (line, length) = match conn.request_head() {
            Ok(head) => head,
            Err(e) => return refusal(&e),
        };
        let Some(length) = length else {
            return Some(error(ErrorCode::BadRequest, "Content-Length is required"));
        };
        if length > MAX_BODY {
            return Some(error(ErrorCode::TooLarge, "body over 512 KiB"));
        }
        // The body is read before the method is known: a socket closed with unread bytes
        // resets the client's read of the answer.
        let body = match conn.body(Some(length), MAX_BODY) {
            Ok(body) => body,
            Err(e) => return refusal(&e),
        };
        Some(
            method(&line)
                .and_then(|method| self.dispatch(method, &body))
                .unwrap_or_else(|e| e),
        )
    }

    /// Runs `method` on its input `body`; `Err` is a C6 `Error` reply.
    fn dispatch(&self, method: Method, body: &[u8]) -> Result<Reply, Reply> {
        let fields = match serde_json::from_slice::<Value>(body) {
            Ok(Value::Object(fields)) if !fields.values().any(Value::is_null) => fields,
            _ => return Err(invalid_body()),
        };
        let input = Value::Object(fields);
        let out = match method {
            Method::IsIdle => {
                let Empty {} = serde_json::from_value(input).map_err(|_| invalid_body())?;
                serde_json::to_vec(&self.is_idle())
            }
            Method::Status => {
                let request: StatusRequest =
                    serde_json::from_value(input).map_err(|_| invalid_body())?;
                if journal::check_key(&request.session).is_err()
                    || request.turn.as_deref().is_some_and(|t| !is_turn_id(t))
                {
                    return Err(invalid_body());
                }
                serde_json::to_vec(&self.status(request)?)
            }
            Method::SubmitTurn => {
                let turn: SubmitTurnRequest =
                    serde_json::from_value(input).map_err(|_| invalid_body())?;
                if journal::check_key(&turn.session).is_err()
                    || turn.prompt.is_empty()
                    || turn.channel.as_deref().is_some_and(str::is_empty)
                {
                    return Err(invalid_body());
                }
                serde_json::to_vec(&self.submit(turn)?)
            }
        }
        .map_err(|_| error(ErrorCode::Internal, "answer does not serialize"))?;
        Ok((200, out))
    }

    fn is_idle(&self) -> IdleStatus {
        let lanes = lock(&self.lanes);
        let lane = lanes.sessions.get(&self.owner);
        if lane.is_some_and(|lane| lane.running.is_some()) {
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

    fn status(&self, request: StatusRequest) -> Result<SessionStatus, Reply> {
        let mut status = {
            let lanes = lock(&self.lanes);
            let lane = lanes.sessions.get(&request.session);
            let running = lane.and_then(|lane| lane.running.as_ref());
            SessionStatus {
                running: running.is_some(),
                since: running
                    .map(|(_, since)| *since)
                    .or_else(|| lane.and_then(last_end))
                    .unwrap_or(lanes.started),
                queue_depth: lane.map_or(0, |lane| lane.queue.len() as u64),
                route: self.route.clone(),
                last_turn: lane.and_then(|lane| lane.ended.back().cloned()),
                turn: request.turn.map(|id| match lane {
                    Some(lane) => lane.report(&id),
                    None => bare_report(id, TurnState::Unknown),
                }),
                session: request.session,
            }
        };
        if status.last_turn.is_none() {
            let stored = self
                .journal
                .last_turn(&status.session)
                .map_err(|_| error(ErrorCode::Internal, "journal read failed"))?;
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
        let mut lanes = lock(&self.lanes);
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
        let lane = lanes.sessions.entry(turn.session.clone()).or_default();
        if busy {
            lane.queue.push_back((id.clone(), turn));
            let depth = lane.queue.len() as u64;
            return Ok(SubmitTurnResponse {
                accepted: true,
                turn: Some(id),
                position: Some(depth),
                queue_depth: depth,
            });
        }
        lane.running = Some((id.clone(), now()));
        let (shared, runner, first) = (
            Arc::clone(&self.lanes),
            Arc::clone(&self.runner),
            id.clone(),
        );
        let spawned =
            std::thread::Builder::new().spawn(move || drive(&shared, runner.as_ref(), first, turn));
        if spawned.is_err() {
            lane.running = None;
            return Err(error(ErrorCode::Internal, "cannot start the turn"));
        }
        Ok(SubmitTurnResponse {
            accepted: true,
            turn: Some(id),
            position: Some(0),
            queue_depth: 0,
        })
    }
}

/// Runs `turn` and then each turn its session queued meanwhile, in order, until the queue is
/// empty (`_dequeue_pending_event`, `_promote_queued_event`).
fn drive(lanes: &Mutex<Lanes>, runner: &dyn Runner, mut id: TurnId, mut turn: SubmitTurnRequest) {
    loop {
        let ending = runner.run(&turn);
        let ended = now();
        let mut lanes = lock(lanes);
        let lane = lanes.sessions.entry(turn.session.clone()).or_default();
        if lane.ended.len() == KEPT_REPORTS {
            lane.ended.pop_front();
        }
        lane.ended.push_back(TurnReport {
            turn: id,
            state: TurnState::Ended,
            outcome: Some(ending.outcome),
            ended: Some(ended),
            line: ending.line,
        });
        let Some((next, queued)) = lane.queue.pop_front() else {
            lane.running = None;
            return;
        };
        lane.running = Some((next.clone(), ended));
        (id, turn) = (next, queued);
    }
}

/// The end of the lane's last ended turn.
fn last_end(lane: &Lane) -> Option<f64> {
    lane.ended.back().and_then(|report| report.ended)
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
        Some("is_idle") => Method::IsIdle,
        Some("status") => Method::Status,
        Some("submit_turn") => Method::SubmitTurn,
        _ => {
            return Err(error(
                ErrorCode::NotFound,
                "no such method or path, or a method this engine does not serve",
            ));
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
