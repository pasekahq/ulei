//! `ulei-memory import`: puts a stream of C10 `PutRequest` JSON lines into the archive through
//! the daemon's socket, the C10 transport the connectors use; the archive file is opened only by
//! the daemon and `delete`. A line may name its original by `bytes_path`, a local file this client
//! reads, instead of `bytes_b64`. An item whose `put` body would pass the 1 MiB cap goes in parts
//! (`put_begin`, `put_part`, `put_end`, ADR-0074). One line at a time, never held whole: its
//! decoded `bytes_b64` goes to an unlinked temporary file once it passes 1 MiB, so memory stays
//! a few MiB whatever the size of the original.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::archive::{MAX_PART, MAX_UPLOAD_TEXT};
use crate::serve::MAX_BODY;
use crate::serve::base64::{self, Decoder};
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
/// Most bytes of a line's fields but `bytes_b64` together, as they stand in the line.
pub const MAX_FIELDS: usize = 4 * 1024 * 1024;
/// Most decoded bytes of a `bytes_b64` held in memory; more go to a temporary file.
const MAX_HELD: usize = MAX_BODY;
/// Bytes read from a file or from the input at a time.
const BLOCK: usize = 64 * 1024;

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

/// A refused line: its code and, for a refusal before any request, why.
#[derive(Debug)]
struct Refusal {
    code: ErrorCode,
    why: Option<String>,
}

impl Refusal {
    fn invalid(why: impl fmt::Display) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            why: Some(why.to_string()),
        }
    }

    const fn bare(code: ErrorCode) -> Self {
        Self { code, why: None }
    }
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
/// per refused line (its number and code, and why when it was refused before any request), one
/// per [`PROGRESS_EVERY`] lines, and none at the end: the caller writes the counts. Stops at the
/// first line whose transport still fails after [`Client::attempts`] tries, or at a failure to
/// read `input` or to write a temporary file.
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
    loop {
        let stop = |counts: Counts, number: u64, reason: String| {
            let stopped = Stopped {
                line: number,
                done: number - 1,
                reason,
            };
            Err((counts, stopped))
        };
        if number >= skip {
            match has_line(input) {
                Ok(true) => {}
                Ok(false) => return Ok(counts),
                Err(e) => return stop(counts, number + 1, format!("stdin: {e}")),
            }
        } else {
            match skip_line(input) {
                Ok(true) => {
                    number += 1;
                    continue;
                }
                Ok(false) => return Ok(counts),
                Err(e) => return stop(counts, number + 1, format!("stdin: {e}")),
            }
        }
        number += 1;
        let scanned = match scan_line(input) {
            Ok(scanned) => scanned,
            Err(Stop(reason)) => return stop(counts, number, reason),
        };
        let result = scanned.and_then(prepare);
        let outcome = match result {
            Ok(prepared) => match put_line(client, prepared) {
                Ok(Ok(put)) => Ok(put),
                Ok(Err(refusal)) => Err(refusal),
                Err(fault) => return stop(counts, number, fault.reason),
            },
            Err(refusal) => Err(refusal),
        };
        counts.lines += 1;
        match outcome {
            Ok(Outcome::Stored) => counts.stored += 1,
            Ok(Outcome::Duplicate) => counts.duplicate += 1,
            Ok(Outcome::Refused(code)) | Err(Refusal { code, why: None }) => {
                counts.refused += 1;
                let _ = writeln!(log, "import: line {number}: refused {}", code.as_str());
            }
            Err(Refusal {
                code,
                why: Some(why),
            }) => {
                counts.refused += 1;
                let _ = writeln!(
                    log,
                    "import: line {number}: refused {}: {why}",
                    code.as_str()
                );
            }
        }
        if counts.lines % PROGRESS_EVERY == 0 {
            let _ = writeln!(log, "import: at line {number}: {counts}");
        }
    }
}

/// A failure that stops the import: of the input or of a temporary file.
struct Stop(String);

/// Whether `input` has another line.
fn has_line(input: &mut impl BufRead) -> io::Result<bool> {
    loop {
        match input.fill_buf() {
            Ok(buf) => return Ok(!buf.is_empty()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Reads and drops the next line; `false` at the end of the input.
fn skip_line(input: &mut impl BufRead) -> io::Result<bool> {
    if !has_line(input)? {
        return Ok(false);
    }
    Bytes::new(input).finish()?;
    Ok(true)
}

/// The bytes of one line of the input, up to its `\n` or the end of the input.
struct Bytes<'a, R> {
    input: &'a mut R,
}

impl<'a, R: BufRead> Bytes<'a, R> {
    const fn new(input: &'a mut R) -> Self {
        Self { input }
    }

    /// The rest of the input's buffer up to the line's end, empty at the end of the line.
    fn window(&mut self) -> io::Result<&[u8]> {
        loop {
            match self.input.fill_buf() {
                Ok(_) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        // Filled above: this call only returns the buffer.
        let buf = self.input.fill_buf()?;
        let end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
        Ok(&buf[..end])
    }

    fn consume(&mut self, n: usize) {
        self.input.consume(n);
    }

    fn peek(&mut self) -> io::Result<Option<u8>> {
        Ok(self.window()?.first().copied())
    }

    fn next(&mut self) -> io::Result<Option<u8>> {
        let b = self.peek()?;
        if b.is_some() {
            self.consume(1);
        }
        Ok(b)
    }

    /// The next byte after JSON whitespace.
    fn next_token(&mut self) -> io::Result<Option<u8>> {
        loop {
            match self.next()? {
                Some(b' ' | b'\t' | b'\r') => {}
                other => return Ok(other),
            }
        }
    }

    /// Reads and drops the rest of the line with its `\n`.
    fn finish(mut self) -> io::Result<()> {
        loop {
            let (len, at_end) = {
                let buf = loop {
                    match self.input.fill_buf() {
                        Ok(buf) => break buf,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                };
                if buf.is_empty() {
                    return Ok(());
                }
                match buf.iter().position(|&b| b == b'\n') {
                    Some(i) => (i + 1, true),
                    None => (buf.len(), false),
                }
            };
            self.consume(len);
            if at_end {
                return Ok(());
            }
        }
    }
}

/// The fields of an import line but its `bytes_b64`: a C10 `PutRequest` without its bytes, and
/// the import's own `bytes_path` and `id`.
#[derive(Debug)]
struct Fields {
    request: c10::PutRequest,
    bytes_path: Option<PathBuf>,
    id: Option<String>,
}

impl Fields {
    /// The fields of the members `kept`, a JSON object, or `None` when they are not those of an
    /// import line.
    fn parse(kept: &[u8]) -> Option<Self> {
        let Value::Object(mut map) = serde_json::from_slice(kept).ok()? else {
            return None;
        };
        let mut take = |name: &str| match map.remove(name) {
            None => Some(None),
            Some(Value::String(s)) => Some(Some(s)),
            Some(_) => None,
        };
        let bytes_path = take("bytes_path")?.map(PathBuf::from);
        let id = take("id")?;
        if map.contains_key("bytes_b64") {
            return None;
        }
        map.insert("bytes_b64".to_owned(), Value::String(String::new()));
        let request = serde_json::from_value(Value::Object(map)).ok()?;
        Some(Self {
            request,
            bytes_path,
            id,
        })
    }
}

/// An original as the import holds it: in memory, or in an open file from its start.
enum Original {
    Memory(Vec<u8>),
    File(File),
}

/// A scanned line: its fields, and the decoded `bytes_b64` with their length and SHA-256.
struct Line {
    fields: Fields,
    decoded: Option<(Original, u64, String)>,
}

/// Reads the next line of `input` to its end, decoding its `bytes_b64` on the way; a line that
/// is no import line is a refusal, its rest read and dropped.
fn scan_line(input: &mut impl BufRead) -> Result<Result<Line, Refusal>, Stop> {
    let mut bytes = Bytes::new(input);
    let scanned = scan_object(&mut bytes);
    let rest = match &scanned {
        Ok(Ok(_)) => match bytes.next_token() {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Err(None),
            Err(e) => Err(Some(e)),
        },
        _ => Ok(()),
    };
    if let Err(e) = bytes.finish() {
        return Err(Stop(format!("stdin: {e}")));
    }
    match (scanned, rest) {
        (Err(Scan::Io(e)), _) | (_, Err(Some(e))) => Err(Stop(format!("stdin: {e}"))),
        (Err(Scan::Spool(e)), _) => Err(Stop(format!("temporary file: {e}"))),
        (Err(Scan::Refused(refusal)), _) => Ok(Err(refusal)),
        (Ok(_), Err(None)) => Ok(Err(Refusal::bare(ErrorCode::InvalidRequest))),
        (Ok(line), Ok(())) => Ok(line),
    }
}

/// Why a scan ended early.
enum Scan {
    Io(io::Error),
    Spool(io::Error),
    Refused(Refusal),
}

impl From<io::Error> for Scan {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

fn not_json() -> Scan {
    Scan::Refused(Refusal::bare(ErrorCode::InvalidRequest))
}

/// One JSON object: every member's raw text kept but `bytes_b64`'s, whose string is decoded as
/// it is read; the kept members are then parsed as [`Fields`].
fn scan_object<R: BufRead>(bytes: &mut Bytes<'_, R>) -> Result<Result<Line, Refusal>, Scan> {
    if bytes.next_token()? != Some(b'{') {
        return Err(not_json());
    }
    let mut kept = vec![b'{'];
    let mut decoded = None;
    let mut first = true;
    loop {
        match bytes.next_token()? {
            Some(b'}') if first => break,
            Some(b'"') => {}
            _ => return Err(not_json()),
        }
        let mut key = vec![b'"'];
        raw_string(bytes, &mut key, MAX_FIELDS)?;
        let name: String = serde_json::from_slice(&key).map_err(|_| not_json())?;
        if bytes.next_token()? != Some(b':') {
            return Err(not_json());
        }
        if name == "bytes_b64" {
            if decoded.is_some() || bytes.next_token()? != Some(b'"') {
                return Err(not_json());
            }
            decoded = Some(decode_string(bytes)?);
        } else {
            if kept.len() > 1 {
                kept.push(b',');
            }
            kept.extend_from_slice(&key);
            kept.push(b':');
            raw_value(bytes, &mut kept)?;
        }
        first = false;
        match bytes.next_token()? {
            Some(b',') => {}
            Some(b'}') => break,
            _ => return Err(not_json()),
        }
    }
    kept.push(b'}');
    let fields = Fields::parse(&kept).ok_or_else(not_json)?;
    Ok(Ok(Line { fields, decoded }))
}

/// Appends a JSON string's raw rest after its opening quote, the closing quote included, to `out`.
fn raw_string<R: BufRead>(
    bytes: &mut Bytes<'_, R>,
    out: &mut Vec<u8>,
    cap: usize,
) -> Result<(), Scan> {
    let mut escaped = false;
    loop {
        let Some(b) = bytes.next()? else {
            return Err(not_json());
        };
        if out.len() >= cap {
            return Err(Scan::Refused(Refusal::bare(ErrorCode::TooLarge)));
        }
        out.push(b);
        match (escaped, b) {
            (false, b'"') => return Ok(()),
            (false, b'\\') => escaped = true,
            _ => escaped = false,
        }
    }
}

/// Appends one JSON value's raw text to `out`, refusing the line when `out` passes
/// [`MAX_FIELDS`]; the value's grammar is left to the parse of [`Fields`].
fn raw_value<R: BufRead>(bytes: &mut Bytes<'_, R>, out: &mut Vec<u8>) -> Result<(), Scan> {
    let mut depth = 0usize;
    loop {
        let Some(b) = bytes.peek()? else {
            return if depth == 0 { Ok(()) } else { Err(not_json()) };
        };
        if depth == 0 && matches!(b, b',' | b'}' | b']') {
            return Ok(());
        }
        bytes.consume(1);
        out.push(b);
        match b {
            b'"' => raw_string(bytes, out, MAX_FIELDS)?,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            _ => {}
        }
        if out.len() > MAX_FIELDS {
            return Err(Scan::Refused(Refusal::bare(ErrorCode::TooLarge)));
        }
    }
}

/// Decodes the rest of a `bytes_b64` string after its opening quote: held in memory up to
/// [`MAX_HELD`] bytes, then in a temporary file; with the length and SHA-256.
fn decode_string<R: BufRead>(bytes: &mut Bytes<'_, R>) -> Result<(Original, u64, String), Scan> {
    let bad = || Scan::Refused(Refusal::bare(ErrorCode::InvalidRequest));
    let mut decoder = Decoder::default();
    let mut held = Vec::new();
    let mut file: Option<File> = None;
    let mut hasher = Sha256::new();
    let mut len = 0u64;
    let mut out = Vec::with_capacity(BLOCK);
    loop {
        let window = bytes.window()?;
        if window.is_empty() {
            return Err(bad());
        }
        let quote = window.iter().position(|&b| b == b'"' || b == b'\\');
        let text = &window[..quote.unwrap_or(window.len())];
        let used = text.len();
        out.clear();
        decoder.feed(text, &mut out).ok_or_else(bad)?;
        let stop = quote.map(|i| window[i]);
        bytes.consume(used);
        match stop {
            Some(b'"') => bytes.consume(1),
            Some(_) => {
                // JSON may write `/` as `\/`; no other escape is in the alphabet.
                bytes.consume(1);
                if bytes.next()? != Some(b'/') {
                    return Err(bad());
                }
                decoder.feed(b"/", &mut out).ok_or_else(bad)?;
            }
            None => {}
        }
        hasher.update(&out);
        len += out.len() as u64;
        match &mut file {
            Some(f) => f.write_all(&out).map_err(Scan::Spool)?,
            None if held.len() + out.len() > MAX_HELD => {
                let mut f = spool().map_err(Scan::Spool)?;
                f.write_all(&held)
                    .and_then(|()| f.write_all(&out))
                    .map_err(Scan::Spool)?;
                held = Vec::new();
                file = Some(f);
            }
            None => held.extend_from_slice(&out),
        }
        if stop == Some(b'"') {
            break;
        }
    }
    decoder.finish().ok_or_else(bad)?;
    let original = match file {
        Some(mut f) => {
            f.seek(SeekFrom::Start(0)).map_err(Scan::Spool)?;
            Original::File(f)
        }
        None => Original::Memory(held),
    };
    Ok((original, len, hex(&hasher.finalize())))
}

/// A new temporary file of this user, unlinked at once, so it goes when it is closed.
fn spool() -> io::Result<File> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir();
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("ulei-memory-import.{}.{n}", std::process::id()));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                fs::remove_file(&path)?;
                return Ok(file);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
}

/// A line ready to put: its fields, its original, and the original's length and id.
struct Prepared {
    fields: Fields,
    original: Original,
    len: u64,
    id: String,
}

/// Checks the line's original: exactly one of `bytes_b64` and `bytes_path`, a readable regular
/// file for the path, and its SHA-256 equal to `id` when the line names one.
fn prepare(line: Line) -> Result<Prepared, Refusal> {
    let Line {
        mut fields,
        decoded,
    } = line;
    let (original, len, id) = match (decoded, fields.bytes_path.take()) {
        (Some(decoded), None) => decoded,
        (None, Some(path)) => from_path(&path)?,
        _ => {
            return Err(Refusal::invalid(
                "a line gives exactly one of bytes_b64 and bytes_path",
            ));
        }
    };
    if let Some(want) = &fields.id
        && *want != id
    {
        return Err(Refusal::invalid(format!(
            "the original's SHA-256 is {id}, not the line's id {want}"
        )));
    }
    Ok(Prepared {
        fields,
        original,
        len,
        id,
    })
}

/// The file at `path`, opened once, with its length and SHA-256 read from it.
fn from_path(path: &Path) -> Result<(Original, u64, String), Refusal> {
    let at = |e: io::Error| Refusal::invalid(format!("bytes_path {}: {e}", path.display()));
    let mut file = File::open(path).map_err(at)?;
    if !file.metadata().map_err(at)?.is_file() {
        return Err(Refusal::invalid(format!(
            "bytes_path {} is not a regular file",
            path.display()
        )));
    }
    let mut hasher = Sha256::new();
    let mut len = 0u64;
    let mut block = vec![0u8; BLOCK];
    loop {
        match file.read(&mut block) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&block[..n]);
                len += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(at(e)),
        }
    }
    file.seek(SeekFrom::Start(0)).map_err(at)?;
    Ok((Original::File(file), len, hex(&hasher.finalize())))
}

/// The outcome of one prepared line, tried again after a transient fault until
/// [`Client::attempts`] tries are spent: one `put` when its body fits [`MAX_BODY`], else an
/// upload in parts, which a retry begins anew.
fn put_line(client: &Client, mut line: Prepared) -> Result<Result<Outcome, Refusal>, Fault> {
    let mut request = line.fields.request.clone();
    let shell = serde_json::to_vec(&request).map_err(Fault::fatal)?.len();
    let whole = line.len.div_ceil(3).saturating_mul(4);
    let single = u64::try_from(shell)
        .unwrap_or(u64::MAX)
        .saturating_add(whole)
        <= MAX_BODY as u64;
    let body = if single {
        let bytes = match &mut line.original {
            Original::Memory(bytes) => std::mem::take(bytes),
            Original::File(file) => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)
                    .map_err(|e| Fault::fatal(format!("original: {e}")))?;
                bytes
            }
        };
        request.bytes_b64 = base64::encode(&bytes);
        Some(serde_json::to_vec(&request).map_err(Fault::fatal)?)
    } else {
        None
    };
    let mut pause = client.pause;
    let mut tries = 1;
    loop {
        let result = match &body {
            Some(body) => put(client, body).map(Ok),
            None => upload(client, &mut line),
        };
        match result {
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

/// One upload of `line` in parts from the start of its original.
fn upload(client: &Client, line: &mut Prepared) -> Result<Result<Outcome, Refusal>, Fault> {
    let fields = &line.fields.request;
    let mut text = head(&fields.text, MAX_UPLOAD_TEXT);
    let opening = loop {
        let body = serde_json::to_vec(&c10::PutBeginRequest {
            source: fields.source.clone(),
            external_id: fields.external_id.clone(),
            who: fields.who.clone(),
            at: fields.at,
            kind: fields.kind.clone(),
            text: text.to_owned(),
            len: line.len,
            id: line.id.clone(),
        })
        .map_err(Fault::fatal)?;
        // A text of many escaped characters may take up to six times its length.
        if body.len() <= MAX_BODY {
            break body;
        }
        text = head(text, text.len() / 2);
    };
    let begun: c10::PutBeginResponse = match call(client, "put_begin", &opening, false)? {
        Ok(begun) => output(serde_json::from_value(begun), "put_begin")?,
        Err(code) => return Ok(Err(Refusal::bare(code))),
    };
    let upload = match (begun.upload, begun.status) {
        (_, Some(status)) => return Ok(Ok(outcome_of(status))),
        (Some(upload), None) => upload,
        (None, None) => return Err(Fault::fatal("a put_begin answer without upload or status")),
    };
    let mut part = vec![0u8; MAX_PART];
    let mut index = 0u64;
    match &mut line.original {
        Original::File(file) => {
            file.seek(SeekFrom::Start(0))
                .map_err(|e| Fault::fatal(format!("original: {e}")))?;
            loop {
                let n =
                    fill(file, &mut part).map_err(|e| Fault::fatal(format!("original: {e}")))?;
                if n == 0 {
                    break;
                }
                if let Err(code) = send_part(client, upload, index, &part[..n])? {
                    return Ok(Err(Refusal::bare(code)));
                }
                index += 1;
            }
        }
        Original::Memory(bytes) => {
            for chunk in bytes.chunks(MAX_PART) {
                if let Err(code) = send_part(client, upload, index, chunk)? {
                    return Ok(Err(Refusal::bare(code)));
                }
                index += 1;
            }
        }
    }
    let end = serde_json::to_vec(&c10::PutEndRequest { upload }).map_err(Fault::fatal)?;
    match call(client, "put_end", &end, true)? {
        Ok(put) => Ok(Ok(outcome_of(
            output::<c10::PutResponse>(serde_json::from_value(put), "put_end")?.status,
        ))),
        Err(code) => Ok(Err(Refusal::bare(code))),
    }
}

fn send_part(
    client: &Client,
    upload: u64,
    index: u64,
    bytes: &[u8],
) -> Result<Result<(), ErrorCode>, Fault> {
    let body = serde_json::to_vec(&c10::PutPartRequest {
        upload,
        index,
        bytes_b64: base64::encode(bytes),
    })
    .map_err(Fault::fatal)?;
    match call(client, "put_part", &body, true)? {
        Ok(part) => {
            output::<c10::PutPartResponse>(serde_json::from_value(part), "put_part").map(|_| Ok(()))
        }
        Err(code) => Ok(Err(code)),
    }
}

/// Reads until `buf` is full or the file ends; the bytes read.
fn fill(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

const fn outcome_of(status: PutStatus) -> Outcome {
    match status {
        PutStatus::Stored => Outcome::Stored,
        PutStatus::Duplicate => Outcome::Duplicate,
    }
}

/// The longest head of `text` of at most `max` bytes that ends at a character boundary.
fn head(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// One `POST /v1/put` of `body` on its own connection.
fn put(client: &Client, body: &[u8]) -> Result<Outcome, Fault> {
    outcome(&exchange(client, "put", body)?)
}

/// One `POST /v1/<method>` of `body` and its output, or the code that refuses the line. Within
/// an upload (`in_upload`) a `not_found` means the upload expired: a transient fault, so the
/// line is tried again from `put_begin`.
fn call(
    client: &Client,
    method: &str,
    body: &[u8],
    in_upload: bool,
) -> Result<Result<Value, ErrorCode>, Fault> {
    let answer = exchange(client, method, body)?;
    match parse_answer(&answer)? {
        Ok(out) => Ok(Ok(out)),
        Err(error) if in_upload && error.code == ErrorCode::NotFound => {
            Err(Fault::transient(format!("not_found: {}", error.message)))
        }
        Err(error) => refusal_of(&error).map(Err),
    }
}

/// The raw answer to one `POST /v1/<method>` of `body` on its own connection.
fn exchange(client: &Client, method: &str, body: &[u8]) -> Result<Vec<u8>, Fault> {
    let socket: &Path = &client.socket;
    let at = |e: io::Error| Fault::transient(format!("{}: {e}", socket.display()));
    let mut stream = UnixStream::connect(socket).map_err(at)?;
    stream.set_write_timeout(Some(client.timeout)).map_err(at)?;
    stream.set_read_timeout(Some(client.timeout)).map_err(at)?;
    let head = format!(
        "POST /v1/{method} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
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
    Ok(answer)
}

/// The outcome of the daemon's raw answer to a `put`.
fn outcome(answer: &[u8]) -> Result<Outcome, Fault> {
    match parse_answer(answer)? {
        Ok(put) => Ok(outcome_of(
            output::<c10::PutResponse>(serde_json::from_value(put), "put")?.status,
        )),
        Err(error) => refusal_of(&error).map(Outcome::Refused),
    }
}

/// The code of a C10 error that refuses the line, or the fault it is.
fn refusal_of(error: &c10::Error) -> Result<ErrorCode, Fault> {
    match error.code {
        ErrorCode::InvalidRequest | ErrorCode::Conflict | ErrorCode::TooLarge => Ok(error.code),
        ErrorCode::StoreFailed => Err(Fault::transient(format!("store_failed: {}", error.message))),
        code => Err(Fault::fatal(format!(
            "{}: {}",
            code.as_str(),
            error.message
        ))),
    }
}

/// `value`, an answer's JSON read as the output of `method`.
fn output<T>(value: Result<T, serde_json::Error>, method: &str) -> Result<T, Fault> {
    value.map_err(|e| {
        Fault::fatal(format!(
            "a 200 answer that is not the output of {method}: {e}"
        ))
    })
}

/// The JSON of a `200` answer, or the C10 error of any other.
fn parse_answer(answer: &[u8]) -> Result<Result<Value, c10::Error>, Fault> {
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
        return serde_json::from_slice(body)
            .map(Ok)
            .map_err(|e| Fault::fatal(format!("a 200 answer that is not JSON: {e}")));
    }
    serde_json::from_slice(body)
        .map(Err)
        .map_err(|e| Fault::fatal(format!("a {status} answer that is not a C10 Error: {e}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Scans every line of `input` through a 16-byte buffer.
    fn scan_all(input: &[u8]) -> Vec<Result<Line, Refusal>> {
        let mut reader = io::BufReader::with_capacity(16, input);
        let mut out = Vec::new();
        while has_line(&mut reader).unwrap() {
            out.push(scan_line(&mut reader).ok().unwrap());
        }
        out
    }

    fn sha(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    fn held(line: Line) -> (Vec<u8>, u64, String, bool) {
        let (original, len, id) = line.decoded.unwrap();
        match original {
            Original::Memory(bytes) => (bytes, len, id, false),
            Original::File(mut file) => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).unwrap();
                (bytes, len, id, true)
            }
        }
    }

    #[test]
    fn lines_are_scanned_with_members_in_any_order_and_an_escaped_slash() {
        let input = concat!(
            "{\"bytes_b64\":\"P\\/8=\", \"source\":\"notes\",\"external_id\":\"n-1\",",
            "\"kind\":\"text/plain\",\"text\":\"a \\\"quoted\\\" text\",\"at\":1.5}\r\n",
            "\n",
            "{\"source\":\"notes\",\"external_id\":\"n-2\",\"kind\":\"k\",\"text\":\"\",",
            "\"bytes_path\":\"/nonexistent/x\",\"id\":\"00\"}"
        );
        let mut got = scan_all(input.as_bytes()).into_iter();
        let first = got.next().unwrap().unwrap();
        assert_eq!(first.fields.request.text, "a \"quoted\" text");
        assert_eq!(first.fields.request.at, Some(1.5));
        let (bytes, len, id, file) = held(first);
        assert_eq!((bytes.as_slice(), len, file), (&[0x3f, 0xff][..], 2, false));
        assert_eq!(id, sha(&[0x3f, 0xff]));
        assert_eq!(
            got.next().unwrap().err().unwrap().code,
            ErrorCode::InvalidRequest
        );
        let third = got.next().unwrap().unwrap();
        assert!(third.decoded.is_none());
        assert_eq!(
            third.fields.bytes_path.as_deref(),
            Some(Path::new("/nonexistent/x"))
        );
        assert_eq!(third.fields.id.as_deref(), Some("00"));
        assert!(got.next().is_none());
    }

    #[test]
    fn a_long_bytes_b64_goes_to_a_temporary_file_and_the_next_line_is_read() {
        let original: Vec<u8> = (0..=3 * MAX_HELD)
            .map(|i| u8::try_from(i * 7 % 251).unwrap())
            .collect();
        let line = format!(
            "{{\"source\":\"notes\",\"external_id\":\"big\",\"kind\":\"k\",\"bytes_b64\":\"{}\",\"text\":\"t\"}}\nnot json\n",
            base64::encode(&original)
        );
        let mut reader = io::BufReader::new(line.as_bytes());
        let first = scan_line(&mut reader).ok().unwrap().unwrap();
        let (bytes, len, id, file) = held(first);
        assert!(file);
        assert_eq!(len, original.len() as u64);
        assert!(bytes == original);
        assert_eq!(id, sha(&original));
        let second = scan_line(&mut reader).ok().unwrap();
        assert_eq!(second.err().unwrap().code, ErrorCode::InvalidRequest);
        assert!(!has_line(&mut reader).unwrap());
    }

    #[test]
    fn malformed_lines_are_refused_and_read_to_their_end() {
        let fields = r#""source":"notes","external_id":"n","kind":"k","text":"t""#;
        let cases = [
            format!("{{{fields},\"bytes_b64\":\"AA==\",\"bytes_b64\":\"AA==\"}}"),
            format!("{{{fields},\"bytes_b64\":\"AA=\"}}"),
            format!("{{{fields},\"bytes_b64\":\"AA==\"}} trailing"),
            format!("{{{fields},\"bytes_b64\":\"AA==\",\"extra\":1}}"),
            format!("{{{fields},\"bytes_b64\":\"A\\nA=\"}}"),
            format!("{{{fields},\"bytes_b64\":3}}"),
            format!("{{{fields},\"id\":7,\"bytes_b64\":\"AA==\"}}"),
            format!("{{{fields},\"bytes_b64\":\"AA==\""),
            "[]".to_owned(),
        ];
        let input = cases.join("\n") + "\n";
        let got = scan_all(input.as_bytes());
        assert_eq!(got.len(), cases.len());
        for (case, got) in cases.iter().zip(got) {
            assert_eq!(
                got.err().map(|r| r.code),
                Some(ErrorCode::InvalidRequest),
                "{case}"
            );
        }
        let long = format!(
            "{{{fields},\"who\":\"{}\",\"bytes_b64\":\"AA==\"}}\n",
            "w".repeat(MAX_FIELDS)
        );
        let got = scan_all(long.as_bytes());
        assert_eq!(got.len(), 1);
        assert_eq!(
            got.into_iter().next().unwrap().err().map(|r| r.code),
            Some(ErrorCode::TooLarge)
        );
    }

    #[test]
    fn a_line_gives_one_original_whose_sha256_matches_its_id() {
        let fields = r#""source":"notes","external_id":"n","kind":"k","text":"t""#;
        let dir = std::env::temp_dir().join(format!("ulei-memory-prep-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("original.bin");
        fs::write(&path, b"file bytes").unwrap();
        let id = sha(b"file bytes");
        let lines = [
            format!(
                "{{{fields},\"bytes_path\":\"{}\",\"id\":\"{id}\"}}",
                path.display()
            ),
            format!(
                "{{{fields},\"bytes_path\":\"{}\",\"id\":\"{}\"}}",
                path.display(),
                "0".repeat(64)
            ),
            format!(
                "{{{fields},\"bytes_path\":\"{}\",\"bytes_b64\":\"AA==\"}}",
                path.display()
            ),
            format!("{{{fields}}}"),
            format!(
                "{{{fields},\"bytes_path\":\"{}\"}}",
                dir.join("missing").display()
            ),
            format!("{{{fields},\"bytes_path\":\"{}\"}}", dir.display()),
            format!(
                "{{{fields},\"bytes_b64\":\"AA==\",\"id\":\"{}\"}}",
                sha(&[0])
            ),
        ];
        let got: Vec<_> = scan_all(lines.join("\n").as_bytes())
            .into_iter()
            .map(|line| line.and_then(prepare))
            .collect();
        let _ = fs::remove_dir_all(&dir);
        let mut got = got.into_iter();
        let first = got.next().unwrap().unwrap();
        assert_eq!((first.len, first.id.as_str()), (10, id.as_str()));
        for _ in 0..5 {
            let refusal = got.next().unwrap().err().unwrap();
            assert_eq!(refusal.code, ErrorCode::InvalidRequest);
            assert!(refusal.why.is_some());
        }
        assert_eq!(got.next().unwrap().ok().unwrap().len, 1);
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
