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
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{EmbedError, Embedder, ONNX_FILE, TOKENIZER_FILE};
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

/// The helper process seen from the daemon: an [`Embedder`] whose calls go over its pipe, one at
/// a time. The helper is killed when this is dropped.
pub struct Helper {
    identity: Identity,
    model_id: String,
    pipe: Mutex<Pipe>,
}

struct Pipe {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Why the pipe is out of step; every later call fails with it.
    broken: Option<String>,
}

impl Drop for Pipe {
    fn drop(&mut self) {
        let _ = reap(&mut self.child);
    }
}

/// A failed call: the helper's own refusal, after which the pipe stays in step, or a fault of
/// the pipe.
enum Fault {
    Refused(String),
    Broken(String),
}

impl Helper {
    /// Spawns `command` with its stdin and stdout piped and its stderr inherited, and waits at
    /// most `timeout` for its ready line.
    ///
    /// # Errors
    ///
    /// [`StartError`]; the helper is killed and reaped first.
    pub fn start(mut command: Command, timeout: Duration) -> Result<Self, StartError> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(StartError::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = reap(&mut child);
            return Err(StartError::Read(io::Error::other("no pipe to the helper")));
        };
        // A blocking read has no deadline, so the first line is read on a thread of its own;
        // killing the helper on a timeout ends that read.
        let (tx, rx) = mpsc::sync_channel(1);
        let reader = thread::Builder::new()
            .name("garden-embed-ready".into())
            .spawn(move || {
                let mut stdout = BufReader::new(stdout);
                let mut line = String::new();
                let read = stdout.read_line(&mut line).map(|_| line);
                let _ = tx.send((read, stdout));
            });
        if let Err(e) = reader {
            let _ = reap(&mut child);
            return Err(StartError::Read(e));
        }
        let (line, stdout) = match rx.recv_timeout(timeout) {
            Ok((Ok(line), stdout)) => (line, stdout),
            Ok((Err(e), _)) => return Err(stopped(&mut child, StartError::Read(e))),
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
            pipe: Mutex::new(Pipe {
                child,
                stdin,
                stdout,
                broken: None,
            }),
        })
    }

    /// The identity of the ready line.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    fn call(&self, kind: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut pipe = self.pipe.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(why) = &pipe.broken {
            return Err(EmbedError::Run(why.clone()));
        }
        match pipe.exchange(kind, texts, self.identity.dim) {
            Ok(vectors) => Ok(vectors),
            Err(Fault::Refused(reason)) => Err(EmbedError::Run(reason)),
            Err(Fault::Broken(why)) => {
                pipe.broken = Some(why.clone());
                Err(EmbedError::Run(why))
            }
        }
    }
}

impl Pipe {
    fn exchange(&mut self, kind: &str, texts: &[&str], dim: usize) -> Result<Vec<Vec<f32>>, Fault> {
        let broken = |e: &dyn fmt::Display| Fault::Broken(format!("helper pipe: {e}"));
        let mut request = json!({"kind": kind, "texts": texts}).to_string();
        request.push('\n');
        self.stdin
            .write_all(request.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| broken(&e))?;
        let mut header = String::new();
        match self.stdout.read_line(&mut header) {
            Ok(0) => return Err(broken(&"the helper closed it")),
            Ok(_) => {}
            Err(e) => return Err(broken(&e)),
        }
        let header: Value =
            serde_json::from_str(&header).map_err(|_| broken(&"malformed answer header"))?;
        if let Some(reason) = header.get("error").and_then(Value::as_str) {
            return Err(Fault::Refused(reason.to_owned()));
        }
        let count = header.get("count").and_then(Value::as_u64);
        let got = header.get("dim").and_then(Value::as_u64);
        if count != u64::try_from(texts.len()).ok() || got != u64::try_from(dim).ok() {
            return Err(broken(&format_args!(
                "answer of {count:?} vectors of {got:?} for {} texts of {dim}",
                texts.len()
            )));
        }
        let mut bytes = vec![0u8; texts.len() * dim * 4];
        self.stdout.read_exact(&mut bytes).map_err(|e| broken(&e))?;
        let (floats, _) = bytes.as_chunks::<4>();
        Ok(floats
            .chunks_exact(dim)
            .map(|row| row.iter().map(|b| f32::from_le_bytes(*b)).collect())
            .collect())
    }
}

impl Embedder for Helper {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.identity.dim
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.call("query", &[text])?
            .pop()
            .ok_or_else(|| EmbedError::Run("no vector".into()))
    }

    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.call("passage", texts)
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
