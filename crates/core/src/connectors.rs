//! The source connectors' runner (ADR-0056 sections 3 to 5.1): discovery of the connected
//! connectors, one run of a stdio connector in its transient unit over pipes into C10 `put`,
//! its cursor, its run record, `reset`, and the launcher inside the unit.

mod cursor;
mod manifest;
mod put;
mod stdio;
mod unit;

use std::fmt::Write as _;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, BufReader, Read as _, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ulei_contracts::config::{Config, Value};
use ulei_contracts::generated::c10::PutRequest;
use ulei_contracts::generated::c11::{
    Grants, GrantsLandlock, GrantsNetwork, Item, Request, RunOutcome, RunRecord, Transport,
};

pub use cursor::reset;
pub use manifest::{Connector, Discovery, discover, enabled, is_name};
pub use unit::{REFUSED_EXIT, exec, refusal_line};

use cursor::STATE;
use put::MAX_BODY;
use stdio::{Event, MAX_LINE, Read};

/// Items per run the runner asks a connector for.
const LIMIT: u64 = 100;
/// Most `conflict` external ids a run record keeps.
const MAX_CONFLICT_IDS: usize = 16;
/// Bytes of the connector's stderr a run record keeps.
const STDERR_TAIL: usize = 2048;

/// The places a run uses besides the connector's own directory.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `connectors.state`.
    pub state: PathBuf,
    /// `memory.socket`, the C10 socket.
    pub archive: PathBuf,
}

/// The end of one run.
#[derive(Debug)]
pub struct Report {
    /// Its line for `connectors.log`.
    pub record: RunRecord,
    /// Why an outcome other than `ok` happened, for the stderr line only.
    pub reason: Option<String>,
}

impl Report {
    /// The run's one stderr line, without the program name.
    pub fn line(&self) -> String {
        let r = &self.record;
        let mut line = format!("{} {}", r.connector, r.outcome.as_str());
        if let Some(code) = &r.code {
            line.push(' ');
            line.push_str(code);
        }
        let _ = write!(
            line,
            ": stored {}, duplicate {}, conflict {}, too_large {}, cursor {}, {} ms",
            r.stored,
            r.duplicate,
            r.conflict,
            r.too_large,
            if r.cursor_moved { "moved" } else { "unchanged" },
            r.duration_ms
        );
        if let Some(reason) = &self.reason {
            line.push_str("; ");
            line.push_str(reason);
        }
        line
    }
}

/// Appends `record` as one JSON line to the run log `log`, created 0600.
///
/// # Errors
/// The log cannot be opened or written.
pub fn append(log: &Path, record: &RunRecord) -> io::Result<()> {
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(log)?
        .write_all(&line)
}

/// Runs `connector` once in its transient unit through `systemd-run`, whose stdio are this
/// process's pipes, puts its items into the archive under its name as `uid`, and moves its
/// cursor after the archive answered every item before it.
///
/// # Errors
/// The clock is before the Unix epoch.
pub fn run(
    config: &Config,
    paths: &Paths,
    connector: &Connector,
    uid: u32,
) -> Result<Report, String> {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("clock: {e}"))?;
    let started = Instant::now();
    let mut tally = Tally::default();
    let grants = grants(config, &connector.name);
    let prepared = match &grants {
        Ok(_) => prepare(config, paths, connector, since.as_secs()),
        Err(refusal) => Err(refusal.clone()),
    };
    let mut unit = None;
    let (outcome, code, reason, stderr) = match prepared {
        Err(Refusal { code, reason }) => (
            RunOutcome::Refused,
            Some(code.to_owned()),
            Some(reason),
            String::new(),
        ),
        Ok(prepared) => {
            tally.cursor.clone_from(&prepared.request.cursor);
            let mut sink = Sink {
                source: &connector.name,
                state_dir: &prepared.state_dir,
                archive: &paths.archive,
                uid,
                tally: &mut tally,
            };
            let end = drive(&prepared, &mut sink);
            if end.started {
                unit = Some(prepared.unit);
            }
            (end.outcome, end.code, end.reason, end.stderr)
        }
    };
    Ok(Report {
        record: RunRecord {
            at: since.as_secs_f64(),
            connector: connector.name.clone(),
            unit,
            outcome,
            code,
            stored: tally.stored,
            duplicate: tally.duplicate,
            conflict: tally.conflict,
            too_large: u64::try_from(tally.too_large_ids.len()).unwrap_or(u64::MAX),
            too_large_ids: tally.too_large_ids,
            conflict_ids: tally.conflict_ids,
            cursor_moved: tally.cursor_moved,
            landlock_abi: None,
            grants: grants.unwrap_or_else(|_| no_grants()),
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            stderr,
        },
        reason,
    })
}

/// A run stopped before its connector started.
#[derive(Clone)]
struct Refusal {
    code: &'static str,
    reason: String,
}

/// A run ready to start.
struct Prepared {
    name: String,
    unit: String,
    timeout: u64,
    launcher: PathBuf,
    program: PathBuf,
    args: Vec<String>,
    request: Request,
    state_dir: PathBuf,
    data_dir: PathBuf,
}

/// The request, unit and paths of a run of `connector` started at Unix second `at`, or why it
/// may not start.
fn prepare(
    config: &Config,
    paths: &Paths,
    connector: &Connector,
    at: u64,
) -> Result<Prepared, Refusal> {
    let name = &connector.name;
    let manifest = &connector.manifest;
    if manifest.transport != Transport::Stdio {
        return Err(Refusal {
            code: "transport_unsupported",
            reason: format!("transport {} is not run yet", manifest.transport.as_str()),
        });
    }
    let state_dir = paths.state.join(name);
    let data_dir = state_dir.join("data");
    if !data_dir.is_absolute() {
        return Err(Refusal {
            code: "data_dir_not_absolute",
            reason: format!(
                "data_dir {} is not absolute; connectors.state must be",
                data_dir.display()
            ),
        });
    }
    let key_file = match &manifest.key {
        None => None,
        Some(key) => Some(key_file(config, name, key)?),
    };
    let cursor = cursor::read(&state_dir).map_err(|reason| Refusal {
        code: "state_unreadable",
        reason,
    })?;
    if let Some(stored) = &cursor {
        stdio::check_cursor(stored).map_err(|reason| Refusal {
            code: "cursor_too_long",
            reason: format!("stored {reason}"),
        })?;
    }
    let contract = manifest::c11_version().map_err(|reason| Refusal {
        code: "contract",
        reason,
    })?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&data_dir)
        .map_err(|e| Refusal {
            code: "data_dir",
            reason: format!("{}: {e}", data_dir.display()),
        })?;
    let (program, args) = match manifest.command.split_first() {
        Some((program, args)) => (program, args.to_vec()),
        None => {
            return Err(Refusal {
                code: "command",
                reason: "command is empty".to_owned(),
            });
        }
    };
    let program = if program.starts_with('/') {
        PathBuf::from(program)
    } else {
        connector.dir.join(program)
    };
    let launcher = std::env::current_exe().map_err(|e| Refusal {
        code: "launcher",
        reason: format!("the path of this runner: {e}"),
    })?;
    Ok(Prepared {
        name: name.clone(),
        unit: unit::unit_name(name, at),
        timeout: manifest.timeout.unwrap_or(unit::DEFAULT_TIMEOUT),
        launcher,
        program,
        args,
        request: Request {
            contract: contract.to_owned(),
            cursor,
            limit: LIMIT,
            key_file,
            data_dir: path_text(&data_dir)?,
        },
        state_dir,
        data_dir,
    })
}

/// The owner's `connectors.<name>.key_file` for the manifest's `key`: an absolute path of an
/// existing file, which the runner never reads.
fn key_file(config: &Config, name: &str, key: &str) -> Result<String, Refusal> {
    let refusal = |code, reason| Refusal { code, reason };
    let setting = format!("connectors.{name}.key_file");
    let path = match optional(config, &setting) {
        Ok(Some(path)) => path,
        Ok(None) => {
            return Err(refusal(
                "key_not_configured",
                format!("the manifest needs key {key} and {setting} is not set"),
            ));
        }
        Err(reason) => return Err(refusal("owner_record", reason)),
    };
    if !Path::new(&path).is_absolute() {
        return Err(refusal(
            "key_file_not_absolute",
            format!("{setting} {path} is not absolute"),
        ));
    }
    if !Path::new(&path).is_file() {
        return Err(refusal(
            "key_not_configured",
            format!("{setting} {path} is not a file"),
        ));
    }
    Ok(path)
}

/// The UTF-8 text of `path` for the request.
fn path_text(path: &Path) -> Result<String, Refusal> {
    path.to_str().map(str::to_owned).ok_or_else(|| Refusal {
        code: "data_dir",
        reason: format!("data_dir {} is not UTF-8", path.display()),
    })
}

/// The owner's records of `connectors.<name>` as the run record lists them.
fn grants(config: &Config, name: &str) -> Result<Grants, Refusal> {
    let refusal = |reason| Refusal {
        code: "owner_record",
        reason,
    };
    let list = |key: &str| -> Result<Option<Vec<String>>, Refusal> {
        Ok(optional(config, &format!("connectors.{name}.{key}"))
            .map_err(refusal)?
            .map(|text| {
                text.split(',')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_owned)
                    .collect()
            }))
    };
    let allow = list("allow")?;
    for entry in allow.iter().flatten() {
        manifest::host_port(entry).map_err(|e| refusal(format!("connectors.{name}.allow: {e}")))?;
    }
    let network = match optional(config, &format!("connectors.{name}.network")).map_err(refusal)? {
        None => None,
        Some(value) if value == "direct" => Some(GrantsNetwork::Direct),
        Some(_) => {
            return Err(refusal(format!(
                "connectors.{name}.network must be \"direct\""
            )));
        }
    };
    let landlock =
        match optional(config, &format!("connectors.{name}.landlock")).map_err(refusal)? {
            None => None,
            Some(value) if value == "off" => Some(GrantsLandlock::Off),
            Some(_) => {
                return Err(refusal(format!(
                    "connectors.{name}.landlock must be \"off\""
                )));
            }
        };
    Ok(Grants {
        allow,
        read: list("read")?,
        write: list("write")?,
        network,
        landlock,
    })
}

fn no_grants() -> Grants {
    Grants {
        allow: None,
        read: None,
        write: None,
        network: None,
        landlock: None,
    }
}

/// The string of `key`, none when unset; an error names a key of another kind.
fn optional(config: &Config, key: &str) -> Result<Option<String>, String> {
    match config.get(key).map(|e| &e.value) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// What a run counted.
#[derive(Default)]
struct Tally {
    stored: u64,
    duplicate: u64,
    conflict: u64,
    too_large_ids: Vec<String>,
    conflict_ids: Vec<String>,
    /// The cursor stored now.
    cursor: Option<String>,
    cursor_moved: bool,
}

/// Where a run's items and cursors go.
struct Sink<'a> {
    source: &'a str,
    state_dir: &'a Path,
    archive: &'a Path,
    uid: u32,
    tally: &'a mut Tally,
}

impl Sink<'_> {
    /// Puts `item` under the run's source; an error ends the run `archive`.
    fn deliver(&mut self, item: Item) -> Result<(), String> {
        let external_id = item.external_id;
        let body = serde_json::to_vec(&PutRequest {
            source: self.source.to_owned(),
            external_id: external_id.clone(),
            who: item.who,
            at: item.at,
            kind: item.kind,
            text: item.text,
            bytes_b64: item.bytes_b64,
        })
        .map_err(|e| format!("put {external_id:?}: {e}"))?;
        if body.len() > MAX_BODY {
            self.tally.too_large_ids.push(external_id);
            return Ok(());
        }
        match put::put(self.archive, self.uid, &body) {
            Ok(put::Answer::Stored) => self.tally.stored += 1,
            Ok(put::Answer::Duplicate) => self.tally.duplicate += 1,
            Ok(put::Answer::Conflict) => {
                self.tally.conflict += 1;
                if self.tally.conflict_ids.len() < MAX_CONFLICT_IDS {
                    self.tally.conflict_ids.push(external_id);
                }
            }
            Err(reason) => return Err(format!("put {external_id:?}: {reason}")),
        }
        Ok(())
    }

    /// Stores `cursor`, which every item before it was answered for.
    fn store(&mut self, cursor: String) -> Result<(), String> {
        if self.tally.cursor.as_ref() == Some(&cursor) {
            return Ok(());
        }
        cursor::write(self.state_dir, Some(&cursor))
            .map_err(|e| format!("cursor {}: {e}", self.state_dir.join(STATE).display()))?;
        self.tally.cursor = Some(cursor);
        self.tally.cursor_moved = true;
        Ok(())
    }
}

/// How reading stdout stopped.
enum Stop {
    Done,
    Error(String, String),
    End,
    Protocol(String),
    Archive(String),
}

/// How a run ended.
struct End {
    outcome: RunOutcome,
    code: Option<String>,
    reason: Option<String>,
    stderr: String,
    /// Whether `systemd-run` started, so a unit may have run.
    started: bool,
}

/// Starts the run's unit, feeds the connector's lines to `sink` and waits for it.
fn drive(prepared: &Prepared, sink: &mut Sink<'_>) -> End {
    let spawned = Command::new("systemd-run")
        .args(unit::systemd_run(&prepared.unit, prepared.timeout))
        .arg("--")
        .arg(&prepared.launcher)
        .arg("exec")
        .arg(&prepared.name)
        .arg(&prepared.data_dir)
        .arg("--")
        .arg(&prepared.program)
        .args(&prepared.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            return End {
                outcome: RunOutcome::Refused,
                code: Some("spawn_failed".to_owned()),
                reason: Some(format!("systemd-run: {e}")),
                stderr: String::new(),
                started: false,
            };
        }
    };
    let started = Instant::now();
    let tail = child.stderr.take().map(stderr_tail);
    let stop = feed(&mut child, &prepared.request, sink);
    let ran = started.elapsed();
    let stopped = if matches!(stop, Stop::Protocol(_) | Stop::Archive(_)) {
        stop_unit(&mut child, &prepared.unit)
    } else {
        None
    };
    let status = child.wait();
    let stderr = tail
        .and_then(|handle| handle.join().ok())
        .map(|bytes| utf8_tail(&bytes))
        .unwrap_or_default();
    let (outcome, code, reason) = outcome(stop, status, &stderr, ran, prepared.timeout);
    let reason = match (reason, stopped) {
        (Some(reason), Some(failed)) => Some(format!("{reason}; {failed}")),
        (reason, failed) => reason.or(failed),
    };
    End {
        outcome,
        code,
        reason,
        stderr,
        started: true,
    }
}

/// Ends a run the runner gave up on: `systemd-run` is killed, which leaves the unit, so the
/// manager stops the unit too. The text says why the stop failed.
fn stop_unit(child: &mut Child, unit: &str) -> Option<String> {
    // Already exited is the only failure, and the wait after this reaps it either way.
    let _ = child.kill();
    match Command::new("systemctl")
        .args(["--user", "stop", unit])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("systemctl --user stop {unit}: {status}")),
        Err(e) => Some(format!("systemctl --user stop {unit}: {e}")),
    }
}

/// The outcome, code and reason of a run that stopped as `stop`, exited with `status` and
/// left `stderr`, after its stdout was read for `ran` of its `timeout` seconds.
fn outcome(
    stop: Stop,
    status: io::Result<ExitStatus>,
    stderr: &str,
    ran: Duration,
    timeout: u64,
) -> (RunOutcome, Option<String>, Option<String>) {
    let timed_out = ran >= Duration::from_secs(timeout);
    match (stop, status) {
        (Stop::Done, _) => (RunOutcome::Ok, None, None),
        (Stop::Error(code, message), _) => (RunOutcome::Error, Some(code), Some(message)),
        (Stop::Archive(reason), _) => (RunOutcome::Archive, None, Some(reason)),
        // The unit's manager ends a run at `RuntimeMaxSec=`, which cuts stdout at any byte.
        (Stop::End | Stop::Protocol(_), _) if timed_out => (
            RunOutcome::Timeout,
            None,
            Some(format!("ran past its timeout of {timeout} s")),
        ),
        (Stop::Protocol(reason), _) => (RunOutcome::Protocol, None, Some(reason)),
        (Stop::End, Ok(status)) if status.code() == Some(i32::from(REFUSED_EXIT)) => {
            match unit::refusal(stderr) {
                Some((code, reason)) => (RunOutcome::Refused, Some(code), Some(reason)),
                None => (RunOutcome::Exit, None, Some(status.to_string())),
            }
        }
        (Stop::End, Ok(status)) if status.success() => (
            RunOutcome::Protocol,
            None,
            Some("stdout ended without a done or error line".to_owned()),
        ),
        (Stop::End, Ok(status)) => (RunOutcome::Exit, None, Some(status.to_string())),
        (Stop::End, Err(e)) => (RunOutcome::Exit, None, Some(format!("wait: {e}"))),
    }
}

/// Writes the request, then reads and handles stdout until the last line or a failure.
fn feed(child: &mut Child, request: &Request, sink: &mut Sink<'_>) -> Stop {
    let written = serde_json::to_vec(request)
        .map_err(io::Error::from)
        .and_then(|mut line| {
            line.push(b'\n');
            match child.stdin.take() {
                Some(mut stdin) => stdin.write_all(&line),
                None => Err(io::Error::other("no stdin pipe")),
            }
        });
    match written {
        // A connector may close stdin unread; its stdout still decides the run.
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => {
            return Stop::Protocol(format!("stdin: {e}"));
        }
        _ => {}
    }
    let Some(stdout) = child.stdout.take() else {
        return Stop::Protocol("no stdout pipe".to_owned());
    };
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    let mut buf = Vec::new();
    let mut n = 0u64;
    loop {
        n += 1;
        let event = match stdio::read_line(&mut reader, &mut buf) {
            Err(e) => return Stop::Protocol(format!("stdout: {e}")),
            Ok(Read::End) => return Stop::End,
            Ok(Read::Unterminated) => {
                return Stop::Protocol(format!("line {n} ends stdout without a newline"));
            }
            Ok(Read::Long) => match stdio::long_item(&buf) {
                Some(external_id) => {
                    sink.tally.too_large_ids.push(external_id);
                    continue;
                }
                None => {
                    return Stop::Protocol(format!(
                        "line {n} is over {MAX_LINE} bytes and not an item with type and external_id before the cut"
                    ));
                }
            },
            Ok(Read::Line) => match stdio::event(&buf) {
                Ok(event) => event,
                Err(reason) => return Stop::Protocol(format!("line {n}: {reason}")),
            },
        };
        let handled = match event {
            Event::Item(item) => sink.deliver(item),
            Event::Checkpoint(cursor) => sink.store(cursor),
            Event::Done(cursor) => {
                return match cursor.map_or(Ok(()), |c| sink.store(c)) {
                    Ok(()) => Stop::Done,
                    Err(reason) => Stop::Archive(reason),
                };
            }
            Event::Error(code, message) => return Stop::Error(code.as_str().to_owned(), message),
        };
        if let Err(reason) = handled {
            return Stop::Archive(reason);
        }
    }
}

/// Drains `stderr` on a thread, keeping its last [`STDERR_TAIL`] bytes.
fn stderr_tail(mut stderr: ChildStderr) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    kept.extend_from_slice(&chunk[..n]);
                    if kept.len() > 2 * STDERR_TAIL {
                        kept.drain(..kept.len() - STDERR_TAIL);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // The tail is diagnostics only; the run's outcome comes from stdout and the exit.
                Err(_) => break,
            }
        }
        kept
    })
}

/// The last [`STDERR_TAIL`] bytes of `bytes` from a UTF-8 boundary on.
fn utf8_tail(bytes: &[u8]) -> String {
    let mut tail = &bytes[bytes.len().saturating_sub(STDERR_TAIL)..];
    while tail.first().is_some_and(|b| b & 0xC0 == 0x80) {
        tail = &tail[1..];
    }
    String::from_utf8_lossy(tail).into_owned()
}
