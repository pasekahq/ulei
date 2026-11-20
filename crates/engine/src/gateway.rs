//! Client of the C4G qb model gateway (ADR-0011, ADR-0012): HTTP/1.1 over a Unix socket, one
//! request per connection; a turn is answered by a chunked NDJSON stream of `StreamEvent`s. Its
//! connection reader also serves the MCP client over loopback TCP (ADR-0015).

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use ulei_contracts::generated::c4g::{
    CancelAnswer, CancelRequest, Error as GatewayError, EventType, RoutesAnswer, StreamEvent,
    TurnRequest,
};

/// Most bytes of the status line, header lines and empty line of an answer together.
pub const MAX_HEAD: usize = 8 * 1024;
/// Most bytes of a `Content-Length` answer body.
pub const MAX_BODY: usize = 512 * 1024;
/// Most bytes of one NDJSON event line, without its newline.
pub const MAX_LINE: usize = 4 * 1024 * 1024;
/// Longest wait for the connect and the answer of `POST /v1/model/cancel`, and so for anything
/// from qb when a turn is interrupted or `run` exits. qb answers once the turn has ended or after
/// its own 5 s wait, always `cancelled: true`, and 404 for a turn that is unknown or has ended,
/// so an answer within this bound means the turn has ended.
pub const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);

/// Most bytes of a chunk-size line, without its CRLF.
const MAX_CHUNK_LINE: usize = 64;
const READ: usize = 8 * 1024;
const MAX_CONNECT_PAUSE: Duration = Duration::from_millis(16);

/// Why a gateway call or a turn failed.
#[derive(Debug)]
pub enum Error {
    /// Connecting, writing or reading failed.
    Io(io::Error),
    /// The connect timeout passed, or the gateway sent nothing for one idle timeout.
    Timeout,
    /// The gateway rejected the request before any work started.
    Rejected {
        /// HTTP status of the answer; informative, `error.class` is authoritative.
        status: u16,
        /// The answer body.
        error: GatewayError,
    },
    /// The answer breaks the wire format of C4G.
    Malformed(Malformed),
    /// The turn stream ended without a terminal `stop` or `error` event.
    Cut,
    /// The turn was cancelled through its [`CancelHandle`].
    Cancelled,
}

/// How an answer breaks the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Malformed {
    /// The status line or a header is malformed or not allowed for this answer.
    Head(&'static str),
    /// The head exceeds [`MAX_HEAD`] bytes.
    HeadTooLarge,
    /// A `Content-Length` body is over [`MAX_BODY`], short, followed by bytes, not JSON, or not
    /// valid for its definition.
    Body(&'static str),
    /// A chunk-size line is not a hexadecimal size.
    ChunkSize,
    /// Chunk data is not followed by CRLF.
    ChunkEnd,
    /// An event line exceeds [`MAX_LINE`] bytes.
    LineTooLarge,
    /// An event line is not JSON.
    NotJson,
    /// An event line is JSON but not a `StreamEvent`: unknown type, field or value.
    UnknownEvent,
    /// An event does not carry exactly the one payload field of its type.
    WrongPayload,
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Head(what) | Self::Body(what) => f.write_str(what),
            Self::HeadTooLarge => write!(f, "head over {MAX_HEAD} bytes"),
            Self::ChunkSize => f.write_str("bad chunk size"),
            Self::ChunkEnd => f.write_str("chunk data not followed by CRLF"),
            Self::LineTooLarge => write!(f, "event line over {MAX_LINE} bytes"),
            Self::NotJson => f.write_str("event line is not JSON"),
            Self::UnknownEvent => f.write_str("event line is not a StreamEvent"),
            Self::WrongPayload => f.write_str("event payload does not match its type"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "gateway socket: {e}"),
            Self::Timeout => f.write_str("gateway timed out"),
            Self::Rejected { status, error } => {
                write!(
                    f,
                    "gateway rejected the request: {status} {}",
                    error.class.as_str()
                )?;
                match &error.message {
                    Some(message) => write!(f, ": {message}"),
                    None => Ok(()),
                }
            }
            Self::Malformed(what) => write!(f, "malformed gateway answer: {what}"),
            Self::Cut => f.write_str("turn stream ended without a terminal event"),
            Self::Cancelled => f.write_str("turn cancelled"),
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

/// A gateway client bound to one socket; every call opens its own connection.
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    connect_timeout: Duration,
    idle_timeout: Duration,
}

impl Client {
    /// A client of the gateway at `socket`. `connect_timeout` bounds the connect (a full listen
    /// backlog); `idle_timeout`, which must be non-zero, bounds every single read and write.
    pub fn new(
        socket: impl Into<PathBuf>,
        connect_timeout: Duration,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            socket: socket.into(),
            connect_timeout,
            idle_timeout,
        }
    }

    /// `GET /v1/model/routes`.
    ///
    /// # Errors
    /// [`Error`] other than [`Error::Cut`] and [`Error::Cancelled`].
    pub fn routes(&self) -> Result<RoutesAnswer, Error> {
        let body = self.call("GET /v1/model/routes", None, None)?;
        serde_json::from_slice(&body)
            .map_err(|_| Error::Malformed(Malformed::Body("not a RoutesAnswer")))
    }

    /// Sends `POST /v1/model/turn` and returns its stream; the answer is read as [`Turn`] is
    /// iterated, so a rejection is its first item. A send the gateway cut short by closing the
    /// connection still yields the answer it sent, and [`Error::Io`] of the write when none.
    ///
    /// # Errors
    /// [`Error::Io`] or [`Error::Timeout`] while connecting or sending.
    pub fn turn(&self, request: &TurnRequest) -> Result<Turn, Error> {
        let body = serde_json::to_vec(request).map_err(|e| Error::Io(e.into()))?;
        let (stream, unsent) = self.send("POST /v1/model/turn", Some(&body))?;
        let local = stream.try_clone().map_err(Error::Io)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        Ok(Turn {
            conn: Conn::new(stream, Arc::clone(&cancelled), self.idle_timeout),
            unsent,
            streaming: false,
            chunk: Chunk::Size,
            done: false,
            line: Vec::new(),
            scanned: 0,
            handle: CancelHandle {
                client: self.clone(),
                request_id: request.request_id.clone(),
                stream: Arc::new(local),
                cancelled,
            },
        })
    }

    /// `POST /v1/model/cancel` for `request_id`.
    fn cancel(&self, request_id: &str) -> Result<CancelAnswer, Error> {
        let request = CancelRequest {
            request_id: request_id.to_owned(),
        };
        let body = serde_json::to_vec(&request).map_err(|e| Error::Io(e.into()))?;
        let deadline = Instant::now() + CANCEL_TIMEOUT;
        let bounded = Self {
            connect_timeout: self.connect_timeout.min(CANCEL_TIMEOUT),
            idle_timeout: self.idle_timeout.min(CANCEL_TIMEOUT),
            ..self.clone()
        };
        let answer = bounded.call("POST /v1/model/cancel", Some(&body), Some(deadline))?;
        serde_json::from_slice(&answer)
            .map_err(|_| Error::Malformed(Malformed::Body("not a CancelAnswer")))
    }

    /// Sends one request and returns the body of its `200` answer, read before `deadline`.
    fn call(
        &self,
        line: &str,
        body: Option<&[u8]>,
        deadline: Option<Instant>,
    ) -> Result<Vec<u8>, Error> {
        let (stream, unsent) = self.send(line, body)?;
        let mut conn = Conn::new(stream, Arc::new(AtomicBool::new(false)), self.idle_timeout);
        conn.deadline = deadline;
        let (status, body) = conn
            .head()
            .and_then(|head| {
                if head.chunked {
                    return Err(Error::Malformed(Malformed::Head("chunked answer")));
                }
                Ok((head.status, conn.body(head.length, MAX_BODY)?))
            })
            .map_err(|e| unread(e, unsent))?;
        if status == 200 {
            Ok(body)
        } else {
            Err(rejection(status, &body))
        }
    }

    /// Connects and writes `<line> HTTP/1.1`, the headers and `body`. A write the gateway cut
    /// by closing the connection still returns the stream, with that failure: the gateway may
    /// answer from the head alone and close before it reads the body.
    fn send(
        &self,
        line: &str,
        body: Option<&[u8]>,
    ) -> Result<(UnixStream, Option<io::Error>), Error> {
        let mut stream = self.connect()?;
        let mut request = format!("{line} HTTP/1.1\r\nHost: localhost\r\n").into_bytes();
        if let Some(body) = body {
            request.extend_from_slice(
                format!(
                    "Content-Type: application/json\r\nContent-Length: {}\r\n",
                    body.len()
                )
                .as_bytes(),
            );
        }
        request.extend_from_slice(b"Connection: close\r\n\r\n");
        request.extend_from_slice(body.unwrap_or_default());
        match stream.write_all(&request) {
            Ok(()) => Ok((stream, None)),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) =>
            {
                Ok((stream, Some(e)))
            }
            Err(e) => Err(io_error(e)),
        }
    }

    /// A blocking stream with the idle timeout on reads and writes. A Unix connect waits only
    /// while the listen backlog is full, which a non-blocking connect reports as `EAGAIN`.
    fn connect(&self) -> Result<UnixStream, Error> {
        let addr = SocketAddrUnix::new(self.socket.as_path()).map_err(|e| Error::Io(e.into()))?;
        let fd = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )
        .map_err(|e| Error::Io(e.into()))?;
        let deadline = Instant::now() + self.connect_timeout;
        let mut pause = Duration::from_millis(1);
        loop {
            match rustix::net::connect(&fd, &addr) {
                Ok(()) => break,
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(Error::Timeout);
                    }
                    std::thread::sleep(pause.min(left));
                    pause = (pause * 2).min(MAX_CONNECT_PAUSE);
                }
                Err(e) => return Err(Error::Io(e.into())),
            }
        }
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(false).map_err(Error::Io)?;
        stream
            .set_read_timeout(Some(self.idle_timeout))
            .map_err(Error::Io)?;
        stream
            .set_write_timeout(Some(self.idle_timeout))
            .map_err(Error::Io)?;
        Ok(stream)
    }
}

/// The failure `read` of reading an answer, or the failed write `unsent` of a request the
/// gateway cut short, since then no answer came; a cancel stays [`Error::Cancelled`].
fn unread(read: Error, unsent: Option<io::Error>) -> Error {
    match (read, unsent) {
        (Error::Cancelled, _) => Error::Cancelled,
        (read, unsent) => unsent.map_or(read, Error::Io),
    }
}

/// A non-200 answer: its body must be a C4G `Error`.
fn rejection(status: u16, body: &[u8]) -> Error {
    match serde_json::from_slice(body) {
        Ok(error) => Error::Rejected { status, error },
        Err(_) => Error::Malformed(Malformed::Body("non-200 body is not an Error")),
    }
}

/// Stops a running turn from any thread.
#[derive(Debug, Clone)]
pub struct CancelHandle {
    client: Client,
    request_id: String,
    stream: Arc<UnixStream>,
    cancelled: Arc<AtomicBool>,
}

impl CancelHandle {
    /// [`CancelHandle::stop`], then posts `POST /v1/model/cancel` with the turn's `request_id`,
    /// waiting at most [`CANCEL_TIMEOUT`] for qb's answer.
    ///
    /// # Errors
    /// [`Error`] of the local shutdown or of the cancel call, a 404 [`Error::Rejected`] when
    /// the turn had ended before the post; the local stream ends either way.
    pub fn cancel(&self) -> Result<CancelAnswer, Error> {
        self.stop().map_err(Error::Io)?;
        self.client.cancel(&self.request_id)
    }

    /// Ends the local stream of the turn at once, so its next item is [`Error::Cancelled`]; qb
    /// cancels the turn when it sees the stream closed, and nothing waits for it.
    ///
    /// # Errors
    /// The failure of the shutdown; the local stream ends either way.
    pub fn stop(&self) -> io::Result<()> {
        self.cancelled.store(true, Ordering::SeqCst);
        self.stream.shutdown(Shutdown::Both)
    }
}

/// The event stream of one turn. Yields each event as its line arrives and ends after the
/// terminal `stop` or `error` event, or after the first error.
pub struct Turn {
    conn: Conn<UnixStream>,
    /// The failure of a request write the gateway cut short, until the answer is read.
    unsent: Option<io::Error>,
    /// The head was read and is a `200` stream.
    streaming: bool,
    chunk: Chunk,
    done: bool,
    line: Vec<u8>,
    /// Bytes of `line` already searched for a newline.
    scanned: usize,
    handle: CancelHandle,
}

/// Position in the framing of a streamed body.
#[derive(Clone, Copy)]
pub(crate) enum Chunk {
    /// A chunk-size line is next.
    Size,
    /// This many bytes of chunk data are left.
    Data(usize),
    /// The CRLF after chunk data is next.
    End,
    /// Not chunked: the body runs to the end of the stream.
    Close,
}

impl Turn {
    /// A handle that cancels this turn.
    pub fn cancel_handle(&self) -> CancelHandle {
        self.handle.clone()
    }

    /// Bounds the rest of the answer: a read past `deadline` fails with [`Error::Timeout`].
    pub fn set_deadline(&mut self, deadline: Instant) {
        self.conn.deadline = Some(deadline);
    }

    /// Reads the head of the answer: `None` when it opens a `200` stream, else the rejection
    /// its body carries.
    fn open(&mut self) -> Result<Option<Error>, Error> {
        let head = self.conn.head()?;
        if head.status == 200 {
            if !head.chunked {
                return Err(Error::Malformed(Malformed::Head(
                    "turn stream is not chunked",
                )));
            }
            return Ok(None);
        }
        if head.chunked {
            return Err(Error::Malformed(Malformed::Head("chunked rejection")));
        }
        let body = self.conn.body(head.length, MAX_BODY)?;
        Ok(Some(rejection(head.status, &body)))
    }

    fn event(&mut self) -> Result<StreamEvent, Error> {
        if !self.streaming {
            let unsent = self.unsent.take();
            if let Some(rejected) = self.open().map_err(|e| unread(e, unsent))? {
                return Err(rejected);
            }
            self.streaming = true;
        }
        let line = next_line(
            &mut self.conn,
            &mut self.chunk,
            &mut self.line,
            &mut self.scanned,
        )?;
        let event: StreamEvent = serde_json::from_slice(&line).map_err(|e| {
            Error::Malformed(if e.is_data() {
                Malformed::UnknownEvent
            } else {
                Malformed::NotJson
            })
        })?;
        if !payload_matches(&event) {
            return Err(Error::Malformed(Malformed::WrongPayload));
        }
        Ok(event)
    }
}

impl Iterator for Turn {
    type Item = Result<StreamEvent, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = if self.handle.cancelled.load(Ordering::SeqCst) {
            Err(Error::Cancelled)
        } else {
            self.event()
        };
        let ends = match &item {
            Ok(event) => matches!(event.r#type, EventType::Stop | EventType::Error),
            Err(_) => true,
        };
        if ends {
            self.done = true;
        }
        Some(item)
    }
}

/// The next NDJSON line without its newline, reassembled across chunks into `line`, of which
/// the first `scanned` bytes hold no newline.
pub(crate) fn next_line<S: Stream>(
    conn: &mut Conn<S>,
    chunk: &mut Chunk,
    line: &mut Vec<u8>,
    scanned: &mut usize,
) -> Result<Vec<u8>, Error> {
    loop {
        if let Some(i) = line[*scanned..].iter().position(|&b| b == b'\n') {
            let end = *scanned + i;
            if end > MAX_LINE {
                return Err(Error::Malformed(Malformed::LineTooLarge));
            }
            let rest = line.split_off(end + 1);
            let mut done = std::mem::replace(line, rest);
            done.pop();
            *scanned = 0;
            return Ok(done);
        }
        *scanned = line.len();
        if *scanned > MAX_LINE {
            return Err(Error::Malformed(Malformed::LineTooLarge));
        }
        feed(conn, chunk, line)?;
    }
}

/// Moves the next bytes of a streamed body into `out` or steps over its framing; the end of
/// the body is [`Error::Cut`].
pub(crate) fn feed<S: Stream>(
    conn: &mut Conn<S>,
    chunk: &mut Chunk,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    *chunk = match *chunk {
        Chunk::Size => match chunk_size(&conn.size_line()?)? {
            0 => return Err(Error::Cut),
            size => Chunk::Data(size),
        },
        Chunk::Data(left) => match conn.take(left, out)? {
            taken if taken == left => Chunk::End,
            taken => Chunk::Data(left - taken),
        },
        Chunk::End => {
            conn.crlf()?;
            Chunk::Size
        }
        Chunk::Close => {
            conn.take(usize::MAX, out)?;
            Chunk::Close
        }
    };
    Ok(())
}

/// The event carries the payload field of its type and no other.
fn payload_matches(event: &StreamEvent) -> bool {
    let present = [
        event.route.is_some(),
        event.text.is_some(),
        event.thinking.is_some(),
        event.tool_call.is_some(),
        event.usage.is_some(),
        event.reason.is_some(),
        event.error.is_some(),
    ];
    let wanted = match event.r#type {
        EventType::Route => 0,
        EventType::TextDelta => 1,
        EventType::Thinking => 2,
        EventType::ToolCall => 3,
        EventType::Usage => 4,
        EventType::Stop => 5,
        EventType::Error => 6,
    };
    present.iter().enumerate().all(|(i, &p)| p == (i == wanted))
}

/// The size of a chunk-size line: hexadecimal, optionally followed by `;` extensions.
fn chunk_size(line: &[u8]) -> Result<usize, Error> {
    let digits = line.split(|&b| b == b';').next().unwrap_or_default();
    if digits.is_empty() || digits.len() > 8 || !digits.iter().all(u8::is_ascii_hexdigit) {
        return Err(Error::Malformed(Malformed::ChunkSize));
    }
    let digits = std::str::from_utf8(digits).map_err(|_| Error::Malformed(Malformed::ChunkSize))?;
    usize::from_str_radix(digits, 16).map_err(|_| Error::Malformed(Malformed::ChunkSize))
}

/// Status line, framing headers and the MCP headers of an answer.
pub(crate) struct Head {
    pub(crate) status: u16,
    pub(crate) chunked: bool,
    pub(crate) length: Option<usize>,
    /// `Content-Type`, as sent.
    pub(crate) content_type: Option<String>,
    /// `Mcp-Session-Id`, as sent.
    pub(crate) session_id: Option<String>,
}

/// A connected stream whose reads can be bounded.
pub(crate) trait Stream: Read {
    /// Bounds each later read; `None` waits without a bound.
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
}

impl Stream for UnixStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        UnixStream::set_read_timeout(self, timeout)
    }
}

impl Stream for &UnixStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        UnixStream::set_read_timeout(self, timeout)
    }
}

impl Stream for TcpStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }
}

/// Buffered reads of one connection; a cancelled turn turns every read failure into
/// [`Error::Cancelled`].
pub(crate) struct Conn<S> {
    stream: S,
    buf: Vec<u8>,
    pos: usize,
    cancelled: Arc<AtomicBool>,
    idle: Duration,
    /// A read past it fails with [`Error::Timeout`].
    pub(crate) deadline: Option<Instant>,
}

impl<S: Stream> Conn<S> {
    pub(crate) fn new(stream: S, cancelled: Arc<AtomicBool>, idle: Duration) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(READ),
            pos: 0,
            cancelled,
            idle,
            deadline: None,
        }
    }

    fn buffered(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    /// Reads more bytes; the end of the stream is [`Error::Cut`], a passed deadline
    /// [`Error::Timeout`].
    fn fill(&mut self) -> Result<(), Error> {
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos >= READ {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        if let Some(deadline) = self.deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout);
            }
            self.stream
                .set_read_timeout(Some(left.min(self.idle)))
                .map_err(Error::Io)?;
        }
        let start = self.buf.len();
        self.buf.resize(start + READ, 0);
        let read = loop {
            match self.stream.read(&mut self.buf[start..]) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => break other,
            }
        };
        self.buf.truncate(start + *read.as_ref().unwrap_or(&0));
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        match read {
            Ok(0) => Err(Error::Cut),
            Ok(_) => Ok(()),
            Err(e) => Err(io_error(e)),
        }
    }

    /// Reads the head of an answer.
    pub(crate) fn head(&mut self) -> Result<Head, Error> {
        let (line, framing) = self.message_head()?;
        Ok(Head {
            status: status_code(&line)?,
            chunked: framing.chunked,
            length: framing.length,
            content_type: framing.content_type,
            session_id: framing.session_id,
        })
    }

    /// Reads the head of a request: its request line and its `Content-Length`. A
    /// `Transfer-Encoding` header is malformed.
    pub(crate) fn request_head(&mut self) -> Result<(String, Option<usize>), Error> {
        let (line, framing) = self.message_head()?;
        if framing.chunked {
            return Err(Error::Malformed(Malformed::Head(
                "Transfer-Encoding is refused",
            )));
        }
        Ok((line, framing.length))
    }

    /// Reads a head up to its empty line: its start line and the framing of its header lines.
    fn message_head(&mut self) -> Result<(String, Framing), Error> {
        let end = loop {
            if let Some(i) = self.buffered().windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            if self.buffered().len() >= MAX_HEAD {
                return Err(Error::Malformed(Malformed::HeadTooLarge));
            }
            self.fill()?;
        };
        if end + 4 > MAX_HEAD {
            return Err(Error::Malformed(Malformed::HeadTooLarge));
        }
        let head = parse_head(&self.buf[self.pos..self.pos + end]);
        self.pos += end + 4;
        head
    }

    /// Reads a body of exactly `length` bytes, at most `max`, up to the end of the stream.
    pub(crate) fn body(&mut self, length: Option<usize>, max: usize) -> Result<Vec<u8>, Error> {
        let length = length.ok_or(Error::Malformed(Malformed::Head(
            "Content-Length is required",
        )))?;
        if length > max {
            return Err(Error::Malformed(Malformed::Body("body over the limit")));
        }
        while self.buffered().len() < length {
            match self.fill() {
                Err(Error::Cut) => return Err(Error::Malformed(Malformed::Body("short body"))),
                other => other?,
            }
        }
        if self.buffered().len() > length {
            return Err(Error::Malformed(Malformed::Body("bytes after the body")));
        }
        Ok(self.buffered().to_vec())
    }

    /// Reads and drops up to `length` body bytes, until the end of the stream, a failure or the
    /// deadline.
    pub(crate) fn discard(&mut self, mut length: usize) {
        loop {
            let n = self.buffered().len().min(length);
            self.pos += n;
            length -= n;
            if length == 0 || self.fill().is_err() {
                return;
            }
        }
    }

    /// The next chunk-size line, without its CRLF.
    fn size_line(&mut self) -> Result<Vec<u8>, Error> {
        loop {
            if let Some(i) = self.buffered().windows(2).position(|w| w == b"\r\n") {
                if i > MAX_CHUNK_LINE {
                    break;
                }
                let line = self.buffered()[..i].to_vec();
                self.pos += i + 2;
                return Ok(line);
            }
            if self.buffered().len() > MAX_CHUNK_LINE + 1 {
                break;
            }
            self.fill()?;
        }
        Err(Error::Malformed(Malformed::ChunkSize))
    }

    /// Consumes the CRLF that ends chunk data.
    fn crlf(&mut self) -> Result<(), Error> {
        while self.buffered().len() < 2 {
            self.fill()?;
        }
        if !self.buffered().starts_with(b"\r\n") {
            return Err(Error::Malformed(Malformed::ChunkEnd));
        }
        self.pos += 2;
        Ok(())
    }

    /// Moves up to `max` bytes into `out`, reading if none is buffered; returns how many.
    fn take(&mut self, max: usize, out: &mut Vec<u8>) -> Result<usize, Error> {
        if self.buffered().is_empty() {
            self.fill()?;
        }
        let n = self.buffered().len().min(max);
        out.extend_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// The framing headers of a head.
struct Framing {
    chunked: bool,
    length: Option<usize>,
    content_type: Option<String>,
    session_id: Option<String>,
}

/// The start line of `head` and the framing of its header lines.
fn parse_head(head: &[u8]) -> Result<(String, Framing), Error> {
    let bad = |what| Error::Malformed(Malformed::Head(what));
    let head = std::str::from_utf8(head).map_err(|_| bad("head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let start = lines.next().unwrap_or_default().to_owned();
    let mut chunked = false;
    let mut length = None;
    let (mut content_type, mut session_id) = (None, None);
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(bad("malformed header"))?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(bad("malformed header"));
        }
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err(bad("Transfer-Encoding other than chunked"));
            }
            chunked = true;
        } else if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad("malformed Content-Length"));
            }
            // All digits, so a parse failure is an overflow: far over the body limit.
            length = Some(value.parse::<usize>().unwrap_or(usize::MAX));
        } else if name.eq_ignore_ascii_case("content-type") {
            content_type.get_or_insert_with(|| value.to_owned());
        } else if name.eq_ignore_ascii_case("mcp-session-id") {
            session_id.get_or_insert_with(|| value.to_owned());
        }
    }
    if chunked && length.is_some() {
        return Err(bad("both Transfer-Encoding and Content-Length"));
    }
    Ok((
        start,
        Framing {
            chunked,
            length,
            content_type,
            session_id,
        },
    ))
}

/// The status of a status line `HTTP/1.1 <3 digits> <reason>`.
fn status_code(line: &str) -> Result<u16, Error> {
    let bad = || Error::Malformed(Malformed::Head("malformed status line"));
    let mut parts = line.splitn(3, ' ');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("HTTP/1.1"), Some(code), Some(_))
            if code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit()) =>
        {
            code.parse().map_err(|_| bad())
        }
        _ => Err(bad()),
    }
}

/// A socket timeout surfaces as `WouldBlock` or `TimedOut`.
pub(crate) fn io_error(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Error::Timeout,
        _ => Error::Io(e),
    }
}
