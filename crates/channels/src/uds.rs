//! The wire of the modules' Unix sockets (ADR-0007): one synchronous HTTP/1.1 JSON request per
//! connection, with a connect timeout and a call timeout, to a peer of this process's uid.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustix::net::sockopt::{self, Timeout};
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use serde::Serialize;
use serde_json::{Map, Value};

/// Most bytes of the status line, header lines and empty line together.
const MAX_HEAD: usize = 8 * 1024;
/// Most bytes of a request or response body.
pub(crate) const MAX_BODY: usize = 512 * 1024;
const CHUNK: usize = 8 * 1024;

/// Why a call failed before its protocol could read an answer.
#[derive(Debug)]
pub(crate) enum Fault {
    /// No socket, connection refused, a socket failure, or the end of the stream before any
    /// byte of the answer.
    Unreachable(io::Error),
    /// The connect or the call timeout passed.
    Timeout,
    /// The peer runs as another uid; nothing was sent.
    PeerUid { expected: u32, actual: u32 },
    /// The request body is over [`MAX_BODY`]; nothing was sent.
    RequestTooLarge(usize),
    /// The answer is not HTTP/1.1 within the limits.
    Malformed(&'static str),
    /// The call's [`Cancel`] fired.
    Cancelled,
}

/// Ends the held calls made with it, at once and for good: the call in flight and every
/// later one fail as cancelled. Clones share the state.
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

    pub(crate) fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// Registers the stream of the call in flight.
    fn hold(&self, stream: &UnixStream) -> Result<(), Fault> {
        let mut held = self.lock();
        if held.cancelled {
            return Err(Fault::Cancelled);
        }
        held.stream = Some(stream.try_clone().map_err(Fault::Unreachable)?);
        Ok(())
    }

    fn release(&self) {
        self.lock().stream = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Held> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How long a call waits for its answer.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Wait {
    /// Until the call timeout.
    CallTimeout,
    /// Without a read timeout; the connect and the write keep their timeouts.
    Unbounded,
}

/// One peer's socket; every call opens its own connection.
#[derive(Debug, Clone)]
pub(crate) struct Endpoint {
    socket: PathBuf,
    connect_timeout: Duration,
    call_timeout: Duration,
    uid: u32,
}

impl Endpoint {
    /// The peer at `socket`, which must run as this process's effective uid. `connect_timeout`
    /// bounds the connect, `call_timeout` the whole call from connect to the last answer byte.
    pub(crate) fn new(
        socket: PathBuf,
        connect_timeout: Duration,
        call_timeout: Duration,
    ) -> io::Result<Self> {
        let (ours, _peer) = UnixStream::pair()?;
        let uid = sockopt::socket_peercred(&ours)?.uid.as_raw();
        Ok(Self {
            socket,
            connect_timeout,
            call_timeout,
            uid,
        })
    }

    /// Sends `input` as canonical JSON to `POST /v1/<method>` and reads the answer: its status
    /// and body. `cancel` can shut the stream down while the call waits `wait` for its answer.
    pub(crate) fn post(
        &self,
        method: &str,
        input: &impl Serialize,
        cancel: Option<&Cancel>,
        wait: Wait,
    ) -> Result<(u16, Vec<u8>), Fault> {
        let value = serde_json::to_value(input)
            .map_err(|_| Fault::Malformed("request does not serialize"))?;
        let body = canonical(&value);
        if body.len() > MAX_BODY {
            return Err(Fault::RequestTooLarge(body.len()));
        }
        let start = Instant::now();
        let deadline = start + self.call_timeout;
        let mut stream = connect(
            &self.socket,
            start + self.connect_timeout.min(self.call_timeout),
        )?;
        let actual = sockopt::socket_peercred(&stream)
            .map_err(|e| Fault::Unreachable(e.into()))?
            .uid
            .as_raw();
        if actual != self.uid {
            return Err(Fault::PeerUid {
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
        let reading = match wait {
            Wait::CallTimeout => Some(deadline),
            Wait::Unbounded => None,
        };
        let answer = exchange(&mut stream, &request, deadline, reading);
        if let Some(cancel) = cancel {
            cancel.release();
        }
        answer
    }
}

/// Writes `request` before `deadline` and reads its answer before `reading`, if any.
fn exchange(
    stream: &mut UnixStream,
    request: &[u8],
    deadline: Instant,
    reading: Option<Instant>,
) -> Result<(u16, Vec<u8>), Fault> {
    stream
        .set_write_timeout(Some(left(deadline)?))
        .map_err(Fault::Unreachable)?;
    stream.write_all(request).map_err(io_error)?;
    read_response(stream, reading)
}

/// Connects to `socket` before `deadline`: a listener whose backlog stays full until then is
/// [`Fault::Timeout`].
fn connect(socket: &Path, deadline: Instant) -> Result<UnixStream, Fault> {
    let errno = |e: rustix::io::Errno| Fault::Unreachable(e.into());
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

/// Reads one answer until `deadline`, if any: its status and body.
fn read_response(
    stream: &mut UnixStream,
    deadline: Option<Instant>,
) -> Result<(u16, Vec<u8>), Fault> {
    let mut buf = Vec::with_capacity(CHUNK);
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() >= MAX_HEAD {
            return Err(Fault::Malformed("head over 8 KiB"));
        }
        if read_some(stream, &mut buf, CHUNK, deadline)? == 0 {
            if buf.is_empty() {
                return Err(Fault::Unreachable(io::ErrorKind::UnexpectedEof.into()));
            }
            return Err(Fault::Malformed("end of stream before the whole answer"));
        }
    };
    let head =
        std::str::from_utf8(&buf[..head_end]).map_err(|_| Fault::Malformed("head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status = status_line(lines.next().unwrap_or_default())?;
    let length = content_length(lines)?;
    if length > MAX_BODY {
        return Err(Fault::Malformed("body over 512 KiB"));
    }
    let mut body = buf.split_off(head_end + 4);
    while body.len() < length {
        let missing = length - body.len();
        if read_some(stream, &mut body, missing, deadline)? == 0 {
            return Err(Fault::Malformed("end of stream before the whole answer"));
        }
    }
    if body.len() > length || read_some(stream, &mut body, 1, deadline)? != 0 {
        return Err(Fault::Malformed("bytes after the body"));
    }
    Ok((status, body))
}

/// Appends at most `max` bytes that arrive before `deadline`, if any; 0 at the end of the
/// stream.
fn read_some(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    max: usize,
    deadline: Option<Instant>,
) -> Result<usize, Fault> {
    let timeout = deadline.map(left).transpose()?;
    stream
        .set_read_timeout(timeout)
        .map_err(Fault::Unreachable)?;
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

/// Time left until `deadline`; none left is [`Fault::Timeout`].
fn left(deadline: Instant) -> Result<Duration, Fault> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(Fault::Timeout)
    } else {
        Ok(left)
    }
}

/// A socket timeout surfaces as `WouldBlock` or `TimedOut`.
fn io_error(e: io::Error) -> Fault {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Fault::Timeout,
        _ => Fault::Unreachable(e),
    }
}

/// The status of a status line `HTTP/1.1 <3 digits> <reason>`.
fn status_line(line: &str) -> Result<u16, Fault> {
    let mut parts = line.splitn(3, ' ');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("HTTP/1.1"), Some(code), Some(_))
            if code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit()) =>
        {
            code.parse()
                .map_err(|_| Fault::Malformed("malformed status line"))
        }
        _ => Err(Fault::Malformed("malformed status line")),
    }
}

/// The `Content-Length` of the header lines; `Transfer-Encoding` is refused.
fn content_length<'a>(lines: impl Iterator<Item = &'a str>) -> Result<usize, Fault> {
    let mut length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(Fault::Malformed("malformed header"))?;
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(Fault::Malformed("Transfer-Encoding is refused"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Fault::Malformed("malformed Content-Length"));
            }
            // All digits, so a parse failure is an overflow: far over the body limit.
            length = Some(value.parse::<usize>().unwrap_or(usize::MAX));
        }
    }
    length.ok_or(Fault::Malformed("Content-Length is required"))
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
