//! The source connectors' runner (ADR-0056 sections 3 to 5.5): discovery of the connected
//! connectors, one run of a stdio or MCP connector in its transient unit over pipes into C10
//! `put` with its network broker, its state and lock, its run record, `reset`, the schedule of
//! `serve`, and the launcher inside the unit with its Landlock.

mod broker;
mod cursor;
mod daemon;
mod manifest;
mod mcp;
mod put;
mod sandbox;
mod schedule;
mod stderr;
mod stdio;
mod unit;

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, BufReader, Read as _, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use ulei_contracts::config::{Config, Value};
use ulei_contracts::generated::c10::PutRequest;
use ulei_contracts::generated::c11::{
    ErrorCode, Grants, GrantsLandlock, GrantsNetwork, Item, Request, RunOutcome, RunRecord,
    Transport,
};

pub use cursor::reset;
pub use daemon::{Wake, say, serve, stop_on_signals, sync_every};
pub use manifest::{Connector, Discovery, discover, enabled, is_name};
pub use unit::{Launch, REFUSED_EXIT, exec, refusal_line};

use crate::serve::now;
use broker::{Broker, Policy};
use cursor::{STATE, State};
use put::MAX_BODY;
use stderr::{Launcher, Seen, Stderr};
use stdio::{Event, MAX_LINE, Read};

/// Items per run the runner asks a connector for.
const LIMIT: u64 = 100;
/// Most `conflict` external ids a run record keeps.
const MAX_CONFLICT_IDS: usize = 16;
/// Longest broker socket path in bytes: `sun_path` holds 108 bytes with the trailing NUL.
const MAX_SOCKET_PATH: usize = 107;

/// The places a run uses besides the connector's own directory.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `connectors.state`.
    pub state: PathBuf,
    /// `memory.socket`, the C10 socket.
    pub archive: PathBuf,
    /// `connectors.run_dir`, where each run's broker listens.
    pub run_dir: PathBuf,
}

/// The end of one run.
#[derive(Debug)]
pub struct Report {
    /// Its line for `connectors.log`.
    pub record: RunRecord,
    /// For the stderr line only: why an outcome other than `ok` happened, or how the runner
    /// ended the unit.
    pub reason: Option<String>,
    /// Unix time before which the next run may not start, from a `rate_limited` error.
    pub not_before: Option<f64>,
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
        if let Some(abi) = r.landlock_abi {
            let _ = write!(line, ", landlock abi {abi}");
        } else if r.grants.landlock == Some(GrantsLandlock::Off) {
            line.push_str(", landlock off");
        }
        if r.grants.network == Some(GrantsNetwork::Direct) {
            match &r.connect_ports {
                Some(ports) => {
                    let shown: Vec<String> = ports.iter().map(u64::to_string).collect();
                    let _ = write!(line, ", direct connect ports [{}]", shown.join(","));
                }
                None => line.push_str(", direct connect ports not limited"),
            }
        }
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
/// cursor after the archive answered every item before it. A run of the same connector in
/// flight in any process makes it `busy`; `runs` holds its unit while it runs.
///
/// # Errors
/// The clock is before the Unix epoch.
pub fn run(
    config: &Config,
    paths: &Paths,
    connector: &Connector,
    uid: u32,
    runs: &Runs,
) -> Result<Report, String> {
    let since = now().map_err(|e| format!("clock: {e}"))?;
    let second = Duration::try_from_secs_f64(since)
        .map_err(|e| format!("clock: {e}"))?
        .as_secs();
    let started = Instant::now();
    let mut tally = Tally::default();
    let grants = grants(config, &connector.name);
    let begun = match &grants {
        Ok(grants) => begin(config, paths, connector, grants, (since, second)),
        Err(refusal) => Err(refusal.clone()),
    };
    let mut unit = None;
    let mut landlock_abi = None;
    let mut connect_ports = None;
    let mut not_before = None;
    let (outcome, code, reason, stderr) = match begun {
        Err(Refusal { code, reason }) => (
            RunOutcome::Refused,
            Some(code.to_owned()),
            Some(reason),
            String::new(),
        ),
        Ok(None) => {
            return Ok(busy(
                &connector.name,
                since,
                grants.unwrap_or_else(|_| no_grants()),
            ));
        }
        Ok(Some((_lock, prepared))) => {
            tally.state.clone_from(&prepared.state);
            let mut sink = Sink {
                source: &connector.name,
                state_dir: &prepared.state_dir,
                archive: &paths.archive,
                uid,
                tally: &mut tally,
            };
            let end = drive(&prepared, &mut sink, runs);
            if end.seen.started {
                unit = Some(prepared.unit);
            }
            if let Some(Launcher::Landlock(abi, ports)) = end.seen.launcher {
                landlock_abi = abi;
                connect_ports = ports.map(|p| p.into_iter().map(u64::from).collect());
            }
            let mut reason = end.reason;
            if let Some(retry_after) = end.retry_after {
                match rate_limited(&prepared.state_dir, &mut tally.state, retry_after) {
                    Ok(at) => not_before = Some(at),
                    Err(e) => reason = Some(format!("{}; {e}", reason.unwrap_or_default())),
                }
            }
            (end.outcome, end.code, reason, end.seen.tail)
        }
    };
    Ok(Report {
        record: RunRecord {
            at: since,
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
            landlock_abi,
            connect_ports,
            grants: grants.unwrap_or_else(|_| no_grants()),
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            stderr,
        },
        reason,
        not_before,
    })
}

/// The record of a run of `name` due at Unix time `at` that did not start because its last run
/// still runs; `grants` are the owner's records in effect.
fn busy(name: &str, at: f64, grants: Grants) -> Report {
    Report {
        record: RunRecord {
            at,
            connector: name.to_owned(),
            unit: None,
            outcome: RunOutcome::Busy,
            code: None,
            stored: 0,
            duplicate: 0,
            conflict: 0,
            too_large: 0,
            too_large_ids: Vec::new(),
            conflict_ids: Vec::new(),
            cursor_moved: false,
            landlock_abi: None,
            connect_ports: None,
            grants,
            duration_ms: 0,
            stderr: String::new(),
        },
        reason: Some("its last run still runs".to_owned()),
        not_before: None,
    }
}

/// The owner's records of `connectors.<name>` as a run record lists them; none when they are
/// malformed, which the run itself refuses.
fn owner_grants(config: &Config, name: &str) -> Grants {
    grants(config, name).unwrap_or_else(|_| no_grants())
}

/// Stores `not_before` as now plus `retry_after` seconds in the state of `state_dir`.
fn rate_limited(state_dir: &Path, state: &mut State, retry_after: u64) -> Result<f64, String> {
    let at = now().map_err(|e| format!("clock: {e}"))? + daemon::seconds(retry_after);
    state.not_before = Some(at);
    cursor::write(state_dir, state)
        .map(|()| at)
        .map_err(|e| format!("not_before {}: {e}", state_dir.join(STATE).display()))
}

/// The units of the runs in flight in this process, which a shutdown stops.
#[derive(Debug, Clone, Default)]
pub struct Runs(Arc<Mutex<InFlight>>);

#[derive(Debug, Default)]
struct InFlight {
    stopping: bool,
    units: Vec<String>,
}

impl Runs {
    fn lock(&self) -> std::sync::MutexGuard<'_, InFlight> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn add(&self, unit: &str) {
        self.lock().units.push(unit.to_owned());
    }

    fn remove(&self, unit: &str) {
        self.lock().units.retain(|u| u != unit);
    }

    fn stopping(&self) -> bool {
        self.lock().stopping
    }

    /// Stops the units in flight and every unit that starts later; one text per failed stop.
    pub fn stop_all(&self) -> Vec<String> {
        let units = {
            let mut in_flight = self.lock();
            in_flight.stopping = true;
            in_flight.units.clone()
        };
        units.iter().filter_map(|unit| stop(unit).err()).collect()
    }
}

/// A run stopped before its connector started.
#[derive(Clone)]
struct Refusal {
    code: &'static str,
    reason: String,
}

/// A run ready to start.
struct Prepared {
    transport: Transport,
    unit: String,
    timeout: u64,
    /// `<connectors.run_dir>/<unit>.sock`.
    broker: PathBuf,
    policy: Policy,
    launcher: PathBuf,
    launch: Launch,
    request: Request,
    state_dir: PathBuf,
    /// The state as the run started it.
    state: State,
}

/// The run lock of `connector` and its prepared run started at Unix time `since`, whose whole
/// `second` names the unit; none while another run holds the lock.
fn begin(
    config: &Config,
    paths: &Paths,
    connector: &Connector,
    grants: &Grants,
    (since, second): (f64, u64),
) -> Result<Option<(File, Prepared)>, Refusal> {
    let state_dir = paths.state.join(&connector.name);
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
    let mut prepared = prepare(config, paths, connector, grants, second)?;
    let lock = match cursor::lock(&state_dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Ok(None),
        Err(e) => {
            return Err(Refusal {
                code: "lock",
                reason: format!("{}: {e}", state_dir.display()),
            });
        }
    };
    let stored = cursor::read(&state_dir).map_err(|reason| Refusal {
        code: "state_unreadable",
        reason,
    })?;
    if let Some(cursor) = &stored.cursor {
        stdio::check_cursor(cursor).map_err(|reason| Refusal {
            code: "cursor_too_long",
            reason: format!("stored {reason}"),
        })?;
    }
    for (code, dir) in [("data_dir", &data_dir), ("run_dir", &paths.run_dir)] {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| Refusal {
                code,
                reason: format!("{}: {e}", dir.display()),
            })?;
    }
    prepared.request.cursor.clone_from(&stored.cursor);
    prepared.state = State {
        cursor: stored.cursor,
        last_start: Some(since),
        not_before: None,
    };
    cursor::write(&state_dir, &prepared.state).map_err(|e| Refusal {
        code: "state_unwritable",
        reason: format!("{}: {e}", state_dir.join(STATE).display()),
    })?;
    Ok(Some((lock, prepared)))
}

/// The request without its cursor, the unit and the launch of a run of `connector` under the
/// owner's `grants` started at Unix second `at`, or why it may not start.
fn prepare(
    config: &Config,
    paths: &Paths,
    connector: &Connector,
    grants: &Grants,
    at: u64,
) -> Result<Prepared, Refusal> {
    let name = &connector.name;
    let manifest = &connector.manifest;
    let state_dir = paths.state.join(name);
    let data_dir = state_dir.join("data");
    let key_file = match &manifest.key {
        None => None,
        Some(key) => Some(key_file(config, name, key)?),
    };
    let request_data_dir = path_text(&data_dir)?;
    let unit = unit::unit_name(name, at);
    let broker = broker_socket(&paths.run_dir, &unit)?;
    let contract = manifest::c11_version().map_err(|reason| Refusal {
        code: "contract",
        reason,
    })?;
    let (program, args) = match manifest.command.split_first() {
        Some((program, args)) => (program, args.iter().map(OsString::from).collect()),
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
    let paths = |list: &Option<Vec<String>>| list.iter().flatten().map(PathBuf::from).collect();
    let (policy, direct) = network(&manifest.source.addresses, grants);
    Ok(Prepared {
        transport: manifest.transport,
        unit,
        timeout: manifest.timeout.unwrap_or(unit::DEFAULT_TIMEOUT),
        broker,
        policy,
        launcher,
        launch: Launch {
            name: name.clone(),
            data_dir,
            dir: connector.dir.clone(),
            key: key_file.as_ref().map(PathBuf::from),
            read: paths(&grants.read),
            write: paths(&grants.write),
            landlock: grants.landlock.is_none(),
            direct,
            program: program.into_os_string(),
            args,
        },
        request: Request {
            contract: contract.to_owned(),
            cursor: None,
            limit: LIMIT,
            key_file,
            data_dir: request_data_dir,
        },
        state_dir,
        state: State::default(),
    })
}

/// The broker socket of `unit` in `run_dir`, a path `OpenFile=` and `sun_path` take.
fn broker_socket(run_dir: &Path, unit: &str) -> Result<PathBuf, Refusal> {
    let refusal = |reason| Refusal {
        code: "run_dir",
        reason,
    };
    let stem = unit.strip_suffix(".service").unwrap_or(unit);
    let path = run_dir.join(format!("{stem}.sock"));
    let text = path.to_str().ok_or_else(|| {
        refusal(format!(
            "connectors.run_dir {} is not UTF-8",
            run_dir.display()
        ))
    })?;
    if !run_dir.is_absolute() || text.contains(':') {
        return Err(refusal(format!(
            "connectors.run_dir {text} must be absolute and hold no colon, which OpenFile= separates at"
        )));
    }
    if text.len() > MAX_SOCKET_PATH {
        return Err(refusal(format!(
            "broker socket {text} is over {MAX_SOCKET_PATH} bytes; a shorter connectors.run_dir is needed"
        )));
    }
    Ok(path)
}

/// The broker's policy for a manifest's `listed` addresses under the owner's `grants`, and
/// under `network = "direct"` the ports of both entry lists, ascending and each once.
fn network(listed: &[String], grants: &Grants) -> (Policy, Option<Vec<u16>>) {
    let policy = Policy {
        listed: listed.to_vec(),
        allow: grants.allow.clone().unwrap_or_default(),
    };
    let direct = grants.network.map(|_| {
        // The entries are checked C11 `HostPort`s, so each ends in its port.
        let mut ports: Vec<u16> = (policy.listed.iter().chain(&policy.allow))
            .filter_map(|entry| entry.rsplit_once(':')?.1.parse().ok())
            .collect();
        ports.sort_unstable();
        ports.dedup();
        ports
    });
    (policy, direct)
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
    let read = list("read")?;
    let write = list("write")?;
    for (key, paths) in [("read", &read), ("write", &write)] {
        if let Some(path) = paths.iter().flatten().find(|p| !Path::new(p).is_absolute()) {
            return Err(refusal(format!(
                "connectors.{name}.{key}: {path} is not absolute"
            )));
        }
    }
    Ok(Grants {
        allow,
        read,
        write,
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
    /// The state stored now.
    state: State,
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
        let request = PutRequest {
            source: self.source.to_owned(),
            external_id: item.external_id,
            who: item.who,
            at: item.at,
            kind: item.kind,
            text: item.text,
            bytes_b64: item.bytes_b64,
        };
        let id = request.external_id.clone();
        let len = put::body_len(&request).map_err(|e| format!("put {id:?}: {e}"))?;
        if len > MAX_BODY {
            self.tally.too_large_ids.push(id);
            return Ok(());
        }
        match put::put(self.archive, self.uid, &request, len) {
            Ok(put::Answer::Stored) => self.tally.stored += 1,
            Ok(put::Answer::Duplicate) => self.tally.duplicate += 1,
            Ok(put::Answer::Conflict) => {
                self.tally.conflict += 1;
                if self.tally.conflict_ids.len() < MAX_CONFLICT_IDS {
                    self.tally.conflict_ids.push(id);
                }
            }
            Err(reason) => return Err(format!("put {id:?}: {reason}")),
        }
        Ok(())
    }

    /// Stores `cursor`, which every item before it was answered for.
    fn store(&mut self, cursor: String) -> Result<(), String> {
        if self.tally.state.cursor.as_ref() == Some(&cursor) {
            return Ok(());
        }
        let state = State {
            cursor: Some(cursor),
            ..self.tally.state.clone()
        };
        cursor::write(self.state_dir, &state)
            .map_err(|e| format!("cursor {}: {e}", self.state_dir.join(STATE).display()))?;
        self.tally.state = state;
        self.tally.cursor_moved = true;
        Ok(())
    }

    /// Takes one event of either transport; what ends the run.
    fn handle(&mut self, event: Event) -> Option<Stop> {
        let handled = match event {
            Event::Item(item) => self.deliver(item),
            Event::TooLarge(external_id) => {
                self.tally.too_large_ids.push(external_id);
                Ok(())
            }
            Event::Checkpoint(cursor) => self.store(cursor),
            Event::Done(cursor) => {
                return Some(match cursor.map_or(Ok(()), |c| self.store(c)) {
                    Ok(()) => Stop::Done,
                    Err(reason) => Stop::Archive(reason),
                });
            }
            Event::Error(code, message, retry_after) => {
                return Some(Stop::Error(code.as_str().to_owned(), message, retry_after));
            }
        };
        handled.err().map(Stop::Archive)
    }
}

/// How reading stdout stopped.
enum Stop {
    Done,
    /// An `Error` line: its code, message and, for `rate_limited`, its `retry_after`.
    Error(String, String, Option<u64>),
    End,
    Protocol(String),
    Archive(String),
}

/// How a run ended.
struct End {
    outcome: RunOutcome,
    code: Option<String>,
    reason: Option<String>,
    seen: Seen,
    /// The `retry_after` of a `rate_limited` error.
    retry_after: Option<u64>,
}

/// Starts the run's broker and unit, feeds the connector's lines to `sink` and waits for it;
/// the broker ends with the run.
fn drive(prepared: &Prepared, sink: &mut Sink<'_>, runs: &Runs) -> End {
    let broker = match Broker::listen(&prepared.broker, prepared.policy.clone()) {
        Ok(broker) => broker,
        Err(reason) => {
            return End {
                outcome: RunOutcome::Refused,
                code: Some("broker".to_owned()),
                reason: Some(reason),
                seen: Stderr::default().end(),
                retry_after: None,
            };
        }
    };
    runs.add(&prepared.unit);
    let end = run_unit(prepared, sink, runs);
    runs.remove(&prepared.unit);
    drop(broker);
    end
}

/// Starts the run's unit, feeds the connector's lines to `sink` and waits for it; a shutdown
/// of `runs` stops the unit once it started.
fn run_unit(prepared: &Prepared, sink: &mut Sink<'_>, runs: &Runs) -> End {
    let direct = prepared.launch.direct.is_some();
    let spawned = Command::new("systemd-run")
        .args(unit::systemd_run(
            &prepared.unit,
            prepared.timeout,
            &prepared.broker,
            direct,
        ))
        .arg("--")
        .arg(&prepared.launcher)
        .arg("exec")
        .args(prepared.launch.argv())
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
                seen: Stderr::default().end(),
                retry_after: None,
            };
        }
    };
    let (unit, in_flight) = (prepared.unit.clone(), runs.clone());
    // A shutdown that came before the unit existed cannot stop it; this stop follows its start.
    let on_start = move || {
        if in_flight.stopping() {
            let _ = stop(&unit);
        }
    };
    let watch = child.stderr.take().map(|pipe| watch_stderr(pipe, on_start));
    let stop = match prepared.transport {
        Transport::Stdio => feed(&mut child, &prepared.request, sink),
        Transport::Mcp => mcp::feed(&mut child, &prepared.request, sink),
    };
    let mut seen = None;
    let stopped = match stop {
        Stop::Protocol(_) | Stop::Archive(_) => stop_unit(&mut child, &prepared.unit),
        // The MCP server got the end of its stdin after its last answer and should exit.
        Stop::Done | Stop::Error(..) if prepared.transport == Transport::Mcp => {
            match watch.as_ref().map(|rx| rx.recv_timeout(mcp::LINGER)) {
                Some(Err(RecvTimeoutError::Timeout)) => {
                    let lingered = format!(
                        "stopped: the server did not exit within {} s of its last answer",
                        mcp::LINGER.as_secs()
                    );
                    Some(match stop_unit(&mut child, &prepared.unit) {
                        Some(failed) => format!("{lingered}; {failed}"),
                        None => lingered,
                    })
                }
                Some(Ok(ended)) => {
                    seen = Some(ended);
                    None
                }
                Some(Err(RecvTimeoutError::Disconnected)) | None => None,
            }
        }
        Stop::Done | Stop::Error(..) | Stop::End => None,
    };
    let status = child.wait();
    let seen = seen
        .or_else(|| watch.and_then(|rx| rx.recv().ok()))
        .unwrap_or_else(|| Stderr::default().end());
    let retry_after = match &stop {
        Stop::Error(code, _, retry_after) if code == ErrorCode::RateLimited.as_str() => {
            *retry_after
        }
        _ => None,
    };
    let (outcome, code, reason) = outcome(stop, status, &seen, prepared.timeout);
    let shutdown = runs
        .stopping()
        .then(|| "stopped at the runner's shutdown".to_owned());
    let reason = [reason, stopped, shutdown]
        .into_iter()
        .flatten()
        .reduce(|all, next| format!("{all}; {next}"));
    End {
        outcome,
        code,
        reason,
        seen,
        retry_after,
    }
}

/// Ends a run the runner gave up on: the manager stops the unit, after which `systemd-run`
/// reports the unit's result and exits; when the stop fails, `systemd-run` is killed instead.
/// The text says why the stop failed.
fn stop_unit(child: &mut Child, unit: &str) -> Option<String> {
    let failed = stop(unit).err()?;
    // Already exited is the only failure, and the wait after this reaps it either way.
    let _ = child.kill();
    Some(failed)
}

/// `systemctl --user stop unit`.
///
/// # Errors
/// The stop failed; the text names the unit and why.
fn stop(unit: &str) -> Result<(), String> {
    match Command::new("systemctl")
        .args(["--user", "stop", unit])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("systemctl --user stop {unit}: {status}")),
        Err(e) => Err(format!("systemctl --user stop {unit}: {e}")),
    }
}

/// The outcome, code and reason of a run that stopped as `stop`, whose `systemd-run` exited with
/// `status` after its stderr showed `seen`, under its `timeout` seconds.
fn outcome(
    stop: Stop,
    status: io::Result<ExitStatus>,
    seen: &Seen,
    timeout: u64,
) -> (RunOutcome, Option<String>, Option<String>) {
    let timed_out = seen.result.as_deref() == Some("timeout");
    match (stop, status) {
        (Stop::Done, _) => (RunOutcome::Ok, None, None),
        (Stop::Error(code, message, _), _) => (RunOutcome::Error, Some(code), Some(message)),
        (Stop::Archive(reason), _) => (RunOutcome::Archive, None, Some(reason)),
        // The unit's manager ends a run at `RuntimeMaxSec=`, which cuts stdout at any byte.
        (Stop::End | Stop::Protocol(_), _) if timed_out => (
            RunOutcome::Timeout,
            None,
            Some(format!("ran past its timeout of {timeout} s")),
        ),
        (Stop::Protocol(reason), _) => (RunOutcome::Protocol, None, Some(reason)),
        (Stop::End, status) if !seen.started => {
            let said = seen.tail.lines().rfind(|line| !line.trim().is_empty());
            let reason = match (said, status) {
                (Some(said), _) => format!("systemd-run: {said}"),
                (None, Ok(status)) => format!("systemd-run {status} without starting the unit"),
                (None, Err(e)) => format!("systemd-run: wait: {e}"),
            };
            (
                RunOutcome::Refused,
                Some("unit_start_failed".to_owned()),
                Some(reason),
            )
        }
        (Stop::End, Ok(status)) if status.code() == Some(i32::from(REFUSED_EXIT)) => {
            match (&seen.launcher, &seen.exec_failed) {
                (Some(Launcher::Refused(code, reason)), _) => (
                    RunOutcome::Refused,
                    Some(code.clone()),
                    Some(reason.clone()),
                ),
                (_, Some(reason)) => (
                    RunOutcome::Refused,
                    Some(unit::EXEC_FAILED.to_owned()),
                    Some(reason.clone()),
                ),
                _ => (RunOutcome::Exit, None, Some(status.to_string())),
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
            Ok(Read::Long(Ok(external_id))) => Event::TooLarge(external_id),
            Ok(Read::Long(Err(why))) => {
                return Stop::Protocol(format!(
                    "line {n} is over {MAX_LINE} bytes and {why}, so no too_large id can be kept"
                ));
            }
            Ok(Read::Line) => match stdio::event(&buf) {
                Ok(event) => event,
                Err(reason) => return Stop::Protocol(format!("line {n}: {reason}")),
            },
        };
        if let Some(stop) = sink.handle(event) {
            return stop;
        }
    }
}

/// Reads the run's stderr on a thread until it closes, which happens once `systemd-run` and the
/// unit ended, and then sends what it saw; `on_start` runs once the unit started.
fn watch_stderr(pipe: ChildStderr, on_start: impl FnOnce() + Send + 'static) -> Receiver<Seen> {
    let (seen, watched) = mpsc::channel();
    thread::spawn(move || {
        // The receiver is gone only once the run no longer waits for it.
        let _ = seen.send(read_stderr(pipe, on_start));
    });
    watched
}

/// What the run's stderr showed until it closed.
fn read_stderr(mut pipe: ChildStderr, on_start: impl FnOnce()) -> Seen {
    let mut stderr = Stderr::default();
    let mut chunk = [0u8; 4096];
    let mut on_start = Some(on_start);
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                stderr.feed(&chunk[..n]);
                if stderr.started()
                    && let Some(on_start) = on_start.take()
                {
                    on_start();
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // The pipe is diagnostics and reports; a read error ends them like a close.
            Err(_) => break,
        }
    }
    stderr.end()
}
