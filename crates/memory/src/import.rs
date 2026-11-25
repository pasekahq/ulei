//! `ulei-memory import`: puts a stream of C10 `PutRequest` JSON lines into the archive through
//! the daemon's socket, the C10 transport the connectors use; the archive file is opened only by
//! the daemon and `delete`. One line at a time, so memory stays that of one line.

use std::fmt;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::serve::MAX_BODY;
use crate::wire::c10::{self, ErrorCode, PutStatus};

/// Deadline of each write of a request and of each read of its answer. The daemon serves one
/// connection at a time and pulls the engine's dialogue before a `search` or on `sync`, so a
/// `put` may wait behind such a request.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// Tries of one line whose transport fails, the first included.
pub const ATTEMPTS: u32 = 4;
/// Pause after the first failed try; it doubles after each further one.
pub const PAUSE: Duration = Duration::from_secs(1);
/// Lines between two progress lines.
pub const PROGRESS_EVERY: u64 = 10_000;
/// Most bytes of an answer the daemon sends to a `put`.
const MAX_ANSWER: u64 = 64 * 1024;

/// Where and how the lines are put.
#[derive(Clone, Debug)]
pub struct Client {
    /// `memory.socket`.
    pub socket: PathBuf,
    /// See [`TIMEOUT`].
    pub timeout: Duration,
    /// See [`ATTEMPTS`].
    pub attempts: u32,
    /// See [`PAUSE`].
    pub pause: Duration,
}

impl Client {
    /// A client of the daemon at `socket` with the default deadline, tries and pause.
    #[must_use]
    pub const fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            timeout: TIMEOUT,
            attempts: ATTEMPTS,
            pause: PAUSE,
        }
    }
}

/// What became of one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The daemon stored a new entry.
    Stored,
    /// The daemon had the entry with the same bytes.
    Duplicate,
    /// The line is not put: refused locally or by the daemon, with its C10 code.
    Refused(ErrorCode),
}

/// The counts of an import.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Lines read after the skipped ones.
    pub lines: u64,
    /// See [`Outcome::Stored`].
    pub stored: u64,
    /// See [`Outcome::Duplicate`].
    pub duplicate: u64,
    /// See [`Outcome::Refused`].
    pub refused: u64,
}

impl fmt::Display for Counts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} lines, {} stored, {} duplicate, {} refused",
            self.lines, self.stored, self.duplicate, self.refused
        )
    }
}

/// Why an import stopped before the end of its input.
#[derive(Debug)]
pub struct Stopped {
    /// The number of the line that could not be put or read, from 1.
    pub line: u64,
    /// The number of the last line fully handled; `--skip` with it resumes after it.
    pub done: u64,
    /// Why.
    pub reason: String,
}

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "stopped at line {}: {}; lines 1 to {} are done, resume with --skip {}",
            self.line, self.reason, self.done, self.done
        )
    }
}

/// A failure of a `put` that is not a refusal of its line.
#[derive(Debug)]
struct Fault {
    /// Another try may succeed: the transport failed or the daemon answered `store_failed`.
    transient: bool,
    reason: String,
}

impl Fault {
    fn transient(reason: impl fmt::Display) -> Self {
        Self {
            transient: true,
            reason: reason.to_string(),
        }
    }

    fn fatal(reason: impl fmt::Display) -> Self {
        Self {
            transient: false,
            reason: reason.to_string(),
        }
    }
}

/// Puts each line of `input` after the first `skip` through `client`. Writes one line to `log`
/// per refused line (its number and code), one per [`PROGRESS_EVERY`] lines, and none at the end:
/// the caller writes the counts. Stops at the first line whose transport still fails after
/// [`Client::attempts`] tries, or at a failure to read `input`.
///
/// # Errors
/// [`Stopped`], with the counts up to the stop.
pub fn import(
    input: &mut impl BufRead,
    skip: u64,
    client: &Client,
    log: &mut impl Write,
) -> Result<Counts, (Counts, Stopped)> {
    let mut counts = Counts::default();
    let mut number = 0u64;
    let mut line = Vec::new();
    loop {
        let read = match read_line(input, &mut line) {
            Ok(read) => read,
            Err(e) => {
                let stopped = Stopped {
                    line: number + 1,
                    done: number,
                    reason: format!("stdin: {e}"),
                };
                return Err((counts, stopped));
            }
        };
        let Some(whole) = read else {
            return Ok(counts);
        };
        number += 1;
        if number <= skip {
            continue;
        }
        let outcome = if whole {
            match put_line(client, &line) {
                Ok(outcome) => outcome,
                Err(fault) => {
                    let stopped = Stopped {
                        line: number,
                        done: number - 1,
                        reason: fault.reason,
                    };
                    return Err((counts, stopped));
                }
            }
        } else {
            Outcome::Refused(ErrorCode::TooLarge)
        };
        counts.lines += 1;
        match outcome {
            Outcome::Stored => counts.stored += 1,
            Outcome::Duplicate => counts.duplicate += 1,
            Outcome::Refused(code) => {
                counts.refused += 1;
                let _ = writeln!(log, "import: line {number}: refused {}", code.as_str());
            }
        }
        if counts.lines % PROGRESS_EVERY == 0 {
            let _ = writeln!(log, "import: at line {number}: {counts}");
        }
    }
}

/// Reads the next line of `input` into `line` without its `\n` (and a `\r` before it), keeping at
/// most [`MAX_BODY`] bytes: `Some(true)` for a line that fits, `Some(false)` for a longer one,
/// whose rest is read and dropped, `None` at the end of the input.
fn read_line(input: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Option<bool>> {
    line.clear();
    let mut fits = true;
    let mut any = false;
    loop {
        let buf = match input.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            break;
        }
        any = true;
        let (part, end) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..i], Some(i + 1)),
            None => (buf, None),
        };
        // One byte over the cap, for a `\r` that the end of the line removes.
        if fits && line.len() + part.len() <= MAX_BODY + 1 {
            line.extend_from_slice(part);
        } else {
            fits = false;
            line.clear();
        }
        let used = end.unwrap_or(buf.len());
        input.consume(used);
        if end.is_some() {
            break;
        }
    }
    if !any {
        return Ok(None);
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if !fits || line.len() > MAX_BODY {
        line.clear();
        return Ok(Some(false));
    }
    Ok(Some(true))
}

/// The outcome of one line of at most [`MAX_BODY`] bytes: refused `invalid_request` without a
/// request when it is not a `PutRequest`, else the daemon's answer, tried again after a transient
/// fault until [`Client::attempts`] tries are spent.
fn put_line(client: &Client, line: &[u8]) -> Result<Outcome, Fault> {
    if serde_json::from_slice::<c10::PutRequest>(line).is_err() {
        return Ok(Outcome::Refused(ErrorCode::InvalidRequest));
    }
    let mut pause = client.pause;
    let mut tries = 1;
    loop {
        match put(client, line) {
            Err(fault) if fault.transient && tries < client.attempts.max(1) => {
                thread::sleep(pause);
                pause = pause.saturating_mul(2);
                tries += 1;
            }
            Err(fault) => {
                return Err(Fault {
                    reason: format!("{} (after {tries} tries)", fault.reason),
                    ..fault
                });
            }
            Ok(outcome) => return Ok(outcome),
        }
    }
}

/// One `POST /v1/put` of `body` on its own connection.
fn put(client: &Client, body: &[u8]) -> Result<Outcome, Fault> {
    let socket: &Path = &client.socket;
    let at = |e: io::Error| Fault::transient(format!("{}: {e}", socket.display()));
    let mut stream = UnixStream::connect(socket).map_err(at)?;
    stream.set_write_timeout(Some(client.timeout)).map_err(at)?;
    stream.set_read_timeout(Some(client.timeout)).map_err(at)?;
    let head = format!(
        "POST /v1/put HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .map_err(at)?;
    let mut answer = Vec::new();
    (&mut stream)
        .take(MAX_ANSWER)
        .read_to_end(&mut answer)
        .map_err(at)?;
    outcome(&answer)
}

/// The outcome of the daemon's raw answer to a `put`.
fn outcome(answer: &[u8]) -> Result<Outcome, Fault> {
    if answer.is_empty() {
        return Err(Fault::transient("the daemon closed without an answer"));
    }
    let Some(head_end) = answer.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Err(Fault::transient("an answer without a whole head"));
    };
    let head = String::from_utf8_lossy(&answer[..head_end]);
    let body = &answer[head_end + 4..];
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| Fault::fatal(format!("a malformed status line: {head}")))?;
    if status == 200 {
        let put: c10::PutResponse = serde_json::from_slice(body)
            .map_err(|e| Fault::fatal(format!("a 200 answer that is not a PutResponse: {e}")))?;
        return Ok(match put.status {
            PutStatus::Stored => Outcome::Stored,
            PutStatus::Duplicate => Outcome::Duplicate,
        });
    }
    let error: c10::Error = serde_json::from_slice(body)
        .map_err(|e| Fault::fatal(format!("a {status} answer that is not a C10 Error: {e}")))?;
    match error.code {
        ErrorCode::InvalidRequest | ErrorCode::Conflict | ErrorCode::TooLarge => {
            Ok(Outcome::Refused(error.code))
        }
        ErrorCode::StoreFailed => Err(Fault::transient(format!("store_failed: {}", error.message))),
        code => Err(Fault::fatal(format!(
            "{}: {}",
            code.as_str(),
            error.message
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn lines(input: &[u8]) -> Vec<Option<(bool, Vec<u8>)>> {
        let mut reader = io::BufReader::with_capacity(16, input);
        let mut out = Vec::new();
        let mut line = Vec::new();
        loop {
            let read = read_line(&mut reader, &mut line).unwrap();
            out.push(read.map(|fits| (fits, line.clone())));
            if read.is_none() {
                return out;
            }
        }
    }

    #[test]
    fn lines_split_on_newline_with_a_last_line_without_one_and_crlf_trimmed() {
        let got = lines(b"one\r\ntwo\n\nlast");
        let want = [
            Some((true, b"one".to_vec())),
            Some((true, b"two".to_vec())),
            Some((true, b"".to_vec())),
            Some((true, b"last".to_vec())),
            None,
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn a_line_over_the_body_cap_is_dropped_whole_and_the_next_one_read() {
        let mut input = vec![b'x'; MAX_BODY + 1];
        input.extend_from_slice(b"\nnext\n");
        let mut exact = vec![b'y'; MAX_BODY];
        exact.extend_from_slice(b"\r\n");
        input.extend_from_slice(&exact);
        let got = lines(&input);
        assert_eq!(got[0], Some((false, Vec::new())));
        assert_eq!(got[1], Some((true, b"next".to_vec())));
        assert_eq!(got[2], Some((true, vec![b'y'; MAX_BODY])));
        assert_eq!(got[3], None);
    }

    #[test]
    fn answers_map_to_outcomes_and_faults() {
        let reply = |status: &str, body: &str| {
            format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let id = "0".repeat(64);
        let stored = reply("200 OK", &format!(r#"{{"id":"{id}","status":"stored"}}"#));
        assert_eq!(outcome(stored.as_bytes()).unwrap(), Outcome::Stored);
        let dup = reply(
            "200 OK",
            &format!(r#"{{"id":"{id}","status":"duplicate"}}"#),
        );
        assert_eq!(outcome(dup.as_bytes()).unwrap(), Outcome::Duplicate);
        for (status, code, want) in [
            (
                "400 Bad Request",
                "invalid_request",
                ErrorCode::InvalidRequest,
            ),
            ("409 Conflict", "conflict", ErrorCode::Conflict),
            ("413 Content Too Large", "too_large", ErrorCode::TooLarge),
        ] {
            let body = format!(r#"{{"code":"{code}","message":"m"}}"#);
            assert_eq!(
                outcome(reply(status, &body).as_bytes()).unwrap(),
                Outcome::Refused(want)
            );
        }
        let failed = reply(
            "500 Internal Server Error",
            r#"{"code":"store_failed","message":"disk"}"#,
        );
        assert!(outcome(failed.as_bytes()).unwrap_err().transient);
        let forbidden = reply("403 Forbidden", r#"{"code":"forbidden","message":"uid"}"#);
        assert!(!outcome(forbidden.as_bytes()).unwrap_err().transient);
        assert!(outcome(b"").unwrap_err().transient);
        assert!(
            !outcome(reply("200 OK", "{}").as_bytes())
                .unwrap_err()
                .transient
        );
    }

    #[test]
    fn lines_that_are_not_put_requests_are_refused_without_a_connection() {
        let client = Client::new(PathBuf::from("/nonexistent/ulei-memory-import.sock"));
        let mut input: &[u8] = b"not json\n{\"source\":\"notes\"}\n\n[]\n";
        let mut log = Vec::new();
        let counts = import(&mut input, 0, &client, &mut log).unwrap();
        assert_eq!(
            counts,
            Counts {
                lines: 4,
                stored: 0,
                duplicate: 0,
                refused: 4
            }
        );
        let log = String::from_utf8(log).unwrap();
        for n in 1..=4 {
            assert!(
                log.contains(&format!("import: line {n}: refused invalid_request\n")),
                "{log}"
            );
        }
    }

    #[test]
    fn skipped_lines_are_neither_read_as_requests_nor_counted() {
        let client = Client::new(PathBuf::from("/nonexistent/ulei-memory-import.sock"));
        let mut input: &[u8] = b"bad\nbad\nbad\n";
        let mut log = Vec::new();
        let counts = import(&mut input, 2, &client, &mut log).unwrap();
        assert_eq!((counts.lines, counts.refused), (1, 1));
        assert_eq!(
            String::from_utf8(log).unwrap(),
            "import: line 3: refused invalid_request\n"
        );
    }

    #[test]
    fn a_daemon_that_is_not_there_stops_the_import_after_its_tries_naming_the_line() {
        let client = Client {
            socket: PathBuf::from("/nonexistent/ulei-memory-import.sock"),
            timeout: Duration::from_secs(1),
            attempts: 3,
            pause: Duration::from_millis(1),
        };
        let good = r#"{"source":"notes","external_id":"n-1","kind":"text/plain","text":"t","bytes_b64":"dA=="}"#;
        let input = format!("bad\n{good}\n{good}\n");
        let mut log = Vec::new();
        let (counts, stopped) = import(&mut input.as_bytes(), 0, &client, &mut log).unwrap_err();
        assert_eq!((counts.lines, counts.refused), (1, 1));
        assert_eq!((stopped.line, stopped.done), (2, 1));
        assert!(
            stopped.reason.ends_with("(after 3 tries)"),
            "{}",
            stopped.reason
        );
        assert!(
            stopped.to_string().contains("resume with --skip 1"),
            "{stopped}"
        );
    }
}
