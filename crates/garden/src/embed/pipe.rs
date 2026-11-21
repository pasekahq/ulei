//! The pipe between `ulei-garden serve` and its embedder helper `ulei-garden-embed` (ADR-0047
//! sections 1 and 2), one call at a time. The helper's first stdout line is its [`Identity`] as
//! one JSON object, or `{"error": <reason>}` before it exits. A request is one JSON line
//! `{"kind": "query" | "passage", "texts": [...]}`; its answer is one JSON line
//! `{"count": n, "dim": d}` followed by `n x d` little-endian f32, or `{"error": <reason>}`.

use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{EmbedError, Embedder, ONNX_FILE, TOKENIZER_FILE, lock};
use crate::miss::lower_hex;

/// The loaded model as the helper's ready line names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The embedder's [`Embedder::model_id`]: the model and the settings that change its vectors.
    pub name: String,
    /// Lowercase hex SHA-256 of [`ONNX_FILE`] in the model directory.
    pub model_sha256: String,
    /// Lowercase hex SHA-256 of [`TOKENIZER_FILE`] in the model directory.
    pub tokenizer_sha256: String,
    /// Length of every vector, above 0.
    pub dim: usize,
}

impl Identity {
    /// The identity of `embedder` over the artifacts in `model_dir`.
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

    fn line(&self) -> String {
        json!({
            "name": self.name,
            "model_sha256": self.model_sha256,
            "tokenizer_sha256": self.tokenizer_sha256,
            "dim": self.dim,
        })
        .to_string()
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
                .filter(|d| *d > 0)?,
        })
    }
}

/// Lowercase hex SHA-256 of the file at `path`, read in chunks.
fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
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
/// `input` with `embedder` until `input` ends.
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
    writeln!(output, "{}", identity.line())?;
    output.flush()?;
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }
        match answer(embedder, identity.dim, &line) {
            Ok(vectors) => {
                let header = json!({"count": vectors.len(), "dim": identity.dim});
                writeln!(output, "{header}")?;
                for x in vectors.iter().flatten() {
                    output.write_all(&x.to_le_bytes())?;
                }
            }
            Err(reason) => writeln!(output, "{}", json!({ "error": reason }))?,
        }
        output.flush()?;
    }
}

/// The helper's side: the line that refuses the start with `reason`, in place of the ready line.
///
/// # Errors
///
/// Writing `output` fails.
pub fn refuse(mut output: impl Write, reason: &str) -> io::Result<()> {
    writeln!(output, "{}", json!({ "error": reason }))?;
    output.flush()
}

/// The vectors of one request line, `dim` long each, or the reason it is refused.
fn answer(embedder: &dyn Embedder, dim: usize, line: &str) -> Result<Vec<Vec<f32>>, String> {
    let request: Value = serde_json::from_str(line).map_err(|e| format!("request: {e}"))?;
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
            Self::Refused(reason) => f.write_str(reason),
            Self::Malformed => f.write_str("its first line is not a ready line"),
            Self::Read(e) => write!(f, "reading its ready line: {e}"),
        }
    }
}

impl std::error::Error for StartError {}

/// Longest wait for the answer of one call in the daemon; a helper that misses it is killed.
pub const CALL_DEADLINE: Duration = Duration::from_secs(5);

/// The helper process seen from the daemon: an [`Embedder`] whose calls go over its pipe, one at
/// a time, a waiting query before a waiting passage. A call that breaks the pipe or misses its
/// deadline kills and reaps the helper, and every later call fails; dropping this kills it too.
pub struct Helper {
    identity: Identity,
    model_id: String,
    deadline: Duration,
    child: Mutex<Child>,
    lane: Mutex<Lane>,
    freed: Condvar,
    broken: Mutex<Option<String>>,
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
    answers: Receiver<Result<Vec<Vec<f32>>, Fault>>,
}

/// One request line and the shape of the answer it expects.
struct Ask {
    line: String,
    count: usize,
    dim: usize,
}

/// A failed call: the helper's own refusal, after which the pipe stays in step, a fault of the
/// pipe, or a pipe already gone before the request reached the helper.
enum Fault {
    Refused(String),
    Broken(String),
    Undelivered(String),
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
        let (asks, asked) = mpsc::sync_channel(1);
        let (answer, answers) = mpsc::sync_channel(1);
        let pipe = thread::Builder::new()
            .name("garden-embed-pipe".into())
            .spawn(move || run_pipe(stdin, BufReader::new(stdout), &ready_tx, &asked, &answer));
        if let Err(e) = pipe {
            let _ = reap(&mut child);
            return Err(StartError::Read(e));
        }
        let line = match ready.recv_timeout(timeout) {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => return Err(stopped(&mut child, StartError::Read(e))),
            Err(RecvTimeoutError::Timeout) => {
                return Err(stopped(&mut child, StartError::Silent(timeout)));
            }
            Err(RecvTimeoutError::Disconnected) => {
                let e = io::Error::other("the reader of the ready line ended");
                return Err(stopped(&mut child, StartError::Read(e)));
            }
        };
        if line.is_empty() {
            return Err(StartError::Exited(reap(&mut child)));
        }
        let first: Option<Value> = serde_json::from_str(&line).ok();
        let refusal = first
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(Value::as_str);
        if let Some(reason) = refusal {
            return Err(stopped(&mut child, StartError::Refused(reason.to_owned())));
        }
        let Some(identity) = first.as_ref().and_then(Identity::parse) else {
            return Err(stopped(&mut child, StartError::Malformed));
        };
        Ok(Self {
            model_id: identity.model_id(),
            identity,
            deadline,
            child: Mutex::new(child),
            lane: Mutex::new(Lane {
                wire: Some(Wire { asks, answers }),
                queries: 0,
            }),
            freed: Condvar::new(),
            broken: Mutex::new(None),
        })
    }

    /// The identity of the ready line.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Why the helper serves no more calls: a broken pipe, a missed deadline or its exit; `None`
    /// while it serves. Its exit is looked for only while no call holds the pipe: a call in
    /// flight names the fault it meets itself.
    pub fn fault(&self) -> Option<String> {
        let lane = lock(&self.lane);
        self.polled(lane.wire.is_some())
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
        let wire = self.take(query);
        let result = match self.polled(true) {
            Some(why) => Err(EmbedError::Down(why)),
            None => match wire.exchange(kind, texts, self.identity.dim, self.deadline) {
                Ok(vectors) => Ok(vectors),
                Err(Fault::Refused(reason)) => Err(EmbedError::Run(reason)),
                Err(Fault::Broken(why)) => {
                    self.kill(&why);
                    Err(EmbedError::Run(why))
                }
                Err(Fault::Undelivered(why)) => {
                    self.kill(&why);
                    Err(EmbedError::Down(why))
                }
            },
        };
        lock(&self.lane).wire = Some(wire);
        self.freed.notify_all();
        result
    }

    /// Waits for the pipe; a query takes it before every waiting passage.
    fn take(&self, query: bool) -> Wire {
        let mut lane = lock(&self.lane);
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
            lane = self
                .freed
                .wait(lane)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn kill(&self, why: &str) {
        lock(&self.broken).get_or_insert_with(|| why.to_owned());
        let _ = reap(&mut lock(&self.child));
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = reap(&mut lock(&self.child));
    }
}

impl Wire {
    fn exchange(
        &self,
        kind: &str,
        texts: &[&str],
        dim: usize,
        deadline: Duration,
    ) -> Result<Vec<Vec<f32>>, Fault> {
        let broken = |e: &dyn fmt::Display| Fault::Broken(format!("helper pipe: {e}"));
        let undelivered = |e: &dyn fmt::Display| Fault::Undelivered(format!("helper pipe: {e}"));
        let mut line = json!({"kind": kind, "texts": texts}).to_string();
        line.push('\n');
        let ask = Ask {
            line,
            count: texts.len(),
            dim,
        };
        self.asks
            .send(ask)
            .map_err(|_| undelivered(&"its thread ended"))?;
        match self.answers.recv_timeout(deadline) {
            Ok(answer) => answer,
            Err(RecvTimeoutError::Timeout) => {
                Err(broken(&format_args!("no answer within {deadline:?}")))
            }
            Err(RecvTimeoutError::Disconnected) => Err(broken(&"its thread ended")),
        }
    }
}

/// The thread of the pipe: reads the ready line, then for each ask writes its line and reads
/// its answer, until the asks end or the pipe breaks.
fn run_pipe(
    mut stdin: ChildStdin,
    mut stdout: BufReader<ChildStdout>,
    ready: &SyncSender<io::Result<String>>,
    asked: &Receiver<Ask>,
    answer: &SyncSender<Result<Vec<Vec<f32>>, Fault>>,
) {
    let mut line = String::new();
    let first = stdout.read_line(&mut line).map(|_| line);
    let failed = first.is_err();
    if ready.send(first).is_err() || failed {
        return;
    }
    while let Ok(ask) = asked.recv() {
        let answered = round_trip(&mut stdin, &mut stdout, &ask);
        let broken = matches!(answered, Err(Fault::Broken(_) | Fault::Undelivered(_)));
        if answer.send(answered).is_err() || broken {
            return;
        }
    }
}

fn round_trip(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    ask: &Ask,
) -> Result<Vec<Vec<f32>>, Fault> {
    let broken = |e: &dyn fmt::Display| Fault::Broken(format!("helper pipe: {e}"));
    stdin
        .write_all(ask.line.as_bytes())
        .and_then(|()| stdin.flush())
        .map_err(|e| Fault::Undelivered(format!("helper pipe: {e}")))?;
    let mut header = String::new();
    match stdout.read_line(&mut header) {
        Ok(0) => return Err(broken(&"the helper closed it")),
        Ok(_) => {}
        Err(e) => return Err(broken(&e)),
    }
    let header: Value =
        serde_json::from_str(&header).map_err(|_| broken(&"malformed answer header"))?;
    if let Some(reason) = header.get("error").and_then(Value::as_str) {
        return Err(Fault::Refused(reason.to_owned()));
    }
    let (count, dim) = (ask.count, ask.dim);
    let got_count = header.get("count").and_then(Value::as_u64);
    let got_dim = header.get("dim").and_then(Value::as_u64);
    if got_count != u64::try_from(count).ok() || got_dim != u64::try_from(dim).ok() {
        return Err(broken(&format_args!(
            "answer of {got_count:?} vectors of {got_dim:?} for {count} texts of {dim}"
        )));
    }
    let mut bytes = vec![0u8; count * dim * 4];
    stdout.read_exact(&mut bytes).map_err(|e| broken(&e))?;
    let (floats, _) = bytes.as_chunks::<4>();
    Ok(floats
        .chunks_exact(dim)
        .map(|row| row.iter().map(|b| f32::from_le_bytes(*b)).collect())
        .collect())
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
