//! The core side of the C6 wire (ADR-0008): one synchronous HTTP/1.1 JSON request per
//! connection to a runtime adapter listening on a Unix socket.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::net::sockopt::{self, Timeout};
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use serde_json::{Map, Value};
use ulei_contracts::generated::c6::{
    Empty, Error as WireError, ErrorCode, IdleStatus, InjectRequest, InjectResponse,
    InterruptRequest, InterruptResponse, RouteSet, SessionStatus, SetRouteRequest, StatusRequest,
    SubmitTurnRequest, SubmitTurnResponse, TurnOutcome, TurnReport, TurnState,
};

/// Most bytes of the status line, header lines and empty line together, in either direction.
pub const MAX_HEAD: usize = 8 * 1024;
/// Most bytes of a request or response body.
pub const MAX_BODY: usize = 512 * 1024;

/// Most turns a session queue holds: the C6 `maximum` of `queue_depth` and `position`.
const MAX_QUEUE: u64 = 32;

const CHUNK: usize = 8 * 1024;

/// Why a C6 call failed.
#[derive(Debug)]
pub enum Error {
    /// Connecting, writing, reading or the peer credentials failed, or the input does not
    /// serialize or names no C6 method.
    Io(io::Error),
    /// The answer did not arrive within the configured timeout.
    Timeout,
    /// The adapter runs as another uid; nothing was sent.
    PeerUid {
        /// Effective uid the client requires of the adapter.
        expected: u32,
        /// `SO_PEERCRED` uid of the adapter.
        actual: u32,
    },
    /// The request body exceeds [`MAX_BODY`] bytes; nothing was sent.
    RequestTooLarge(usize),
    /// The response is not HTTP/1.1 within the limits, or its body is not JSON.
    Malformed(&'static str),
    /// The adapter answered with a C6 `Error`.
    Remote {
        /// Error class.
        code: ErrorCode,
        /// Diagnostic of the adapter.
        message: String,
    },
    /// The response body is JSON but not valid for its definition.
    Invalid(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "C6 socket: {e}"),
            Self::Timeout => f.write_str("C6 call timed out"),
            Self::PeerUid { expected, actual } => {
                write!(f, "C6 adapter runs as uid {actual}, expected {expected}")
            }
            Self::RequestTooLarge(len) => {
                write!(f, "C6 request body has {len} bytes, at most {MAX_BODY}")
            }
            Self::Malformed(what) => write!(f, "malformed C6 response: {what}"),
            Self::Remote { code, message } => {
                write!(f, "C6 adapter error {}: {message}", code.as_str())
            }
            Self::Invalid(what) => write!(f, "invalid C6 response: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// A C6 client bound to one adapter socket; every call opens its own connection.
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    timeout: Duration,
    uid: u32,
}

impl Client {
    /// A client of the adapter at `socket` that requires the adapter to run as this process's
    /// effective uid; `timeout` bounds each call from connect to the last response byte.
    ///
    /// # Errors
    /// [`Error::Io`] when the effective uid cannot be read.
    pub fn new(socket: impl Into<PathBuf>, timeout: Duration) -> Result<Self, Error> {
        Ok(Self::with_uid(socket, timeout, own_uid()?))
    }

    /// As [`Client::new`], but the adapter must run as `uid`.
    pub fn with_uid(socket: impl Into<PathBuf>, timeout: Duration, uid: u32) -> Self {
        Self {
            socket: socket.into(),
            timeout,
            uid,
        }
    }

    /// C6 `inject`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn inject(&self, request: &InjectRequest) -> Result<InjectResponse, Error> {
        let body = self.call("inject", &input(serde_json::to_value(request))?)?;
        let answer: InjectResponse =
            serde_json::from_value(body).map_err(|_| Error::Invalid("not an InjectResponse"))?;
        if answer.turn.as_deref().is_some_and(|t| !is_turn_id(t)) {
            return Err(Error::Invalid("InjectResponse turn is not a TurnId"));
        }
        Ok(answer)
    }

    /// C6 `is_idle`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn is_idle(&self) -> Result<IdleStatus, Error> {
        let body = self.call("is_idle", &input(serde_json::to_value(&Empty {}))?)?;
        let status: IdleStatus =
            serde_json::from_value(body).map_err(|_| Error::Invalid("not an IdleStatus"))?;
        if status.since.is_some_and(|since| since < 0.0) {
            return Err(Error::Invalid("IdleStatus since is negative"));
        }
        Ok(status)
    }

    /// C6 `submit_turn`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn submit_turn(&self, request: &SubmitTurnRequest) -> Result<SubmitTurnResponse, Error> {
        let body = self.call("submit_turn", &input(serde_json::to_value(request))?)?;
        let answer: SubmitTurnResponse =
            serde_json::from_value(body).map_err(|_| Error::Invalid("not a SubmitTurnResponse"))?;
        if answer.queue_depth > MAX_QUEUE || answer.position.is_some_and(|p| p > MAX_QUEUE) {
            return Err(Error::Invalid("SubmitTurnResponse is over the queue cap"));
        }
        if answer.turn.as_deref().is_some_and(|t| !is_turn_id(t)) {
            return Err(Error::Invalid("SubmitTurnResponse turn is not a TurnId"));
        }
        if answer.turn.is_some() != answer.accepted || answer.position.is_some() != answer.accepted
        {
            return Err(Error::Invalid(
                "SubmitTurnResponse turn and position are not present exactly when accepted",
            ));
        }
        Ok(answer)
    }

    /// C6 `interrupt`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn interrupt(&self, request: &InterruptRequest) -> Result<InterruptResponse, Error> {
        let body = self.call("interrupt", &input(serde_json::to_value(request))?)?;
        let answer: InterruptResponse =
            serde_json::from_value(body).map_err(|_| Error::Invalid("not an InterruptResponse"))?;
        if answer.queue_depth > MAX_QUEUE {
            return Err(Error::Invalid("InterruptResponse is over the queue cap"));
        }
        if [&answer.turn, &answer.dropped]
            .into_iter()
            .flatten()
            .any(|t| !is_turn_id(t))
        {
            return Err(Error::Invalid("InterruptResponse turn is not a TurnId"));
        }
        if answer.turn.is_some() != answer.interrupted
            || (answer.dropped.is_some() && !answer.interrupted)
        {
            return Err(Error::Invalid(
                "InterruptResponse turn or dropped is present without an interrupt",
            ));
        }
        Ok(answer)
    }

    /// C6 `status`; the answer reports the turn the request names, when it names one.
    ///
    /// # Errors
    /// [`Error`].
    pub fn status(&self, request: &StatusRequest) -> Result<SessionStatus, Error> {
        let body = self.call("status", &input(serde_json::to_value(request))?)?;
        let status: SessionStatus =
            serde_json::from_value(body).map_err(|_| Error::Invalid("not a SessionStatus"))?;
        if !is_session_key(&status.session) || !is_route_alias(&status.route) {
            return Err(Error::Invalid(
                "SessionStatus session or route is not valid",
            ));
        }
        if status.since < 0.0 {
            return Err(Error::Invalid("SessionStatus since is negative"));
        }
        if status.queue_depth > MAX_QUEUE {
            return Err(Error::Invalid("SessionStatus is over the queue cap"));
        }
        if status.turn.as_ref().map(|r| &r.turn) != request.turn.as_ref() {
            return Err(Error::Invalid(
                "SessionStatus turn is not the turn asked for",
            ));
        }
        if let Some(report) = &status.last_turn {
            check_report(report, true)?;
        }
        if let Some(report) = &status.turn {
            check_report(report, false)?;
        }
        Ok(status)
    }

    /// C6 `set_route`.
    ///
    /// # Errors
    /// [`Error`].
    pub fn set_route(&self, request: &SetRouteRequest) -> Result<RouteSet, Error> {
        let body = self.call("set_route", &input(serde_json::to_value(request))?)?;
        let answer: RouteSet =
            serde_json::from_value(body).map_err(|_| Error::Invalid("not a RouteSet"))?;
        if !is_session_key(&answer.session) || !is_route_alias(&answer.route) {
            return Err(Error::Invalid("RouteSet session or route is not valid"));
        }
        Ok(answer)
    }

    /// Sends `input` to `POST /v1/<method>` and returns the `200` body unchecked; the typed
    /// methods check their answers.
    ///
    /// # Errors
    /// [`Error`]; [`Error::Io`] when `method` is not a C6 method name (`[a-z_]`, 1 to 64).
    pub fn call(&self, method: &str, input: &Value) -> Result<Value, Error> {
        if method.is_empty()
            || method.len() > 64
            || !method.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a C6 method name",
            )));
        }
        let body = canonical(input);
        if body.len() > MAX_BODY {
            return Err(Error::RequestTooLarge(body.len()));
        }
        let deadline = Instant::now() + self.timeout;
        let mut stream = connect(&self.socket, deadline)?;
        let actual = sockopt::socket_peercred(&stream)
            .map_err(|e| Error::Io(e.into()))?
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
        stream
            .set_write_timeout(Some(left(deadline)?))
            .map_err(Error::Io)?;
        stream.write_all(&request).map_err(io_error)?;
        let (status, body) = read_response(&mut stream, deadline)?;
        if status == 200 {
            return Ok(body);
        }
        let error: WireError = serde_json::from_value(body)
            .map_err(|_| Error::Invalid("non-200 body is not Error"))?;
        if status_of(error.code) != status {
            return Err(Error::Invalid("status does not match the Error code"));
        }
        Err(Error::Remote {
            code: error.code,
            message: error.message,
        })
    }
}

/// Connects to `socket` before `deadline`: a listener whose backlog stays full until then is
/// [`Error::Timeout`].
fn connect(socket: &Path, deadline: Instant) -> Result<UnixStream, Error> {
    let errno = |e: rustix::io::Errno| Error::Io(e.into());
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

/// A request serialized by `serde_json::to_value`; a failure is [`Error::Io`].
fn input(value: serde_json::Result<Value>) -> Result<Value, Error> {
    value.map_err(|e| Error::Io(e.into()))
}

/// Checks what the generated [`TurnReport`] does not encode: the `TurnId` pattern, `ended` not
/// negative and the fields its `state` carries. A `last_turn` read back from a journal after a
/// runtime restart is `ended` without `outcome` and `ended`.
fn check_report(report: &TurnReport, last_turn: bool) -> Result<(), Error> {
    if !is_turn_id(&report.turn) {
        return Err(Error::Invalid("TurnReport turn is not a TurnId"));
    }
    if report.ended.is_some_and(|ended| ended < 0.0) {
        return Err(Error::Invalid("TurnReport ended is negative"));
    }
    let fields = match report.state {
        TurnState::Ended if last_turn => report.outcome.is_some() == report.ended.is_some(),
        TurnState::Ended => report.outcome.is_some() && report.ended.is_some(),
        TurnState::Dropped => report.outcome.is_none() && report.ended.is_some(),
        TurnState::Queued | TurnState::Running | TurnState::Unknown => {
            report.outcome.is_none() && report.ended.is_none()
        }
    };
    if !fields {
        return Err(Error::Invalid(
            "TurnReport outcome or ended does not fit its state",
        ));
    }
    let line = matches!(
        report.outcome,
        Some(TurnOutcome::QuotaExhausted | TurnOutcome::ContextExhausted | TurnOutcome::Failed)
    );
    if report.line.is_some() && !line {
        return Err(Error::Invalid(
            "TurnReport line without an outcome that has one",
        ));
    }
    Ok(())
}

/// Whether `text` matches C6 `TurnId`: `^[a-zA-Z0-9:._-]{1,128}$`.
fn is_turn_id(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'_' | b'-'))
}

/// Whether `text` is a C6 `SessionKey`: 1 to 128 characters.
fn is_session_key(text: &str) -> bool {
    (1..=128).contains(&text.chars().count())
}

/// Whether `text` matches C6 `RouteAlias`: `^[a-z][a-z0-9_-]{0,63}$`.
fn is_route_alias(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && text.len() <= 64
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// HTTP status of each C6 `ErrorCode` (ADR-0008).
fn status_of(code: ErrorCode) -> u16 {
    match code {
        ErrorCode::BadRequest => 400,
        ErrorCode::Forbidden => 403,
        ErrorCode::NotFound => 404,
        ErrorCode::TooLarge => 413,
        ErrorCode::Internal => 500,
    }
}

/// Reads one response until `deadline`: its status and JSON body.
fn read_response(stream: &mut UnixStream, deadline: Instant) -> Result<(u16, Value), Error> {
    let mut buf = Vec::with_capacity(CHUNK);
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() >= MAX_HEAD {
            return Err(Error::Malformed("head over 8 KiB"));
        }
        read_some(stream, &mut buf, CHUNK, deadline)?;
    };
    if head_end + 4 > MAX_HEAD {
        return Err(Error::Malformed("head over 8 KiB"));
    }
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
        read_some(stream, &mut body, missing, deadline)?;
    }
    if body.len() > length {
        return Err(Error::Malformed("bytes after the body"));
    }
    end_of_stream(stream, deadline)?;
    let value = serde_json::from_slice(&body).map_err(|_| Error::Malformed("body is not JSON"))?;
    Ok((status, value))
}

/// Waits until `deadline` for the end of the stream after the body; a byte instead is
/// [`Error::Malformed`].
fn end_of_stream(stream: &mut UnixStream, deadline: Instant) -> Result<(), Error> {
    stream
        .set_read_timeout(Some(left(deadline)?))
        .map_err(Error::Io)?;
    match stream.read(&mut [0u8; 1]) {
        Ok(0) => Ok(()),
        Ok(_) => Err(Error::Malformed("bytes after the body")),
        // A peer that closes with request bytes unread resets the stream; nothing follows.
        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => Ok(()),
        Err(e) => Err(io_error(e)),
    }
}

/// Appends at most `max` bytes that arrive before `deadline`.
fn read_some(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    max: usize,
    deadline: Instant,
) -> Result<(), Error> {
    stream
        .set_read_timeout(Some(left(deadline)?))
        .map_err(Error::Io)?;
    let mut chunk = [0u8; CHUNK];
    match stream.read(&mut chunk[..max.min(CHUNK)]) {
        Ok(0) => Err(Error::Malformed("end of stream before the whole response")),
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Ok(())
        }
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
        _ => Error::Io(e),
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
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(Error::Malformed("malformed header"));
        }
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

/// Compact JSON with object keys in code point order at every level: a workspace build may
/// unify `serde_json` with `preserve_order`, and then `Map` keeps insertion order.
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

/// Effective uid of this process: std has no `geteuid`, and a socketpair peer is ourselves.
fn own_uid() -> Result<u32, Error> {
    let (ours, _peer) = UnixStream::pair().map_err(Error::Io)?;
    let cred = sockopt::socket_peercred(&ours).map_err(|e| Error::Io(e.into()))?;
    Ok(cred.uid.as_raw())
}
