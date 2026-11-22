//! The daemon `ulei-memory serve` (ADR-0051 section 2): its settings from `ulei.toml`, the
//! private socket, and the C10 `x-methods` but `delete` over `x-transport`, synchronous HTTP/1.1
//! JSON on a Unix socket, one request per connection, served one connection at a time.

mod base64;

use std::fmt;
use std::fs::{self, DirBuilder};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use ulei_contracts::config::{self, Config, Entry};

use crate::archive::{self, Archive, Item};
use crate::wire::c10::{self, ErrorCode};

/// Longest socket path in bytes: `sun_path` holds 108 bytes with the trailing NUL.
pub const MAX_SOCKET_PATH: usize = 107;
/// Deadline of every read and of every write of a connection.
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// Most bytes of a request body.
pub const MAX_BODY: usize = 1024 * 1024;
/// Most bytes of the request line, header lines and empty line together.
pub const MAX_HEAD: usize = 8 * 1024;
/// The server name `health` answers.
pub const VERSION: &str = concat!("ulei-memory ", env!("CARGO_PKG_VERSION"));

const CHUNK: usize = 8 * 1024;

/// What the daemon and the `delete` command read from `ulei.toml`.
#[derive(Clone, Debug)]
pub struct Settings {
    /// `memory.archive`.
    pub archive: PathBuf,
    /// `memory.socket`.
    pub socket: PathBuf,
}

impl Settings {
    /// The settings of the file `flag` names, else of the one [`Config::read`] finds; an error
    /// is one line naming the file, the line when the key is set, and the key.
    ///
    /// # Errors
    /// The file cannot be found or decoded, a key is missing, empty or not a string.
    pub fn read(flag: Option<&Path>) -> Result<Self, String> {
        let config = Config::read(flag).map_err(|e| e.to_string())?;
        let required = |key: &str| {
            string(&config, key)?
                .map(|(_, s)| PathBuf::from(s))
                .ok_or_else(|| format!("config {}: {key} is missing", config.path.display()))
        };
        Ok(Self {
            archive: required("memory.archive")?,
            socket: required("memory.socket")?,
        })
    }
}

/// The one-line refusal of `entry`.
fn refuse(config: &Config, entry: &Entry, fault: impl fmt::Display) -> String {
    format!(
        "config {}:{}: {} {fault}",
        config.path.display(),
        entry.line,
        entry.key
    )
}

/// The entry at `key` and its non-empty string, if set.
fn string<'a>(config: &'a Config, key: &str) -> Result<Option<(&'a Entry, &'a str)>, String> {
    match config.get(key) {
        None => Ok(None),
        Some(entry) => match &entry.value {
            config::Value::String(s) if !s.is_empty() => Ok(Some((entry, s))),
            config::Value::String(_) => Err(refuse(config, entry, "is empty")),
            _ => Err(refuse(config, entry, "must be a string")),
        },
    }
}

/// Why the socket could not be bound.
#[derive(Debug)]
pub enum BindError {
    /// The path is longer than [`MAX_SOCKET_PATH`] bytes.
    PathTooLong(usize),
    /// The socket's directory is not owned by the daemon's uid or is open to other users.
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

/// A C10 `x-methods` entry the socket serves; `delete` is the owner's command only.
#[derive(Clone, Copy, Debug)]
enum Method {
    Put,
    Search,
    Read,
    Sync,
    Health,
}

impl Method {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "put" => Self::Put,
            "search" => Self::Search,
            "read" => Self::Read,
            "sync" => Self::Sync,
            "health" => Self::Health,
            _ => return None,
        })
    }
}

/// An HTTP status and its JSON body.
type Reply = (u16, Vec<u8>);

/// The archive served to peers of one uid.
pub struct Server {
    archive: Archive,
    uid: u32,
    timeout: Duration,
}

impl Server {
    /// A server over `archive` that serves peers of `uid` and gives every read and write of a
    /// connection `timeout`.
    #[must_use]
    pub const fn new(archive: Archive, uid: u32, timeout: Duration) -> Self {
        Self {
            archive,
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

    /// Serves one connection: checks the peer uid before reading, then reads one request,
    /// answers it with `Connection: close` and closes. A connection that fails or misses its
    /// deadline is closed without an answer.
    pub fn serve(&mut self, mut stream: UnixStream) {
        let reply = match rustix::net::sockopt::socket_peercred(&stream) {
            Ok(cred) if cred.uid.as_raw() == self.uid => self.answer(&mut stream),
            Ok(_) => Some(error(
                ErrorCode::Forbidden,
                "peer uid differs from the daemon",
            )),
            Err(_) => Some(error(ErrorCode::Forbidden, "peer credentials unavailable")),
        };
        if let Some(reply) = reply {
            self.send(&mut stream, &reply);
        }
    }

    fn send(&self, stream: &mut UnixStream, (status, body): &Reply) {
        if stream.set_write_timeout(Some(self.timeout)).is_ok() {
            let _ = stream.write_all(&response(*status, body));
        }
    }

    fn answer(&mut self, stream: &mut UnixStream) -> Option<Reply> {
        let deadline = Instant::now() + self.timeout;
        match read_request(stream, deadline) {
            Ok((line, body)) => Some(
                route(&line)
                    .and_then(|m| self.dispatch(m, &body))
                    .unwrap_or_else(|e| e),
            ),
            Err(Refused::Answer(code, message)) => Some(error(code, message)),
            Err(Refused::TooLarge(unread)) => {
                // Closing with the body unread would reset the client's read of the answer.
                self.send(stream, &error(ErrorCode::TooLarge, "body over 1 MiB"));
                let _ = stream.shutdown(Shutdown::Write);
                discard(stream, unread, deadline);
                None
            }
            Err(Refused::Close) => None,
        }
    }

    /// Runs `method` on the archive; `Err` is a C10 `Error` reply.
    fn dispatch(&mut self, method: Method, body: &[u8]) -> Result<Reply, Reply> {
        let invalid = |_| error(ErrorCode::InvalidRequest, "body is not the method input");
        let a = &mut self.archive;
        let out = match method {
            Method::Put => {
                let req: c10::PutRequest = serde_json::from_slice(body).map_err(invalid)?;
                let bytes = base64::decode(&req.bytes_b64).ok_or_else(|| {
                    error(
                        ErrorCode::InvalidRequest,
                        "bytes_b64 is not standard base64",
                    )
                })?;
                let item = Item {
                    source: &req.source,
                    external_id: &req.external_id,
                    who: req.who.as_deref(),
                    at: req.at,
                    kind: &req.kind,
                    text: &req.text,
                    bytes: &bytes,
                };
                serde_json::to_vec(&a.put(&item).map_err(|e| failure(&e))?)
            }
            Method::Search => {
                let req: c10::SearchRequest = serde_json::from_slice(body).map_err(invalid)?;
                serde_json::to_vec(&c10::SearchResponse {
                    hits: a.search(&req).map_err(|e| failure(&e))?,
                    synced: true,
                })
            }
            Method::Read => {
                let req: c10::ReadRequest = serde_json::from_slice(body).map_err(invalid)?;
                let original = a.read(&req.id).map_err(|e| failure(&e))?;
                serde_json::to_vec(&c10::ReadResponse {
                    bytes_b64: base64::encode(&original.bytes),
                    kind: original.kind,
                    len: original.len,
                    entries: original.entries,
                })
            }
            Method::Sync => {
                let c10::SyncRequest {} = serde_json::from_slice(body).map_err(invalid)?;
                serde_json::to_vec(&c10::SyncResponse {
                    put: 0,
                    synced: true,
                })
            }
            Method::Health => {
                let c10::HealthRequest {} = serde_json::from_slice(body).map_err(invalid)?;
                serde_json::to_vec(&c10::Health {
                    version: VERSION.to_owned(),
                    items: a.items().map_err(|e| failure(&e))?,
                    last_sync_at: None,
                    sync_error: None,
                })
            }
        };
        out.map(|body| (200, body))
            .map_err(|_| error(ErrorCode::StoreFailed, "answer does not serialize"))
    }
}

/// Why a request was not dispatched.
enum Refused {
    Answer(ErrorCode, &'static str),
    /// A body over [`MAX_BODY`], with this many of its bytes not read yet.
    TooLarge(usize),
    /// Deadline, I/O failure or end of stream before a whole request.
    Close,
}

/// Reads one request until `deadline`: its request line and its body.
fn read_request(stream: &mut UnixStream, deadline: Instant) -> Result<(String, Vec<u8>), Refused> {
    let too_large = Refused::Answer(ErrorCode::TooLarge, "headers over 8 KiB");
    let mut buf = Vec::with_capacity(CHUNK);
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() >= MAX_HEAD {
            return Err(too_large);
        }
        read_some(stream, &mut buf, CHUNK, deadline)?;
    };
    if head_end + 4 > MAX_HEAD {
        return Err(too_large);
    }
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| bad("headers are not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or_default().to_owned();
    let length = content_length(lines)?;
    let mut body = buf.split_off(head_end + 4);
    if length > MAX_BODY {
        return Err(Refused::TooLarge(length.saturating_sub(body.len())));
    }
    while body.len() < length {
        let missing = length - body.len();
        read_some(stream, &mut body, missing, deadline)?;
    }
    if body.len() > length {
        return Err(bad("bytes after the body"));
    }
    Ok((line, body))
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

/// The method of a request line `POST /v1/<name> HTTP/1.1`.
fn route(line: &str) -> Result<Method, Reply> {
    let mut parts = line.split(' ');
    let (Some(verb), Some(target), Some("HTTP/1.1"), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(error(ErrorCode::InvalidRequest, "malformed request line"));
    };
    target
        .strip_prefix("/v1/")
        .filter(|_| verb == "POST")
        .and_then(Method::from_name)
        .ok_or_else(|| error(ErrorCode::UnknownMethod, "no such method or path"))
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
    Refused::Answer(ErrorCode::InvalidRequest, message)
}

/// The C10 `Error` reply of a failure of the archive.
fn failure(e: &archive::Error) -> Reply {
    error(e.code(), &e.to_string())
}

/// The status and C10 `Error` body of `code`.
fn error(code: ErrorCode, message: &str) -> Reply {
    let status = match code {
        ErrorCode::InvalidRequest => 400,
        ErrorCode::Forbidden => 403,
        ErrorCode::UnknownMethod | ErrorCode::NotFound => 404,
        ErrorCode::Conflict => 409,
        ErrorCode::TooLarge => 413,
        ErrorCode::StoreFailed => 500,
    };
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
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
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
