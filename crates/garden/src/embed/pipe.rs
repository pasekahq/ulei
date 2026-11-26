//! The pipe between `ulei-garden serve` and its embedder helper `ulei-garden-embed` (ADR-0047
//! sections 1 and 2). The helper's first stdout line is its [`Identity`] as one JSON object, or
//! `{"error": <reason>}` before it exits, with `"class": "corrupted"` when the reason is a
//! corrupted model file ([`CORRUPTED`], ADR-0087 open question 15). A request is one JSON line
//! `{"kind": "query" | "passage", "texts": [...]}`; its answer is one JSON line
//! `{"count": n, "dim": d}` followed by `n x d` little-endian f32, or `{"error": <reason>}`.
//!
//! **Lanes** (ADR-0087 section 9). A helper whose ready line carries `"lanes": 2` ([`LANES`])
//! answers queries and passages on two lanes at once: a request that carries `"id": <u64>` runs
//! on the query lane for `"kind": "query"` and on the indexing lane for `"kind": "passage"`, and
//! its answer header carries the same `"id"`. Each lane answers its requests in order; the two
//! lanes' answers interleave, each answer written whole. A request without an `"id"` is answered
//! in place as above, so such a helper still serves a daemon that knows no lanes. The daemon uses
//! the lanes only with a helper that announces them and sends at most one request per lane at a
//! time; with any other helper it keeps one call at a time, a waiting query before a waiting
//! passage.

use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{EmbedError, Embedder, ONNX_FILE, TOKENIZER_FILE, lock};
use crate::miss::lower_hex;

/// The value of `"lanes"` in the ready line of a helper that serves the query lane and the
/// indexing lane at once.
pub const LANES: u64 = 2;

/// How much higher the nice value of the helper's indexing lane is than the helper's own (at
/// most 19), ADR-0087 section 9. A query on the same core then gets about 90 % of it (CFS
/// weights 1024 against 110), while a passage next to other work at the helper's nice value still
/// gets about a tenth of a core: 19 higher would leave it under 2 %, and a long bge-m3 passage
/// would miss [`PASSAGE_DEADLINE`].
pub const INDEX_NICE: i32 = 10;

/// The longest request line the helper reads, its newline included. Larger than any line the
/// daemon sends: one text per request, a query under the 1 MiB request body or a passage of an
/// entity's name and a fact's text, each under its own 1 MiB body, and JSON escaping never makes
/// a text longer than in the body it came in.
pub const MAX_REQUEST: usize = 8 * 1024 * 1024;

/// The longest vector a helper may declare in its ready line: four times the width of the
/// largest model in use (bge-m3, 1024), so the daemon's buffer for an answer stays bounded. A
/// longer one is a malformed ready line.
pub const MAX_DIM: usize = 4096;

/// The loaded model as the helper's ready line names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The embedder's [`Embedder::model_id`]: the model and the settings that change its vectors.
    pub name: String,
    /// Lowercase hex SHA-256 of [`ONNX_FILE`] in the model directory, or the content digest
    /// sealed in the encoder engine's weight file ([`super::ENGINE_FILE`], [`super::digest`]).
    pub model_sha256: String,
    /// Lowercase hex SHA-256 of [`TOKENIZER_FILE`] in the model directory, or again the
    /// engine's weight file's content digest: the file holds its tokenizer.
    pub tokenizer_sha256: String,
    /// Length of every vector, above 0 and at most [`MAX_DIM`].
    pub dim: usize,
}

impl Identity {
    /// The identity of `embedder` over the ONNX artifacts in `model_dir`.
    ///
    /// # Errors
    ///
    /// An artifact cannot be read; the message names its file.
    pub fn of(embedder: &dyn Embedder, model_dir: &Path) -> Result<Self, String> {
        let digest =
            |file: &str| sha256_file(&model_dir.join(file)).map_err(|e| format!("{file}: {e}"));
        Ok(Self {
            name: embedder.model_id().to_owned(),
            model_sha256: digest(ONNX_FILE)?,
            tokenizer_sha256: digest(TOKENIZER_FILE)?,
            dim: embedder.dim(),
        })
    }

    /// The model id stored vectors carry: the name and both digests, so a changed artifact
    /// leaves every vector of the old one stale.
    pub fn model_id(&self) -> String {
        format!(
            "{}/{}/{}",
            self.name, self.model_sha256, self.tokenizer_sha256
        )
    }

    fn json(&self) -> Value {
        json!({
            "name": self.name,
            "model_sha256": self.model_sha256,
            "tokenizer_sha256": self.tokenizer_sha256,
            "dim": self.dim,
        })
    }

    fn parse(v: &Value) -> Option<Self> {
        let text = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
        let digest = |key: &str| {
            text(key).filter(|d| {
                d.len() == 64
                    && d.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        };
        Some(Self {
            name: text("name").filter(|n| !n.is_empty())?,
            model_sha256: digest("model_sha256")?,
            tokenizer_sha256: digest("tokenizer_sha256")?,
            dim: v
                .get("dim")
                .and_then(Value::as_u64)
                .and_then(|d| usize::try_from(d).ok())
                .filter(|d| (1..=MAX_DIM).contains(d))?,
        })
    }
}

/// Lowercase hex SHA-256 of the file at `path`, read in chunks.
fn sha256_file(path: &Path) -> io::Result<String> {
    sha256_of(File::open(path)?)
}

/// Lowercase hex SHA-256 of everything `file` reads to its end, read in chunks.
///
/// # Errors
///
/// Reading `file` fails.
pub fn sha256_of(mut file: impl Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => return Ok(lower_hex(&hasher.finalize())),
            Ok(n) => hasher.update(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// The helper's side: writes the ready line of `identity`, then answers every request line of
/// `input` with `embedder`, one at a time, until `input` ends. The ready line announces no
/// lanes.
///
/// # Errors
///
/// Reading `input` or writing `output` fails.
pub fn serve(
    embedder: &dyn Embedder,
    identity: &Identity,
    mut input: impl BufRead,
    mut output: impl Write,
) -> io::Result<()> {
    writeln!(output, "{}", identity.json())?;
    output.flush()?;
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let answer = request(&line).and_then(|r| answer(embedder, identity.dim, &r));
        write_answer(&mut output, None, identity.dim, &answer)?;
    }
}

/// The helper's side with lanes (see the module docs): writes the ready line of `identity` with
/// `"lanes": 2`, then reads request lines of `input` until it ends. A query, or a request
/// without an id, is answered on the calling thread; a passage with an id goes to the indexing
/// lane, a thread of its own that first calls `lower` (to lower its own priority) and then
/// embeds the passages in the order they came. Both write whole answers to `output` in turn.
/// When `input` ends, the passages already read are answered before this returns.
///
/// # Errors
///
/// Reading `input` or writing `output` fails, or the indexing lane cannot be started.
pub fn serve_lanes<E: Embedder + Sync + ?Sized>(
    embedder: &E,
    identity: &Identity,
    mut input: impl BufRead,
    output: impl Write + Send,
    lower: impl FnOnce() + Send,
) -> io::Result<()> {
    let output = Mutex::new(output);
    {
        let mut ready = identity.json();
        ready["lanes"] = LANES.into();
        let mut out = lock(&output);
        writeln!(out, "{ready}")?;
        out.flush()?;
    }
    let dim = identity.dim;
    let (passages, asked) = mpsc::channel::<(u64, Value)>();
    thread::scope(|scope| {
        let output = &output;
        let indexing = thread::Builder::new()
            .name("garden-embed-index".into())
            .spawn_scoped(scope, move || -> io::Result<()> {
                lower();
                for (id, request) in asked {
                    let answer = answer(embedder, dim, &request);
                    write_answer(&mut *lock(output), Some(id), dim, &answer)?;
                }
                Ok(())
            })?;
        let read = (|| -> io::Result<()> {
            let mut line = String::new();
            loop {
                line.clear();
                if input.read_line(&mut line)? == 0 {
                    return Ok(());
                }
                let request = match request(&line) {
                    Ok(request) => request,
                    Err(reason) => {
                        write_answer(&mut *lock(output), None, dim, &Err(reason))?;
                        continue;
                    }
                };
                let id = request.get("id").and_then(Value::as_u64);
                match id {
                    Some(id) if request.get("kind").and_then(Value::as_str) == Some("passage") => {
                        passages
                            .send((id, request))
                            .map_err(|_| io::Error::other("the indexing lane ended"))?;
                    }
                    _ => {
                        let answer = answer(embedder, dim, &request);
                        write_answer(&mut *lock(output), id, dim, &answer)?;
                    }
                }
            }
        })();
        drop(passages);
        let indexed = indexing
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("the indexing lane panicked")));
        read.and(indexed)
    })
}

/// Lowers the calling thread's priority by [`INDEX_NICE`] (its nice value raised by that much,
/// at most to 19), for the helper's indexing lane; raising one's own nice value needs no
/// privilege. On Linux the nice value of `getpriority` and `setpriority(PRIO_PROCESS, 0)` is the
/// calling thread's own, so the other threads of the helper keep theirs; elsewhere it would be
/// the whole process's, so nothing is done there.
///
/// # Errors
///
/// The system refuses the change.
pub fn lower_this_thread() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let own = rustix::process::getpriority_process(None)?;
        rustix::process::setpriority_process(None, own.saturating_add(INDEX_NICE).min(19))?;
    }
    Ok(())
}

/// Writes one answer, its header carrying `id` when there is one, and flushes.
fn write_answer(
    output: &mut impl Write,
    id: Option<u64>,
    dim: usize,
    answer: &Result<Vec<Vec<f32>>, String>,
) -> io::Result<()> {
    let mut header = match answer {
        Ok(vectors) => json!({"count": vectors.len(), "dim": dim}),
        Err(reason) => json!({ "error": reason }),
    };
    if let Some(id) = id {
        header["id"] = id.into();
    }
    writeln!(output, "{header}")?;
    if let Ok(vectors) = answer {
        for x in vectors.iter().flatten() {
            output.write_all(&x.to_le_bytes())?;
        }
    }
    output.flush()
}

/// The `class` of a refusal whose reason is a corrupted model file: the bytes of the encoder
/// engine's weight file are not the ones its converter wrote (ADR-0087 E6c). The daemon then
/// starts lexical-only instead of refusing to start (ADR-0087 open question 15).
pub const CORRUPTED: &str = "corrupted";

/// The helper's side: the line that refuses the start with `reason`, in place of the ready line;
/// `corrupted` adds the class [`CORRUPTED`].
///
/// # Errors
///
/// Writing `output` fails.
pub fn refuse(mut output: impl Write, reason: &str, corrupted: bool) -> io::Result<()> {
    let line = if corrupted {
        json!({ "error": reason, "class": CORRUPTED })
    } else {
        json!({ "error": reason })
    };
    writeln!(output, "{line}")?;
    output.flush()
}

/// The request of one line, or why it is refused.
fn request(line: &str) -> Result<Value, String> {
    serde_json::from_str(line).map_err(|e| format!("request: {e}"))
}

/// The vectors of one request, `dim` long each, or the reason it is refused.
fn answer(
    embedder: &(impl Embedder + ?Sized),
    dim: usize,
    request: &Value,
) -> Result<Vec<Vec<f32>>, String> {
    let texts: Vec<&str> = request
        .get("texts")
        .and_then(Value::as_array)
        .and_then(|texts| texts.iter().map(Value::as_str).collect())
        .ok_or("request: texts must be a list of strings")?;
    let vectors = match request.get("kind").and_then(Value::as_str) {
        Some("query") => texts.iter().map(|t| embedder.embed_query(t)).collect(),
        Some("passage") => embedder.embed_passages(&texts),
        _ => return Err("request: kind must be \"query\" or \"passage\"".into()),
    }
    .map_err(|e| e.to_string())?;
    if vectors.len() != texts.len() || vectors.iter().any(|v| v.len() != dim) {
        return Err(format!(
            "the embedder returned {} vectors for {} texts, or one not {dim} long",
            vectors.len(),
            texts.len()
        ));
    }
    Ok(vectors)
}

/// Why the helper did not get ready.
#[derive(Debug)]
pub enum StartError {
    /// The helper could not be spawned.
    Spawn(io::Error),
    /// The helper closed its stdout, and exited with this status, before a ready line.
    Exited(io::Result<ExitStatus>),
    /// No ready line within this time.
    Silent(Duration),
    /// The helper refused to start for this reason.
    Refused(String),
    /// The helper refused to start because its model file is corrupted ([`CORRUPTED`]), for
    /// this reason.
    Corrupt(String),
    /// The first line is neither a ready line nor a refusal.
    Malformed,
    /// Reading the first line failed.
    Read(io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "cannot start: {e}"),
            Self::Exited(Ok(status)) => write!(f, "exited before its ready line ({status})"),
            Self::Exited(Err(e)) => write!(f, "exited before its ready line ({e})"),
            Self::Silent(timeout) => write!(f, "no ready line within {timeout:?}"),
            Self::Refused(reason) | Self::Corrupt(reason) => f.write_str(reason),
            Self::Malformed => f.write_str("its first line is not a ready line"),
            Self::Read(e) => write!(f, "reading its ready line: {e}"),
        }
    }
}

impl std::error::Error for StartError {}

/// Longest wait for the answer of one call in the daemon; a helper that misses it is killed.
/// With lanes it bounds the query lane's calls.
pub const CALL_DEADLINE: Duration = Duration::from_secs(5);

/// The longest line the daemon reads from its helper: a ready line or an answer header is a
/// short JSON object. A helper writing without a newline is refused at this length instead of
/// growing the daemon's memory until the call's deadline (ADR-0087 E12a, F2).
const MAX_LINE: u64 = 64 * 1024;

/// `read_line` of at most [`MAX_LINE`] bytes: the bytes read, 0 at the end of the stream.
///
/// # Errors
///
/// Reading fails, the line is longer than [`MAX_LINE`] or not UTF-8.
fn read_line_bounded(reader: &mut impl BufRead, line: &mut String) -> io::Result<usize> {
    let mut bytes = Vec::new();
    let n = reader
        .by_ref()
        .take(MAX_LINE)
        .read_until(b'\n', &mut bytes)?;
    if n as u64 == MAX_LINE && bytes.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a line of the helper longer than {MAX_LINE} bytes"),
        ));
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push_str(text);
    Ok(n)
}

/// How long a call whose pipe broke waits for the helper to end by itself before the daemon
/// kills it: long enough for a process killed from outside to become a zombie.
const EXIT_GRACE: Duration = Duration::from_millis(200);

/// Whether `status` is a SIGKILL: never sent by the helper to itself on a bad input, so one
/// the daemon did not send came from outside (the OOM killer, an operator).
fn killed_from_outside(status: ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(9)
}

/// Longest wait for the answer of a passage on the indexing lane, which runs at a lower
/// priority and may wait for the CPU while other work has it (ADR-0087 section 9); a hung
/// helper is still found within [`CALL_DEADLINE`] by the next query.
pub const PASSAGE_DEADLINE: Duration = Duration::from_secs(300);

/// The helper process seen from the daemon: an [`Embedder`] whose calls go over its pipe. A
/// helper that announces lanes takes a query while a passage is in flight, one call per lane at
/// a time; any other takes one call at a time, a waiting query before a waiting passage. A call
/// that breaks the pipe or misses its deadline kills and reaps the helper, so a call in flight
/// on the other lane fails too, and every later call fails; dropping this kills it too.
pub struct Helper {
    identity: Identity,
    model_id: String,
    deadline: Duration,
    passage_deadline: Duration,
    child: Mutex<Child>,
    link: Link,
    broken: Mutex<Option<String>>,
}

/// How calls reach the helper.
enum Link {
    /// One call at a time over the thread of the pipe.
    Single { lane: Mutex<Lane>, freed: Condvar },
    /// The query lane and the indexing lane at once.
    Lanes(Lanes),
}

/// The pipe and the queries waiting for it; `wire` is `None` while a call holds the pipe.
struct Lane {
    wire: Option<Wire>,
    queries: usize,
}

/// The caller's end of the thread that owns the helper's stdin and stdout, so that a blocked
/// write or read never outlasts the deadline of a call.
struct Wire {
    asks: SyncSender<Ask>,
    answers: Receiver<Answer>,
}

/// One request line and the shape of the answer it expects.
struct Ask {
    line: String,
    count: usize,
    dim: usize,
}

/// The vectors of a call, or why it failed.
type Answer = Result<Vec<Vec<f32>>, Fault>;

/// A failed call: the helper's own refusal, after which the pipe stays in step, a fault of the
/// pipe, a fault of the pipe while a call was in flight on the other lane too (either text may
/// have caused it), or a pipe already gone before the request reached the helper.
enum Fault {
    Refused(String),
    Broken(String),
    Shared(String),
    Undelivered(String),
}

/// The query lane (0) and the indexing lane (1) over one pipe: a writer thread that owns the
/// helper's stdin and a reader thread that owns its stdout and hands each answer to the lane
/// whose request carries its id.
struct Lanes {
    lines: Sender<(usize, String)>,
    ends: [Mutex<Receiver<Answer>>; 2],
    shared: Arc<Mutex<Pending>>,
    next_id: AtomicU64,
    busy: AtomicUsize,
}

/// The request in flight on each lane and, once the pipe is out of use, why.
struct Pending {
    calls: [Option<Call>; 2],
    answers: [Sender<Answer>; 2],
    gone: Option<String>,
}

struct Call {
    id: u64,
    count: usize,
}

impl Pending {
    /// Fails the call in flight on `lane`, if any, with `fault`.
    fn fail(&mut self, lane: usize, fault: Fault) {
        if self.calls[lane].take().is_some() {
            let _ = self.answers[lane].send(Err(fault));
        }
    }

    /// Ends the pipe for `why`: every call in flight fails as broken, later ones undelivered.
    /// `answered` counts the calls in flight when the fault came that already took their answer
    /// (a garbled one); with two calls in flight in all, each fails as [`Fault::Shared`].
    fn end(&mut self, why: &str, answered: usize) {
        self.gone.get_or_insert_with(|| why.to_owned());
        let shared = answered + self.calls.iter().flatten().count() > 1;
        for lane in 0..2 {
            let why = why.to_owned();
            let fault = if shared {
                Fault::Shared(why)
            } else {
                Fault::Broken(why)
            };
            self.fail(lane, fault);
        }
    }
}

impl Helper {
    /// Spawns `command` with its stdin and stdout piped and its stderr inherited, waits at most
    /// `timeout` for its ready line and gives each later call `deadline`.
    ///
    /// # Errors
    ///
    /// [`StartError`]; the helper is killed and reaped first.
    pub fn start(
        mut command: Command,
        timeout: Duration,
        deadline: Duration,
    ) -> Result<Self, StartError> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(StartError::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = reap(&mut child);
            return Err(StartError::Read(io::Error::other("no pipe to the helper")));
        };
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let first = thread::Builder::new()
            .name("garden-embed-ready".into())
            .spawn(move || {
                let mut stdout = BufReader::new(stdout);
                let mut line = String::new();
                let read = read_line_bounded(&mut stdout, &mut line).map(|_| line);
                let _ = ready_tx.send((read, stdout));
            });
        if let Err(e) = first {
            let _ = reap(&mut child);
            return Err(StartError::Read(e));
        }
        let (first_line, stdout) = match ready.recv_timeout(timeout) {
            Ok((Ok(first_line), stdout)) => (first_line, stdout),
            Ok((Err(e), _)) => return Err(stopped(&mut child, StartError::Read(e))),
            Err(RecvTimeoutError::Timeout) => {
                return Err(stopped(&mut child, StartError::Silent(timeout)));
            }
            Err(RecvTimeoutError::Disconnected) => {
                let e = io::Error::other("the reader of the ready line ended");
                return Err(stopped(&mut child, StartError::Read(e)));
            }
        };
        if first_line.is_empty() {
            return Err(StartError::Exited(reap(&mut child)));
        }
        let first: Option<Value> = serde_json::from_str(&first_line).ok();
        let refusal = first
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(Value::as_str);
        if let Some(reason) = refusal {
            let reason = reason.to_owned();
            let corrupted = first
                .as_ref()
                .and_then(|v| v.get("class"))
                .and_then(Value::as_str)
                == Some(CORRUPTED);
            let e = if corrupted {
                StartError::Corrupt(reason)
            } else {
                StartError::Refused(reason)
            };
            return Err(stopped(&mut child, e));
        }
        let Some(identity) = first.as_ref().and_then(Identity::parse) else {
            return Err(stopped(&mut child, StartError::Malformed));
        };
        let lanes = first
            .as_ref()
            .and_then(|v| v.get("lanes"))
            .and_then(Value::as_u64)
            == Some(LANES);
        let link = if lanes {
            Lanes::start(stdin, stdout, identity.dim).map(Link::Lanes)
        } else {
            Link::single(stdin, stdout)
        };
        let link = match link {
            Ok(link) => link,
            Err(e) => return Err(stopped(&mut child, StartError::Read(e))),
        };
        Ok(Self {
            model_id: identity.model_id(),
            identity,
            deadline,
            passage_deadline: deadline,
            child: Mutex::new(child),
            link,
            broken: Mutex::new(None),
        })
    }

    /// Gives each passage on the indexing lane `deadline` instead of the deadline of
    /// [`Helper::start`]; without lanes every call keeps that one.
    #[must_use]
    pub fn with_passage_deadline(mut self, deadline: Duration) -> Self {
        self.passage_deadline = deadline;
        self
    }

    /// The identity of the ready line.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Whether the helper announced lanes: a query does not wait for a passage in flight.
    pub fn has_lanes(&self) -> bool {
        matches!(self.link, Link::Lanes(_))
    }

    /// Why the helper serves no more calls: a broken pipe, a missed deadline or its exit; `None`
    /// while it serves. Its exit is looked for only while no call is in flight: a call in flight
    /// names the fault it meets itself.
    pub fn fault(&self) -> Option<String> {
        let idle = match &self.link {
            Link::Single { lane, .. } => lock(lane).wire.is_some(),
            Link::Lanes(lanes) => lanes.busy.load(Ordering::SeqCst) == 0,
        };
        self.polled(idle)
    }

    /// The fault, after looking for the helper's exit when `poll`.
    fn polled(&self, poll: bool) -> Option<String> {
        let mut broken = lock(&self.broken);
        if poll
            && broken.is_none()
            && let Ok(Some(status)) = lock(&self.child).try_wait()
        {
            *broken = Some(format!("the helper exited ({status})"));
        }
        broken.clone()
    }

    fn call(&self, kind: &str, texts: &[&str], query: bool) -> Result<Vec<Vec<f32>>, EmbedError> {
        let dim = self.identity.dim;
        let answered = match &self.link {
            Link::Single { lane, freed } => {
                let wire = take(lane, freed, query);
                let answered = match self.polled(true) {
                    Some(why) => Err(EmbedError::Down(why)),
                    None => Ok(wire.exchange(kind, texts, dim, self.deadline)),
                };
                self.mark_broken(&answered);
                lock(lane).wire = Some(wire);
                freed.notify_all();
                answered
            }
            Link::Lanes(lanes) => {
                let (lane, deadline) = if query {
                    (0, self.deadline)
                } else {
                    (1, self.passage_deadline)
                };
                let end = lock(&lanes.ends[lane]);
                lanes.busy.fetch_add(1, Ordering::SeqCst);
                let answered = match self.polled(true) {
                    Some(why) => Err(EmbedError::Down(why)),
                    None => Ok(lanes.exchange(&end, lane, kind, texts, deadline)),
                };
                self.mark_broken(&answered);
                lanes.busy.fetch_sub(1, Ordering::SeqCst);
                answered
            }
        };
        match answered? {
            Ok(vectors) => Ok(vectors),
            Err(Fault::Refused(reason)) => Err(EmbedError::Run(reason)),
            Err(Fault::Broken(why)) => Err(self.broke(why, false)),
            // The pipe failed with a query in flight too: either text may have caused it, so a
            // passage is not charged with it (review E13r, finding 4).
            Err(Fault::Shared(why)) => Err(self.broke(why, !query)),
            Err(Fault::Undelivered(why)) => {
                self.kill(&why);
                Err(EmbedError::Down(why))
            }
        }
    }

    /// The error of a call whose pipe broke for `why`, after the helper is killed and reaped:
    /// down for a helper killed with SIGKILL by someone else (the OOM killer, an operator) while the call
    /// was in flight, which did not fail on this text, so the worker does not count it toward
    /// `PASSAGE_TRIES` (ADR-0087 E12a, F3); down when `ambiguous`; else a crash of its own, a
    /// garbled answer or a missed deadline, the text's failure.
    fn broke(&self, why: String, ambiguous: bool) -> EmbedError {
        if let Some(status) = self.exited_within(EXIT_GRACE)
            && killed_from_outside(status)
        {
            let why = format!("the helper was killed ({status}): {why}");
            *lock(&self.broken) = Some(why.clone());
            self.kill(&why);
            return EmbedError::Down(why);
        }
        self.kill(&why);
        if ambiguous {
            EmbedError::Down(why)
        } else {
            EmbedError::Run(why)
        }
    }

    /// Marks the helper broken when `answered` is a fault of the pipe, before the caller hands
    /// the pipe or its lane on and before it waits for how the helper ended: a late answer of a
    /// call that missed its deadline may still arrive on the pipe, and the next caller would take
    /// it as its own (review E13r, finding 1). Every later call is then down.
    fn mark_broken(&self, answered: &Result<Answer, EmbedError>) {
        if let Ok(Err(Fault::Broken(why) | Fault::Shared(why) | Fault::Undelivered(why))) = answered
        {
            lock(&self.broken).get_or_insert_with(|| why.clone());
        }
    }

    /// The helper's exit status if it has ended by itself or ends within `grace` (its pipe
    /// closes a moment before it becomes a zombie); `None` while it still runs.
    fn exited_within(&self, grace: Duration) -> Option<ExitStatus> {
        let since = std::time::Instant::now();
        loop {
            if let Ok(Some(status)) = lock(&self.child).try_wait() {
                return Some(status);
            }
            if since.elapsed() >= grace {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn kill(&self, why: &str) {
        lock(&self.broken).get_or_insert_with(|| why.to_owned());
        let _ = reap(&mut lock(&self.child));
    }
}

/// Waits for the single pipe; a query takes it before every waiting passage.
fn take(lane: &Mutex<Lane>, freed: &Condvar, query: bool) -> Wire {
    let mut lane = lock(lane);
    if query {
        lane.queries += 1;
    }
    loop {
        if (query || lane.queries == 0)
            && let Some(wire) = lane.wire.take()
        {
            if query {
                lane.queries -= 1;
            }
            return wire;
        }
        lane = freed.wait(lane).unwrap_or_else(PoisonError::into_inner);
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = reap(&mut lock(&self.child));
    }
}

impl Link {
    /// One call at a time over a thread that owns both ends of the pipe.
    fn single(stdin: ChildStdin, stdout: BufReader<ChildStdout>) -> io::Result<Self> {
        let (asks, asked) = mpsc::sync_channel(1);
        let (answer, answers) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("garden-embed-pipe".into())
            .spawn(move || run_pipe(stdin, stdout, &asked, &answer))?;
        Ok(Self::Single {
            lane: Mutex::new(Lane {
                wire: Some(Wire { asks, answers }),
                queries: 0,
            }),
            freed: Condvar::new(),
        })
    }
}

fn request_line(kind: &str, texts: &[&str], id: Option<u64>) -> String {
    let mut request = json!({"kind": kind, "texts": texts});
    if let Some(id) = id {
        request["id"] = id.into();
    }
    let mut line = request.to_string();
    line.push('\n');
    line
}

/// Waits at most `deadline` for the answer on `answers`.
fn awaited(answers: &Receiver<Answer>, deadline: Duration) -> Answer {
    let broken = |e: &dyn fmt::Display| Fault::Broken(format!("helper pipe: {e}"));
    match answers.recv_timeout(deadline) {
        Ok(answer) => answer,
        Err(RecvTimeoutError::Timeout) => {
            Err(broken(&format_args!("no answer within {deadline:?}")))
        }
        Err(RecvTimeoutError::Disconnected) => Err(broken(&"its thread ended")),
    }
}

impl Wire {
    fn exchange(&self, kind: &str, texts: &[&str], dim: usize, deadline: Duration) -> Answer {
        let ask = Ask {
            line: request_line(kind, texts, None),
            count: texts.len(),
            dim,
        };
        self.asks
            .send(ask)
            .map_err(|_| Fault::Undelivered("helper pipe: its thread ended".to_owned()))?;
        awaited(&self.answers, deadline)
    }
}

/// The thread of the single pipe: for each ask writes its line and reads its answer, until the
/// asks end or the pipe breaks.
fn run_pipe(
    mut stdin: ChildStdin,
    mut stdout: BufReader<ChildStdout>,
    asked: &Receiver<Ask>,
    answer: &SyncSender<Answer>,
) {
    while let Ok(ask) = asked.recv() {
        let answered = round_trip(&mut stdin, &mut stdout, &ask);
        let broken = matches!(answered, Err(Fault::Broken(_) | Fault::Undelivered(_)));
        if answer.send(answered).is_err() || broken {
            return;
        }
    }
}

fn round_trip(stdin: &mut ChildStdin, stdout: &mut BufReader<ChildStdout>, ask: &Ask) -> Answer {
    stdin
        .write_all(ask.line.as_bytes())
        .and_then(|()| stdin.flush())
        .map_err(|e| Fault::Undelivered(format!("helper pipe: {e}")))?;
    let header = read_header(stdout).map_err(|e| Fault::Broken(format!("helper pipe: {e}")))?;
    if let Some(reason) = header.get("error").and_then(Value::as_str) {
        return Err(Fault::Refused(reason.to_owned()));
    }
    read_vectors(stdout, &header, ask.count, ask.dim)
        .map_err(|e| Fault::Broken(format!("helper pipe: {e}")))
}

/// The next answer header.
fn read_header(stdout: &mut impl BufRead) -> Result<Value, String> {
    let mut header = String::new();
    match read_line_bounded(stdout, &mut header) {
        Ok(0) => return Err("the helper closed it".into()),
        Ok(_) => {}
        Err(e) => return Err(e.to_string()),
    }
    serde_json::from_str(&header).map_err(|_| "malformed answer header".into())
}

/// The `count` vectors of `dim` after `header`, which must name both.
fn read_vectors(
    stdout: &mut impl Read,
    header: &Value,
    count: usize,
    dim: usize,
) -> Result<Vec<Vec<f32>>, String> {
    let got_count = header.get("count").and_then(Value::as_u64);
    let got_dim = header.get("dim").and_then(Value::as_u64);
    if got_count != u64::try_from(count).ok() || got_dim != u64::try_from(dim).ok() {
        return Err(format!(
            "answer of {got_count:?} vectors of {got_dim:?} for {count} texts of {dim}"
        ));
    }
    let size = count
        .checked_mul(dim)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| format!("answer of {count} vectors of {dim} overflows"))?;
    let mut bytes = vec![0u8; size];
    stdout.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    let (floats, _) = bytes.as_chunks::<4>();
    Ok(floats
        .chunks_exact(dim.max(1))
        .map(|row| row.iter().map(|b| f32::from_le_bytes(*b)).collect())
        .collect())
}

impl Lanes {
    fn start(stdin: ChildStdin, stdout: BufReader<ChildStdout>, dim: usize) -> io::Result<Self> {
        let (query_tx, query_rx) = mpsc::channel();
        let (passage_tx, passage_rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Pending {
            calls: [None, None],
            answers: [query_tx, passage_tx],
            gone: None,
        }));
        let (lines, written) = mpsc::channel::<(usize, String)>();
        let pending = Arc::clone(&shared);
        thread::Builder::new()
            .name("garden-embed-write".into())
            .spawn(move || write_lanes(stdin, &written, &pending))?;
        let pending = Arc::clone(&shared);
        // On a failure here the writer ends with the dropped sender.
        thread::Builder::new()
            .name("garden-embed-read".into())
            .spawn(move || read_lanes(stdout, dim, &pending))?;
        Ok(Self {
            lines,
            ends: [Mutex::new(query_rx), Mutex::new(passage_rx)],
            shared,
            next_id: AtomicU64::new(1),
            busy: AtomicUsize::new(0),
        })
    }

    /// One call on `lane`, whose receiver `end` the caller holds.
    fn exchange(
        &self,
        end: &Receiver<Answer>,
        lane: usize,
        kind: &str,
        texts: &[&str],
        deadline: Duration,
    ) -> Answer {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut pending = lock(&self.shared);
            if let Some(why) = &pending.gone {
                return Err(Fault::Undelivered(why.clone()));
            }
            pending.calls[lane] = Some(Call {
                id,
                count: texts.len(),
            });
        }
        let request = request_line(kind, texts, Some(id));
        if self.lines.send((lane, request)).is_err() {
            let why = "helper pipe: its thread ended".to_owned();
            lock(&self.shared).calls[lane] = None;
            return Err(Fault::Undelivered(why));
        }
        awaited(end, deadline)
    }
}

/// The writer of the lanes: writes each request line; a failed write fails its call as
/// undelivered and ends the pipe.
fn write_lanes(
    mut stdin: ChildStdin,
    written: &Receiver<(usize, String)>,
    pending: &Mutex<Pending>,
) {
    while let Ok((lane, line)) = written.recv() {
        if let Err(e) = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
        {
            let why = format!("helper pipe: {e}");
            let mut pending = lock(pending);
            pending.fail(lane, Fault::Undelivered(why.clone()));
            pending.gone.get_or_insert(why);
            return;
        }
    }
}

/// The reader of the lanes: hands each answer to the lane whose call carries its id. An answer
/// that names no call in flight, or of another shape than its call, or a closed or failed pipe
/// ends the pipe: every call in flight fails.
fn read_lanes(mut stdout: BufReader<ChildStdout>, dim: usize, pending: &Mutex<Pending>) {
    let why = loop {
        let header = match read_header(&mut stdout) {
            Ok(header) => header,
            Err(e) => break e,
        };
        let id = header.get("id").and_then(Value::as_u64);
        let found = {
            let mut pending = lock(pending);
            let lane = (0..2).find(|&l| {
                pending.calls[l]
                    .as_ref()
                    .is_some_and(|call| Some(call.id) == id)
            });
            lane.and_then(|l| pending.calls[l].take().map(|call| (l, call)))
        };
        let Some((lane, call)) = found else {
            break format!("an answer for no call in flight (id {id:?})");
        };
        let answer = if let Some(reason) = header.get("error").and_then(Value::as_str) {
            Err(Fault::Refused(reason.to_owned()))
        } else {
            match read_vectors(&mut stdout, &header, call.count, dim) {
                Ok(vectors) => Ok(vectors),
                Err(e) => {
                    let why = format!("helper pipe: {e}");
                    let mut pending = lock(pending);
                    let _ = pending.answers[lane].send(Err(Fault::Broken(why.clone())));
                    pending.end(&why, 1);
                    return;
                }
            }
        };
        let _ = lock(pending).answers[lane].send(answer);
    };
    lock(pending).end(&format!("helper pipe: {why}"), 0);
}

impl Embedder for Helper {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.identity.dim
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.call("query", &[text], true)?
            .pop()
            .ok_or_else(|| EmbedError::Run("no vector".into()))
    }

    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.call("passage", texts, false)
    }
}

/// Kills and reaps `child`; an exited child keeps the status it exited with.
fn reap(child: &mut Child) -> io::Result<ExitStatus> {
    let _ = child.kill();
    child.wait()
}

fn stopped(child: &mut Child, e: StartError) -> StartError {
    let _ = reap(child);
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header whose shape overflows the answer's size in bytes is refused, never allocated or
    /// wrapped to fewer vectors than asked (review E13r, finding 8).
    #[test]
    fn an_answer_whose_size_overflows_is_refused() {
        let dim = usize::MAX / 2;
        let header = json!({"count": 2, "dim": dim});
        let got = read_vectors(&mut io::empty(), &header, 2, dim);
        assert!(got.is_err(), "{got:?}");
    }

    fn fake_identity() -> (crate::embed::HashEmbedder, Identity) {
        let embedder = crate::embed::HashEmbedder::new(4);
        let identity = Identity {
            name: embedder.model_id().to_owned(),
            model_sha256: "a".repeat(64),
            tokenizer_sha256: "a".repeat(64),
            dim: 4,
        };
        (embedder, identity)
    }

    /// A query line of `len` bytes, its newline included.
    fn query_line(len: usize) -> Vec<u8> {
        let head = br#"{"kind":"query","texts":[""#;
        let tail = b"\"]}\n";
        let mut line = head.to_vec();
        line.resize(len - tail.len(), b'a');
        line.extend_from_slice(tail);
        line
    }

    /// The answer headers of a helper's `output` after its ready line, without the vectors.
    fn headers(output: &[u8], dim: usize) -> Vec<Value> {
        let mut rest = output;
        let mut headers = Vec::new();
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            let header: Value = serde_json::from_slice(&rest[..end]).unwrap();
            let count = header
                .get("count")
                .and_then(Value::as_u64)
                .map_or(0, |c| usize::try_from(c).unwrap());
            rest = &rest[end + 1 + count * dim * 4..];
            headers.push(header);
        }
        headers
    }

    /// The helper bounds a request line at [`MAX_REQUEST`] bytes instead of reading an endless
    /// line into memory (review E13r, F6 of the engine review): a longer one is refused with the
    /// protocol's error answer, its bytes skipped, and the next request is answered in step.
    #[test]
    fn serve_refuses_a_request_line_over_the_bound_and_answers_the_next_one() {
        let (embedder, identity) = fake_identity();
        let mut input = query_line(MAX_REQUEST);
        input.extend(query_line(MAX_REQUEST + 1));
        input.extend(query_line(64));
        let mut output = Vec::new();
        serve(&embedder, &identity, &input[..], &mut output).unwrap();
        let mut lines = headers(&output, 4).into_iter();
        assert_eq!(lines.next().unwrap()["dim"], 4, "the ready line");
        assert_eq!(lines.next().unwrap()["count"], 1, "a line of the bound is served");
        let refused = lines.next().unwrap();
        assert_eq!(
            refused["error"].as_str(),
            Some(format!("request: a line longer than {MAX_REQUEST} bytes").as_str())
        );
        assert_eq!(lines.next().unwrap()["count"], 1, "the next one in step");
        assert!(lines.next().is_none());
    }

    /// With lanes an answer must carry its request's id, which an over-long line cannot give:
    /// the helper ends with a clear error instead of answering out of step.
    #[test]
    fn serve_lanes_ends_on_a_request_line_over_the_bound() {
        let (embedder, identity) = fake_identity();
        let mut input = query_line(64);
        input.extend(query_line(MAX_REQUEST + 1));
        input.extend(query_line(64));
        let mut output = Vec::new();
        let served = serve_lanes(&embedder, &identity, &input[..], &mut output, || {});
        let e = served.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            e.to_string(),
            format!("a request line longer than {MAX_REQUEST} bytes")
        );
        let lines = headers(&output, 4);
        assert_eq!(lines.len(), 2, "the ready line and the first answer only");
    }
}
