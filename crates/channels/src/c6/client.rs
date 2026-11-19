//! The channel's side of the C6 wire (ADR-0008, ADR-0033): one synchronous HTTP/1.1 JSON request
//! per connection to the engine's Unix socket, with a connect timeout and a call timeout.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustix::net::sockopt::{self, Timeout};
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use ulei_contracts::generated::c6::{
    Error as WireError, ErrorCode, InterruptRequest, InterruptResponse, RouteSet, SessionStatus,
    SetRouteRequest, StatusRequest, SubmitTurnRequest, SubmitTurnResponse, TurnEvent,
    TurnEventType, TurnEventsPage, TurnEventsRequest, TurnOutcome, TurnState,
};

/// Most bytes of the status line, header lines and empty line together.
const MAX_HEAD: usize = 8 * 1024;
/// Most bytes of a request or response body.
const MAX_BODY: usize = 512 * 1024;
const CHUNK: usize = 8 * 1024;

/// Why a C6 call failed.
#[derive(Debug)]
pub enum Error {
    /// The engine could not be reached: no socket, connection refused, or a socket failure.
    Unreachable(io::Error),
    /// The connect or the call timeout passed.
    Timeout,
    /// The engine runs as another uid; nothing was sent.
    PeerUid {
        /// Uid the client requires of the engine.
        expected: u32,
        /// `SO_PEERCRED` uid of the engine.
        actual: u32,
    },
    /// The request body is over 512 KiB; nothing was sent.
    RequestTooLarge(usize),
    /// The answer is not HTTP/1.1 JSON within the limits, or not valid for its definition.
    Malformed(&'static str),
    /// The engine answered with a C6 `Error`.
    Remote {
        /// Error class.
        code: ErrorCode,
        /// Diagnostic of the engine.
        message: String,
    },
    /// The call's [`Cancel`] fired.
    Cancelled,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "C6 socket: {e}"),
            Self::Timeout => f.write_str("C6 call timed out"),
            Self::PeerUid { expected, actual } => {
                write!(f, "C6 engine runs as uid {actual}, expected {expected}")
            }
            Self::RequestTooLarge(len) => {
                write!(f, "C6 request body has {len} bytes, at most {MAX_BODY}")
            }
            Self::Malformed(what) => write!(f, "malformed C6 answer: {what}"),
            Self::Remote { code, message } => {
                write!(f, "C6 engine error {}: {message}", code.as_str())
            }
            Self::Cancelled => f.write_str("C6 call cancelled"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreachable(e) => Some(e),
            _ => None,
        }
    }
}

/// A page of C6 `turn_events`: where the turn is, and its events after the request's cursor,
/// oldest first; the last one's `seq` is the cursor plus their number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventsPage {
    /// Where the turn is.
    pub state: TurnState,
    /// The events.
    pub events: Vec<Event>,
}

/// One event of a turn with the payload its type carries (ADR-0031 section 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The turn started running.
    Start,
    /// An assistant message while the turn runs.
    Interim(String),
    /// The turn's answer.
    Final(String),
    /// The turn ended; `line` is present for `quota_exhausted`, `context_exhausted` and `failed`.
    End {
        /// How it ended.
        outcome: TurnOutcome,
        /// Its one line.
        line: Option<String>,
    },
    /// The turn left the queue without running.
    Dropped,
}

impl Event {
    fn of(event: TurnEvent) -> Result<Self, Error> {
        let lacks = Error::Malformed("TurnEvent lacks the payload of its type");
        Ok(match (event.r#type, event.text, event.outcome) {
            (TurnEventType::Start, ..) => Self::Start,
            (TurnEventType::Interim, Some(text), _) => Self::Interim(text),
            (TurnEventType::Final, Some(text), _) => Self::Final(text),
            (TurnEventType::End, _, Some(outcome)) => {
                let lined = matches!(
                    outcome,
                    TurnOutcome::QuotaExhausted
                        | TurnOutcome::ContextExhausted
                        | TurnOutcome::Failed
                );
                if lined != event.line.is_some() {
                    return Err(lacks);
                }
                Self::End {
                    outcome,
                    line: event.line,
                }
            }
            (TurnEventType::Dropped, ..) => Self::Dropped,
            _ => return Err(lacks),
        })
    }
}

/// Ends the held calls made with it, at once and for good: the call in flight and every
/// later one fail with [`Error::Cancelled`]. Clones share the state.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<Mutex<Held>>);

#[derive(Debug, Default)]
struct Held {
    cancelled: bool,
    stream: Option<UnixStream>,
}

impl Cancel {
    /// A cancel not fired yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fires: a call blocked in its read returns at once.
    pub fn cancel(&self) {
        let mut held = self.lock();
        held.cancelled = true;
        if let Some(stream) = held.stream.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// Registers the stream of the call in flight.
    fn hold(&self, stream: &UnixStream) -> Result<(), Error> {
        let mut held = self.lock();
        if held.cancelled {
            return Err(Error::Cancelled);
        }
        held.stream = Some(stream.try_clone().map_err(Error::Unreachable)?);
        Ok(())
    }

    fn release(&self) {
        self.lock().stream = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Held> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A C6 client of one engine socket; every call opens its own connection.
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    connect_timeout: Duration,
    call_timeout: Duration,
    uid: u32,
}

impl Client {
    /// A client of the engine at `socket`, which must run as this process's effective uid.
    /// `connect_timeout` bounds the connect, `call_timeout` the whole call from connect to the
    /// last answer byte.
    ///
    /// # Errors
    /// The effective uid cannot be read.
    pub fn new(
        socket: impl Into<PathBuf>,
        connect_timeout: Duration,
        call_timeout: Duration,
    ) -> io::Result<Self> {
        let (ours, _peer) = UnixStream::pair()?;
        let uid = sockopt::socket_peercred(&ours)?.uid.as_raw();
        Ok(Self {
            socket: socket.into(),
            connect_timeout,
            call_timeout,
            uid,
        })
    }

    /// C6 `submit_turn`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn submit_turn(&self, request: &SubmitTurnRequest) -> Result<SubmitTurnResponse, Error> {
        let answer: SubmitTurnResponse = self.call("submit_turn", request)?;
        if answer.accepted != answer.turn.is_some() {
            return Err(Error::Malformed(
                "SubmitTurnResponse turn is not present exactly when accepted",
            ));
        }
        Ok(answer)
    }

    /// C6 `interrupt`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn interrupt(&self, request: &InterruptRequest) -> Result<InterruptResponse, Error> {
        self.call("interrupt", request)
    }

    /// C6 `status`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn status(&self, request: &StatusRequest) -> Result<SessionStatus, Error> {
        let status: SessionStatus = self.call("status", request)?;
        if !status.since.is_finite() || status.since < 0.0 {
            return Err(Error::Malformed("SessionStatus since is not a Unix time"));
        }
        Ok(status)
    }

    /// C6 `set_route`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn set_route(&self, request: &SetRouteRequest) -> Result<RouteSet, Error> {
        self.call("set_route", request)
    }

    /// C6 `turn_events`: the page the engine holds until the turn has an event after
    /// `request.after` or will have none, waited for at most the call timeout ([`Error::Timeout`],
    /// after which the caller calls again with the same `after`) or until `cancel` fires.
    ///
    /// # Errors
    /// [`Error`]; a page whose turn or event numbers do not follow the request, or whose
    /// events lack the payload of their type, is [`Error::Malformed`].
    pub fn turn_events(
        &self,
        request: &TurnEventsRequest,
        cancel: &Cancel,
    ) -> Result<EventsPage, Error> {
        let page: TurnEventsPage = self
            .call_with("turn_events", request, Some(cancel))
            .map_err(|e| {
                if cancel.is_cancelled() {
                    Error::Cancelled
                } else {
                    e
                }
            })?;
        if page.turn != request.turn {
            return Err(Error::Malformed(
                "TurnEventsPage turn differs from the request",
            ));
        }
        let mut events = Vec::with_capacity(page.events.len());
        for (event, seq) in page.events.into_iter().zip(request.after + 1..) {
            if event.seq != seq {
                return Err(Error::Malformed(
                    "TurnEventsPage events do not follow after",
                ));
            }
            events.push(Event::of(event)?);
        }
        Ok(EventsPage {
            state: page.state,
            events,
        })
    }

    /// Sends `input` to `POST /v1/<method>` and reads the `200` answer as `T`.
    fn call<T: DeserializeOwned>(&self, method: &str, input: &impl Serialize) -> Result<T, Error> {
        self.call_with(method, input, None)
    }

    /// [`Self::call`] whose stream `cancel` can shut down while the call waits.
    fn call_with<T: DeserializeOwned>(
        &self,
        method: &str,
        input: &impl Serialize,
        cancel: Option<&Cancel>,
    ) -> Result<T, Error> {
        let value = serde_json::to_value(input)
            .map_err(|_| Error::Malformed("request does not serialize"))?;
        let body = canonical(&value);
        if body.len() > MAX_BODY {
            return Err(Error::RequestTooLarge(body.len()));
        }
        let start = Instant::now();
        let deadline = start + self.call_timeout;
        let mut stream = connect(
            &self.socket,
            start + self.connect_timeout.min(self.call_timeout),
        )?;
        let actual = sockopt::socket_peercred(&stream)
            .map_err(|e| Error::Unreachable(e.into()))?
            .uid
            .as_raw();
        if actual != self.uid {
            return Err(Error::PeerUid {
                expected: self.uid,
                actual,
            });
        }
        let mut request = format!(
            "POST /v1/{method} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(&body);
        if let Some(cancel) = cancel {
            cancel.hold(&stream)?;
        }
        let answer = exchange(&mut stream, &request, deadline);
        if let Some(cancel) = cancel {
            cancel.release();
        }
        let (status, body) = answer?;
        if status == 200 {
            return serde_json::from_slice(&body)
                .map_err(|_| Error::Malformed("200 body is not valid for the method output"));
        }
        let error: WireError = serde_json::from_slice(&body)
            .map_err(|_| Error::Malformed("non-200 body is not a C6 Error"))?;
        Err(Error::Remote {
            code: error.code,
            message: error.message,
        })
    }
}

/// Writes `request` and reads its answer before `deadline`.
fn exchange(
    stream: &mut UnixStream,
    request: &[u8],
    deadline: Instant,
) -> Result<(u16, Vec<u8>), Error> {
    stream
        .set_write_timeout(Some(left(deadline)?))
        .map_err(Error::Unreachable)?;
    stream.write_all(request).map_err(io_error)?;
    read_response(stream, deadline)
}

/// Connects to `socket` before `deadline`: a listener whose backlog stays full until then is
/// [`Error::Timeout`].
fn connect(socket: &Path, deadline: Instant) -> Result<UnixStream, Error> {
    let errno = |e: rustix::io::Errno| Error::Unreachable(e.into());
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(errno)?;
    // A Unix `connect` to a full backlog waits up to the send timeout, then fails with EAGAIN.
    sockopt::set_socket_timeout(&fd, Timeout::Send, Some(left(deadline)?)).map_err(errno)?;
    let address = SocketAddrUnix::new(socket).map_err(errno)?;
    rustix::net::connect(&fd, &address).map_err(|e| io_error(e.into()))?;
    Ok(UnixStream::from(fd))
}

/// Reads one answer until `deadline`: its status and body.
fn read_response(stream: &mut UnixStream, deadline: Instant) -> Result<(u16, Vec<u8>), Error> {
    let mut buf = Vec::with_capacity(CHUNK);
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() >= MAX_HEAD {
            return Err(Error::Malformed("head over 8 KiB"));
        }
        if read_some(stream, &mut buf, CHUNK, deadline)? == 0 {
            return Err(Error::Malformed("end of stream before the whole answer"));
        }
    };
    let head =
        std::str::from_utf8(&buf[..head_end]).map_err(|_| Error::Malformed("head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status = status_line(lines.next().unwrap_or_default())?;
    let length = content_length(lines)?;
    if length > MAX_BODY {
        return Err(Error::Malformed("body over 512 KiB"));
    }
    let mut body = buf.split_off(head_end + 4);
    while body.len() < length {
        let missing = length - body.len();
        if read_some(stream, &mut body, missing, deadline)? == 0 {
            return Err(Error::Malformed("end of stream before the whole answer"));
        }
    }
    if body.len() > length || read_some(stream, &mut body, 1, deadline)? != 0 {
        return Err(Error::Malformed("bytes after the body"));
    }
    Ok((status, body))
}

/// Appends at most `max` bytes that arrive before `deadline`; 0 at the end of the stream.
fn read_some(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    max: usize,
    deadline: Instant,
) -> Result<usize, Error> {
    stream
        .set_read_timeout(Some(left(deadline)?))
        .map_err(Error::Unreachable)?;
    let mut chunk = [0u8; CHUNK];
    match stream.read(&mut chunk[..max.min(CHUNK)]) {
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Ok(n)
        }
        // A peer that closes with request bytes unread resets the stream; nothing follows.
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset && max == 1 => Ok(0),
        Err(e) => Err(io_error(e)),
    }
}

/// Time left until `deadline`; none left is [`Error::Timeout`].
fn left(deadline: Instant) -> Result<Duration, Error> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(Error::Timeout)
    } else {
        Ok(left)
    }
}

/// A socket timeout surfaces as `WouldBlock` or `TimedOut`.
fn io_error(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Error::Timeout,
        _ => Error::Unreachable(e),
    }
}

/// The status of a status line `HTTP/1.1 <3 digits> <reason>`.
fn status_line(line: &str) -> Result<u16, Error> {
    let mut parts = line.splitn(3, ' ');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("HTTP/1.1"), Some(code), Some(_))
            if code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit()) =>
        {
            code.parse()
                .map_err(|_| Error::Malformed("malformed status line"))
        }
        _ => Err(Error::Malformed("malformed status line")),
    }
}

/// The `Content-Length` of the header lines; `Transfer-Encoding` is refused.
fn content_length<'a>(lines: impl Iterator<Item = &'a str>) -> Result<usize, Error> {
    let mut length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(Error::Malformed("malformed header"))?;
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(Error::Malformed("Transfer-Encoding is refused"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::Malformed("malformed Content-Length"));
            }
            // All digits, so a parse failure is an overflow: far over the body limit.
            length = Some(value.parse::<usize>().unwrap_or(usize::MAX));
        }
    }
    length.ok_or(Error::Malformed("Content-Length is required"))
}

/// Compact JSON with object keys in code point order: a workspace build may unify
/// `serde_json` with `preserve_order`, and then `Map` keeps insertion order.
fn canonical(value: &Value) -> Vec<u8> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            Value::Object(fields) => {
                let mut entries: Vec<_> = fields.iter().collect();
                entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
                let mut out = Map::new();
                for (key, item) in entries {
                    out.insert(key.clone(), sorted(item));
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }
    sorted(value).to_string().into_bytes()
}
