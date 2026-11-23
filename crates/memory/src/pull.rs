//! The pull of the engine's raw dialogue into the archive (ADR-0051 section 3): C6
//! `raw_sessions` pages from the engine's socket after the stored cursor, each page's items,
//! skipped references and cursor stored in one transaction.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::archive::{self, Archive, Item, Page, Skip};
use crate::wire::c6::{Error as WireError, RawMessage, RawSessionsPage, RawSessionsQuery};

/// The source of the dialogue's items.
pub const SOURCE: &str = "dialogue";
/// Media type of a dialogue item: one C4G message in its canonical serialization.
pub const KIND: &str = "application/vnd.ulei.c4g-message+json";
/// Messages asked for per page. The engine's last page carries no cursor, so up to this many
/// messages are read again by the next pull and answered as duplicates.
pub const PAGE_LIMIT: u64 = 100;
/// Most bytes of an answer body, C6 `x-transport.max_body_bytes`.
pub const MAX_ANSWER_BODY: usize = 512 * 1024;
/// Most bytes of an answer's status line and headers.
const MAX_ANSWER_HEAD: usize = 8 * 1024;
const CHUNK: usize = 8 * 1024;

/// Why a pull stopped before the engine's last page.
#[derive(Debug)]
pub enum Error {
    /// The engine's socket failed or missed the deadline.
    Io(io::Error),
    /// The engine answered something other than a C6 `200` page.
    Answer(String),
    /// A page breaks the C6 shape the engine always sends.
    Malformed(String),
    /// The archive refused or failed to store a page.
    Archive(archive::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "engine.c6_socket: {e}"),
            Self::Answer(why) => write!(f, "engine.c6_socket: {why}"),
            Self::Malformed(why) => write!(f, "raw_sessions page: {why}"),
            Self::Archive(e) => write!(f, "archive: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// The engine's C6 socket, each exchange within `timeout`.
#[derive(Clone, Debug)]
pub struct Engine {
    socket: PathBuf,
    timeout: Duration,
}

impl Engine {
    /// The engine at `socket`; every exchange has `timeout` from connect to the last byte.
    #[must_use]
    pub const fn new(socket: PathBuf, timeout: Duration) -> Self {
        Self { socket, timeout }
    }

    /// The `raw_sessions` page after `cursor`.
    fn raw_sessions(&self, cursor: Option<u64>) -> Result<RawSessionsPage, Error> {
        let query = RawSessionsQuery {
            since: None,
            until: None,
            limit: Some(PAGE_LIMIT),
            cursor: cursor.map(|c| c.to_string()),
        };
        let body = serde_json::to_vec(&query).map_err(|e| Error::Malformed(e.to_string()))?;
        let (status, answer) = self.exchange("raw_sessions", &body)?;
        if status != 200 {
            let why = serde_json::from_slice::<WireError>(&answer)
                .map_or_else(|_| "no C6 error body".to_owned(), |e| e.message);
            return Err(Error::Answer(format!(
                "raw_sessions answered {status}: {why}"
            )));
        }
        serde_json::from_slice(&answer).map_err(|e| Error::Malformed(e.to_string()))
    }

    /// Sends `POST /v1/<method>` with `body` and reads the status and body of the answer.
    fn exchange(&self, method: &str, body: &[u8]) -> Result<(u16, Vec<u8>), Error> {
        let deadline = Instant::now() + self.timeout;
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_write_timeout(Some(self.timeout))?;
        let mut request = format!(
            "POST /v1/{method} HTTP/1.1\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        stream.write_all(&request)?;
        let mut buf = Vec::with_capacity(CHUNK);
        let head_end = loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
            if buf.len() > MAX_ANSWER_HEAD {
                return Err(Error::Answer("answer headers over 8 KiB".to_owned()));
            }
            read_some(&mut stream, &mut buf, deadline)?;
        };
        let head = std::str::from_utf8(&buf[..head_end])
            .map_err(|_| Error::Answer("answer headers are not UTF-8".to_owned()))?;
        let (status, length) = status_and_length(head)?;
        if length > MAX_ANSWER_BODY {
            return Err(Error::Answer(format!("answer body of {length} bytes")));
        }
        let mut answer = buf.split_off(head_end + 4);
        while answer.len() < length {
            read_some(&mut stream, &mut answer, deadline)?;
        }
        answer.truncate(length);
        Ok((status, answer))
    }
}

/// Appends the bytes that arrive before `deadline`; the end of the stream is an error.
fn read_some(stream: &mut UnixStream, buf: &mut Vec<u8>, deadline: Instant) -> Result<(), Error> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::Error::from(io::ErrorKind::TimedOut).into());
    }
    stream.set_read_timeout(Some(left))?;
    let mut chunk = [0u8; CHUNK];
    match stream.read(&mut chunk)? {
        0 => Err(Error::Answer(
            "connection closed before the whole answer".to_owned(),
        )),
        n => {
            buf.extend_from_slice(&chunk[..n]);
            Ok(())
        }
    }
}

/// The status of an answer's head and its `Content-Length`.
fn status_and_length(head: &str) -> Result<(u16, usize), Error> {
    let malformed = || Error::Answer("malformed answer head".to_owned());
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.strip_prefix("HTTP/1.1 "))
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse().ok())
        .ok_or_else(malformed)?;
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .ok_or_else(malformed)?;
    Ok((status, length))
}

/// Pulls every page after the archive's dialogue cursor; answers the items stored and, when a
/// page failed, why. The pages before a failure stay stored.
pub fn sync(archive: &mut Archive, engine: &Engine) -> (u64, Result<(), Error>) {
    let mut stored = 0;
    loop {
        match page(archive, engine) {
            Ok((put, more)) => {
                stored += put;
                if !more {
                    return (stored, Ok(()));
                }
            }
            Err(e) => return (stored, Err(e)),
        }
    }
}

/// Pulls and stores the page after the stored cursor; answers its stored items and whether the
/// engine has more.
fn page(archive: &mut Archive, engine: &Engine) -> Result<(u64, bool), Error> {
    let cursor = archive.cursor(SOURCE).map_err(Error::Archive)?;
    let raw = engine.raw_sessions(cursor)?;
    let next = raw
        .cursor
        .as_deref()
        .map(|c| {
            c.parse::<u64>()
                .map_err(|_| Error::Malformed(format!("cursor {c:?} is not a journal position")))
        })
        .transpose()?;
    if let (Some(next), Some(cursor)) = (next, cursor)
        && next <= cursor
    {
        return Err(Error::Malformed(format!(
            "cursor {next} does not advance past {cursor}"
        )));
    }
    let messages = raw
        .messages
        .iter()
        .map(dialogue)
        .collect::<Result<Vec<_>, _>>()?;
    let skipped = raw
        .skipped
        .iter()
        .flatten()
        .map(|s| {
            Ok((
                external_id(s.session_key.as_deref(), &s.session_id, s.seq)?,
                s.len,
                s.ts,
            ))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let page = Page {
        source: SOURCE,
        items: messages
            .iter()
            .map(|(external_id, m, message)| Item {
                source: SOURCE,
                external_id,
                who: Some(&m.role),
                at: m.ts,
                kind: KIND,
                text: &m.content,
                bytes: message.as_bytes(),
            })
            .collect(),
        skipped: skipped
            .iter()
            .map(|(external_id, len, at)| Skip {
                external_id,
                len: *len,
                at: *at,
            })
            .collect(),
        cursor: next,
    };
    let stored = archive.pull(&page).map_err(Error::Archive)?;
    Ok((stored, next.is_some()))
}

/// The external id, the message and its bytes of the dialogue item of `m`.
fn dialogue(m: &RawMessage) -> Result<(String, &RawMessage, &str), Error> {
    let message = m
        .message
        .as_deref()
        .ok_or_else(|| Error::Malformed(format!("message {} has no stored bytes", m.session_id)))?;
    Ok((
        external_id(m.session_key.as_deref(), &m.session_id, m.seq)?,
        m,
        message,
    ))
}

/// `<session_key>/<session>/<seq>`.
fn external_id(key: Option<&str>, session: &str, seq: Option<u64>) -> Result<String, Error> {
    match (key, seq) {
        (Some(key), Some(seq)) => Ok(format!("{key}/{session}/{seq}")),
        _ => Err(Error::Malformed(format!(
            "a message of session {session} without session_key or seq"
        ))),
    }
}
