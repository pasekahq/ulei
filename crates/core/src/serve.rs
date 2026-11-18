//! The `ulei-inbox serve` daemon of ADR-0007 section 6: C1 `x-methods` as synchronous,
//! single-threaded HTTP/1.1 JSON over a Unix socket, one request per connection.

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use ulei_contracts::generated::c1::{
    AckRequest, ClaimRequest, ErrorCode, GetRequest, HealthRequest, RenewRequest, RetryRequest,
    StartRequest,
};

use crate::inbox::{
    self, EventRow, Inbox, LEASE_TTL, RETRY_DELAY, canonical, fixed_notation, lower_hex_of_len,
    valid_id,
};

/// Longest `--socket` path in bytes: `sun_path` holds 108 bytes with the trailing NUL.
pub const MAX_SOCKET_PATH: usize = 107;
/// How long one connection may take to send its whole request.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Most bytes of the request line, header lines and empty line together.
pub const MAX_HEAD: usize = 8 * 1024;
/// Most bytes of a request body.
pub const MAX_BODY: usize = 512 * 1024;

const SERVER: &str = concat!("ulei-inbox ", env!("CARGO_PKG_VERSION"));
const CHUNK: usize = 8 * 1024;

/// A C1 `x-methods` operation.
#[derive(Debug, Clone, Copy)]
enum Op {
    Publish,
    Claim,
    Start,
    Renew,
    Retry,
    Ack,
    Get,
    Health,
}

impl Op {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "publish" => Self::Publish,
            "claim" => Self::Claim,
            "start" => Self::Start,
            "renew" => Self::Renew,
            "retry" => Self::Retry,
            "ack" => Self::Ack,
            "get" => Self::Get,
            "health" => Self::Health,
            _ => return None,
        })
    }
}

/// An HTTP status and its canonical JSON body.
type Reply = (u16, Vec<u8>);

/// Why the socket could not be bound.
#[derive(Debug)]
pub enum BindError {
    /// The path is longer than [`MAX_SOCKET_PATH`] bytes.
    PathTooLong(usize),
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

/// Binds and listens at `path` with mode 0600. An existing path is replaced only when it is
/// a socket that refuses a connection (stale); anything else there is [`BindError::Occupied`].
///
/// # Errors
/// [`BindError`].
pub fn bind(path: &Path) -> Result<UnixListener, BindError> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(BindError::PathTooLong(len));
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

/// The daemon state: the store, the uid a peer must have and the per-connection deadline.
pub struct Server {
    inbox: Inbox,
    uid: u32,
    timeout: Duration,
}

impl Server {
    /// A server over `inbox` that serves peers of `uid` and closes a connection whose
    /// request has not fully arrived within `timeout`.
    pub fn new(inbox: Inbox, uid: u32, timeout: Duration) -> Self {
        Self {
            inbox,
            uid,
            timeout,
        }
    }

    /// Serves connections one at a time, blocking in `accept`; returns only on an accept
    /// failure other than an aborted connection.
    pub fn run(&mut self, listener: &UnixListener) -> io::Error {
        loop {
            match listener.accept() {
                Ok((stream, _)) => self.serve(stream),
                Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {}
                Err(e) => return e,
            }
        }
    }

    /// Serves one accepted connection: checks the peer uid before reading, then reads one
    /// request, answers it with `Connection: close` and closes. A connection that fails or
    /// times out is closed without an answer.
    pub fn serve(&mut self, mut stream: UnixStream) {
        let answer = match rustix::net::sockopt::socket_peercred(&stream) {
            Ok(cred) if cred.uid.as_raw() == self.uid => self.answer(&mut stream),
            Ok(_) => Some(error(
                ErrorCode::Forbidden,
                "peer uid differs from the daemon",
            )),
            Err(_) => Some(error(ErrorCode::Internal, "peer credentials unavailable")),
        };
        if let Some(reply) = answer {
            self.send(&mut stream, &reply);
        }
    }

    /// Writes `reply` within the request timeout; a failed write is a client gone.
    fn send(&self, stream: &mut UnixStream, (status, body): &Reply) {
        if stream.set_write_timeout(Some(self.timeout)).is_ok() {
            let _ = stream.write_all(&response(*status, body));
        }
    }

    fn answer(&mut self, stream: &mut UnixStream) -> Option<Reply> {
        let deadline = Instant::now() + self.timeout;
        match read_request(stream, deadline) {
            Ok((op, body)) => Some(self.dispatch(op, &body).unwrap_or_else(|e| e)),
            Err(Refused::Answer(code, message)) => Some(error(code, message)),
            Err(Refused::TooLarge(unread)) => {
                // Closing with the body unread would reset the client's read of the answer,
                // and a client still writing the body would not read it at all.
                self.send(stream, &error(ErrorCode::TooLarge, "body over 512 KiB"));
                let _ = stream.shutdown(Shutdown::Write);
                discard(stream, unread, deadline);
                None
            }
            Err(Refused::Close) => None,
        }
    }

    /// Runs `op` on the store with the server clock; `Err` is a C1 `Error` reply.
    fn dispatch(&mut self, op: Op, body: &[u8]) -> Result<Reply, Reply> {
        let Ok(Value::Object(mut fields)) = serde_json::from_slice::<Value>(body) else {
            return Err(error(ErrorCode::BadRequest, "body is not a JSON object"));
        };
        let now = now().map_err(|_| error(ErrorCode::Internal, "clock before the epoch"))?;
        let inbox = &mut self.inbox;
        let out = match op {
            Op::Publish => {
                let packet = fields.remove("packet").ok_or_else(invalid_body)?;
                if !fields.is_empty() {
                    return Err(invalid_body());
                }
                let id = inbox.publish(&packet, now).map_err(|e| store_error(&e))?;
                json!({ "id": id })
            }
            Op::Claim => {
                let req: ClaimRequest = input(fields)?;
                let ttl = from_now(req.ttl, LEASE_TTL, |ttl| ttl > 0.0, now)?;
                row_response(inbox.claim(now, ttl).map_err(|e| store_error(&e))?.as_ref())?
            }
            Op::Start => {
                let req: StartRequest = input(fields)?;
                check_ids(&req.id, &req.lease)?;
                applied(inbox.start(&req.id, &req.lease, &req.session_id))?
            }
            Op::Renew => {
                let req: RenewRequest = input(fields)?;
                check_ids(&req.id, &req.lease)?;
                let ttl = from_now(req.ttl, LEASE_TTL, |ttl| ttl > 0.0, now)?;
                applied(inbox.renew(&req.id, &req.lease, now, ttl))?
            }
            Op::Retry => {
                let req: RetryRequest = input(fields)?;
                check_ids(&req.id, &req.lease)?;
                let delay = from_now(req.delay, RETRY_DELAY, |delay| delay >= 0.0, now)?;
                applied(inbox.retry(&req.id, &req.lease, now, delay))?
            }
            Op::Ack => {
                let req: AckRequest = input(fields)?;
                check_ids(&req.id, &req.lease)?;
                applied(inbox.ack(&req.id, &req.lease, req.outcome.as_str(), now))?
            }
            Op::Get => {
                let req: GetRequest = input(fields)?;
                if !valid_id(&req.id) {
                    return Err(invalid_body());
                }
                row_response(inbox.get(&req.id).map_err(|e| store_error(&e))?.as_ref())?
            }
            Op::Health => {
                let HealthRequest {} = input(fields)?;
                json!({ "ok": true, "version": SERVER })
            }
        };
        let body =
            canonical(&out).map_err(|_| error(ErrorCode::Internal, "response not canonical"))?;
        Ok((200, body))
    }
}

/// Why a request was not dispatched.
enum Refused {
    Answer(ErrorCode, &'static str),
    /// A body over [`MAX_BODY`], with this many of its bytes not read yet.
    TooLarge(usize),
    /// Timeout, I/O failure or end of stream before a whole request.
    Close,
}

/// Reads one request until `deadline` and returns its operation and body.
fn read_request(stream: &mut UnixStream, deadline: Instant) -> Result<(Op, Vec<u8>), Refused> {
    let mut buf = Vec::with_capacity(CHUNK);
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() >= MAX_HEAD {
            return Err(too_large_head());
        }
        read_some(stream, &mut buf, CHUNK, deadline)?;
    };
    if head_end + 4 > MAX_HEAD {
        return Err(too_large_head());
    }
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| bad("headers are not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let op = route(lines.next().unwrap_or_default())?;
    let length = content_length(lines)?;
    let mut body = buf.split_off(head_end + 4);
    if length > MAX_BODY {
        return Err(Refused::TooLarge(length - body.len()));
    }
    body.reserve(length.saturating_sub(body.len()));
    while body.len() < length {
        let missing = length - body.len();
        read_some(stream, &mut body, missing, deadline)?;
    }
    if body.len() > length {
        return Err(bad("bytes after the body"));
    }
    Ok((op, body))
}

fn too_large_head() -> Refused {
    Refused::Answer(ErrorCode::TooLarge, "headers over 8 KiB")
}

/// Appends at most `max` bytes that arrive before `deadline`.
fn read_some(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    max: usize,
    deadline: Instant,
) -> Result<(), Refused> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
        return Err(Refused::Close);
    }
    let mut chunk = [0u8; CHUNK];
    match stream.read(&mut chunk[..max.min(CHUNK)]) {
        Ok(0) | Err(_) => Err(Refused::Close),
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            Ok(())
        }
    }
}

/// Reads and drops up to `length` bytes that arrive before `deadline`, until the end of the
/// stream or a failure.
fn discard(stream: &mut UnixStream, mut length: usize, deadline: Instant) {
    let mut scratch = Vec::with_capacity(CHUNK);
    while length > 0 {
        scratch.clear();
        if read_some(stream, &mut scratch, length, deadline).is_err() {
            return;
        }
        length -= scratch.len();
    }
}

/// The operation of a request line `POST /v1/<name> HTTP/1.1`.
fn route(line: &str) -> Result<Op, Refused> {
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some("HTTP/1.1"), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("malformed request line"));
    };
    if method.is_empty() || target.is_empty() {
        return Err(bad("malformed request line"));
    }
    target
        .strip_prefix("/v1/")
        .filter(|_| method == "POST")
        .and_then(Op::from_name)
        .ok_or(Refused::Answer(
            ErrorCode::NotFound,
            "no such method or path",
        ))
}

/// The `Content-Length` of the header lines; `Transfer-Encoding` is refused.
fn content_length<'a>(lines: impl Iterator<Item = &'a str>) -> Result<usize, Refused> {
    let mut length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| bad("malformed header"))?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_graphic())
            || value.contains(['\r', '\n'])
        {
            return Err(bad("malformed header"));
        }
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(bad("Transfer-Encoding is refused"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad("malformed Content-Length"));
            }
            // All digits, so a parse failure is an overflow: far over the body limit.
            length = Some(value.parse::<usize>().unwrap_or(usize::MAX));
        }
    }
    length.ok_or_else(|| bad("Content-Length is required"))
}

fn bad(message: &'static str) -> Refused {
    Refused::Answer(ErrorCode::BadRequest, message)
}

fn input<T: DeserializeOwned>(fields: Map<String, Value>) -> Result<T, Reply> {
    serde_json::from_value(Value::Object(fields)).map_err(|_| invalid_body())
}

fn invalid_body() -> Reply {
    error(
        ErrorCode::BadRequest,
        "body is not valid for the method input",
    )
}

/// `value`, or `default` when absent, if `valid` holds and `now + value` prints in fixed
/// notation: a stored time outside it could not be answered as canonical JSON.
fn from_now(
    value: Option<f64>,
    default: f64,
    valid: fn(f64) -> bool,
    now: f64,
) -> Result<f64, Reply> {
    let value = value.unwrap_or(default);
    if valid(value) && fixed_notation(now + value) {
        Ok(value)
    } else {
        Err(invalid_body())
    }
}

/// C1 `EventId` and `Lease` patterns, which the generated types do not check.
fn check_ids(id: &str, lease: &str) -> Result<(), Reply> {
    if valid_id(id) && lower_hex_of_len(lease, 32) {
        Ok(())
    } else {
        Err(invalid_body())
    }
}

fn applied(changed: Result<bool, inbox::Error>) -> Result<Value, Reply> {
    Ok(json!({ "ok": changed.map_err(|e| store_error(&e))? }))
}

/// C1 `ClaimResponse` or `GetResponse`. The packet is the stored JSON, not the generated
/// `Event`, which would print an integer `source_ts` as a float.
fn row_response(row: Option<&EventRow>) -> Result<Value, Reply> {
    let Some(row) = row else {
        return Ok(json!({}));
    };
    let packet: Value = serde_json::from_slice(&row.packet)
        .map_err(|_| error(ErrorCode::Internal, "stored packet is not JSON"))?;
    let mut out = json!({
        "id": row.id,
        "packet": packet,
        "digest": row.digest,
        "status": row.status,
        "attempts": row.attempts,
        "available": row.available,
        "created": row.created,
    });
    let optional = [
        ("lease", row.lease.clone().map(Value::from)),
        ("lease_until", row.lease_until.map(Value::from)),
        ("session_id", row.session_id.clone().map(Value::from)),
        ("outcome", row.outcome.clone().map(Value::from)),
    ];
    for (name, value) in optional {
        if let Some(value) = value {
            out[name] = value;
        }
    }
    Ok(json!({ "row": out }))
}

fn store_error(e: &inbox::Error) -> Reply {
    match e {
        inbox::Error::Unauthenticated => {
            error(ErrorCode::Unauthenticated, "packet MAC does not verify")
        }
        inbox::Error::InvalidPacket(_) => error(
            ErrorCode::InvalidPacket,
            "packet fields or values are invalid",
        ),
        inbox::Error::Conflict => error(ErrorCode::Conflict, "event id has different content"),
        _ => error(ErrorCode::Internal, "store failure"),
    }
}

/// The status and canonical C1 `Error` body of `code`.
fn error(code: ErrorCode, message: &str) -> Reply {
    let status = match code {
        ErrorCode::BadRequest => 400,
        ErrorCode::Unauthenticated => 401,
        ErrorCode::Forbidden => 403,
        ErrorCode::NotFound => 404,
        ErrorCode::Conflict => 409,
        ErrorCode::TooLarge => 413,
        ErrorCode::InvalidPacket => 422,
        ErrorCode::Internal => 500,
    };
    // Keys in code point order and serde_json string escaping: the canonical form.
    let body = format!(
        "{{\"code\":\"{}\",\"message\":{}}}",
        code.as_str(),
        Value::from(message)
    );
    (status, body.into_bytes())
}

fn response(status: u16, body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Content Too Large",
        422 => "Unprocessable Content",
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

fn now() -> Result<f64, std::time::SystemTimeError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64())
}
