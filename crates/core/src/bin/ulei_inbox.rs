//! `ulei-inbox`: everything that opens `events.sqlite3` (ADR-0007): the `serve` daemon and
//! the `shadow` pass; and `status` and `reconcile`, which open no database and talk C1 to the
//! daemon (ADR-0040 section 5, ADR-0099).

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::io::Write;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ulei_contracts::config::{Config, Value};
use ulei_core::c6::{CALL_TIMEOUT, CONNECT_TIMEOUT, Client};
use ulei_core::cli::flags;
use ulei_core::config::string;
use ulei_core::deliver::{self, Consumer, Pace};
use ulei_core::inbox::shadow::{self, Failure};
use ulei_core::inbox::{Inbox, own_uid};
use ulei_core::reconcile;
use ulei_core::serve::{READ_TIMEOUT, Server, bind};
use ulei_core::status;

const USAGE: &str =
    "usage: ulei-inbox serve [--config PATH] [--deliver-tick-ms N] [--lease-ttl-ms N] \
[--retry-delay-ms N]
       ulei-inbox shadow --live-root DIR --state DIR --out FILE.jsonl
       ulei-inbox status [--config PATH]
       ulei-inbox reconcile ID --outcome reconciled-delivered --note TEXT [--config PATH]
";
/// Exit status of a usage error or of a config `status` cannot use.
const USAGE_ERROR: u8 = 2;
/// Exit status of a shadow pass that met a busy or hot live database (`EX_TEMPFAIL`).
const RETRY_LATER: u8 = 75;
/// The delivery tick `--deliver-tick-ms` accepts, in milliseconds.
const TICK_MS: RangeInclusive<u64> = 10..=2000;
/// The lease `--lease-ttl-ms` accepts, in milliseconds: at least two of the longest ticks, so a
/// lease renewed every tick never runs out between renews.
const LEASE_TTL_MS: RangeInclusive<u64> = 4000..=120_000;
/// The retry delay `--retry-delay-ms` accepts, in milliseconds.
const RETRY_DELAY_MS: RangeInclusive<u64> = 100..=15_000;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    match args.next().as_deref().and_then(OsStr::to_str) {
        Some("serve") => match flags(
            args,
            [
                "--config",
                "--deliver-tick-ms",
                "--lease-ttl-ms",
                "--retry-delay-ms",
            ],
        ) {
            Some([config, tick, ttl, delay]) => match Settings::read(config.as_deref()) {
                Ok(settings) => match pace_of(settings.deliver.is_some(), [&tick, &ttl, &delay]) {
                    Some(pace) => fail(&serve(&settings, pace)),
                    None => usage(),
                },
                Err(e) => fail(&e),
            },
            _ => usage(),
        },
        Some("status") => match flags(args, ["--config"]) {
            Some([config]) => status(config.as_deref()),
            None => usage(),
        },
        Some("reconcile") => {
            let id = args.next();
            match (id, flags(args, ["--config", "--outcome", "--note"])) {
                (Some(id), Some([config, outcome, note])) => {
                    run_reconcile(&id, config.as_deref(), outcome.as_deref(), note.as_deref())
                }
                _ => usage(),
            }
        }
        Some("shadow") => match flags(args, ["--live-root", "--state", "--out"]) {
            Some([Some(live_root), Some(state), Some(out)]) => run_shadow(&live_root, &state, &out),
            _ => usage(),
        },
        Some("--help" | "-h") => match std::io::stdout().lock().write_all(USAGE.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(1),
        },
        _ => usage(),
    }
}

/// What `serve` reads from `ulei.toml`.
struct Settings {
    /// `inbox.root`.
    root: PathBuf,
    /// `inbox.socket`.
    socket: PathBuf,
    /// `engine.c6_socket` when `inbox.deliver` is true.
    deliver: Option<PathBuf>,
}

impl Settings {
    /// The settings of the file `flag` names, else of the one [`Config::read`] finds; an error
    /// is one line naming the file or the key.
    fn read(flag: Option<&Path>) -> Result<Self, String> {
        let config = Config::read(flag).map_err(|e| e.to_string())?;
        let path = |key: &str| string(&config, key).map(PathBuf::from);
        let root = path("inbox.root")?;
        let socket = path("inbox.socket")?;
        let deliver = match config.get("inbox.deliver").map(|e| &e.value) {
            Some(Value::Bool(true)) => Some(path("engine.c6_socket")?),
            Some(Value::Bool(false)) | None => None,
            Some(_) => return Err("inbox.deliver must be true or false".to_owned()),
        };
        Ok(Self {
            root,
            socket,
            deliver,
        })
    }
}

/// The status of `inbox.socket`, `engine.c6_socket`, `engine.owner_session` and `qb.socket` of
/// the file `flag` names, else of the one [`Config::read`] finds; exit 1 when one of `inbox`,
/// `engine`, `channel` and `qb` is not up, 2 with nothing probed when the config is unusable;
/// the `hive-mcp` line never changes it.
fn status(flag: Option<&Path>) -> ExitCode {
    let settings = Config::read(flag)
        .map_err(|e| e.to_string())
        .and_then(|config| {
            Ok(status::Settings {
                inbox: string(&config, "inbox.socket")?.into(),
                engine: string(&config, "engine.c6_socket")?.into(),
                owner_session: string(&config, "engine.owner_session")?,
                qb: string(&config, "qb.socket")?.into(),
                hive_mcp: status::HiveMcp::of(&config),
            })
        });
    let settings = match settings {
        Ok(settings) => settings,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "ulei-inbox: {e}");
            return ExitCode::from(USAGE_ERROR);
        }
    };
    let report = match own_uid().and_then(|uid| status::probe(&settings, uid)) {
        Ok(report) => report,
        Err(e) => return fail(&format!("status: {e}")),
    };
    let mut out = std::io::stdout().lock();
    if let Err(e) = out
        .write_all(report.text.as_bytes())
        .and_then(|()| out.flush())
    {
        return fail(&format!("stdout: {e}"));
    }
    if report.up {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Closes the `uncertain` event `id` through the daemon of the config `flag` names: exit 0 and
/// the confirmation line; 2 with one stderr line and nothing sent when an argument or the config
/// is unusable; 1 with one stderr line when the inbox refused or could not be reached.
fn run_reconcile(
    id: &OsStr,
    flag: Option<&Path>,
    outcome: Option<&Path>,
    note: Option<&Path>,
) -> ExitCode {
    let refuse = |reason: &str| {
        let _ = writeln!(std::io::stderr(), "ulei-inbox reconcile: {reason}");
        ExitCode::from(USAGE_ERROR)
    };
    let (Some(outcome), Some(note)) = (outcome, note) else {
        return refuse("--outcome reconciled-delivered and --note TEXT are required");
    };
    let (Some(id), Some(outcome), Some(note)) = (id.to_str(), outcome.to_str(), note.to_str())
    else {
        return refuse("the id, --outcome and --note must be UTF-8");
    };
    let request = match reconcile::request(id, outcome, note) {
        Ok(request) => request,
        Err(reason) => return refuse(&reason),
    };
    let socket = match Config::read(flag)
        .map_err(|e| e.to_string())
        .and_then(|config| string(&config, "inbox.socket"))
    {
        Ok(socket) => PathBuf::from(socket),
        Err(e) => return refuse(&e),
    };
    let audit = match own_uid()
        .map_err(|e| format!("effective uid: {e}"))
        .and_then(|uid| reconcile::send(&socket, &request, uid))
    {
        Ok(audit) => audit,
        Err(reason) => {
            let _ = writeln!(std::io::stderr(), "ulei-inbox reconcile: {id}: {reason}");
            return ExitCode::from(1);
        }
    };
    let mut out = std::io::stdout().lock();
    match writeln!(out, "{}", reconcile::confirmation(&audit)).and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&format!("stdout: {e}")),
    }
}

/// The pace of the values of `--deliver-tick-ms`, `--lease-ttl-ms` and `--retry-delay-ms`,
/// [`Pace::DEFAULT`] for each one absent; `None` when one is given without `deliver` or is not
/// a number in its range.
fn pace_of(deliver: bool, [tick, ttl, delay]: [&Option<PathBuf>; 3]) -> Option<Pace> {
    if !deliver && (tick.is_some() || ttl.is_some() || delay.is_some()) {
        return None;
    }
    let mut pace = Pace::DEFAULT;
    if let Some(value) = tick {
        pace.tick = ms_of(value, &TICK_MS)?;
    }
    if let Some(value) = ttl {
        pace.lease_ttl = ms_of(value, &LEASE_TTL_MS)?.as_secs_f64();
    }
    if let Some(value) = delay {
        pace.retry_delay = ms_of(value, &RETRY_DELAY_MS)?.as_secs_f64();
    }
    Some(pace)
}

/// The milliseconds of `value`, or `None` for a value that is not a number in `range`.
fn ms_of(value: &Path, range: &RangeInclusive<u64>) -> Option<Duration> {
    let ms = value.to_str()?.parse().ok()?;
    range.contains(&ms).then(|| Duration::from_millis(ms))
}

fn fail(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "ulei-inbox: {message}");
    ExitCode::from(1)
}

/// One shadow pass; prints the counts, or exits [`RETRY_LATER`] or 1 with the failure.
fn run_shadow(live_root: &Path, state: &Path, out: &Path) -> ExitCode {
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs_f64(),
        Err(e) => return fail(&format!("clock: {e}")),
    };
    match shadow::run(live_root, state, out, now) {
        Ok(pass) => {
            let mut stdout = std::io::stdout().lock();
            match writeln!(
                stdout,
                "ulei-inbox shadow: {} rows, {} divergences",
                pass.rows, pass.divergences
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(&format!("stdout: {e}")),
            }
        }
        Err(e @ Failure::Retry(_)) => {
            let _ = writeln!(std::io::stderr(), "ulei-inbox shadow: {e}");
            ExitCode::from(RETRY_LATER)
        }
        Err(e) => fail(&format!("shadow: {e}")),
    }
}

/// Opens the store and, with `deliver`, the consumer of the root at `pace`; binds the socket,
/// starts the `deliver` thread, prints the ready line and serves until accept fails.
fn serve(settings: &Settings, pace: Pace) -> String {
    let (root, socket) = (settings.root.as_path(), settings.socket.as_path());
    let inbox = match Inbox::open(root) {
        Ok(inbox) => inbox,
        Err(e) => return format!("{}: {e}", root.display()),
    };
    let uid = match own_uid() {
        Ok(uid) => uid,
        Err(e) => return format!("effective uid: {e}"),
    };
    let consumer = match settings
        .deliver
        .as_deref()
        .map(|engine| consumer(root, engine, uid, pace))
    {
        Some(Ok(consumer)) => consumer,
        Some(Err(e)) => return e,
        None => None,
    };
    let listener = match bind(socket) {
        Ok(listener) => listener,
        Err(e) => return format!("{}: {e}", socket.display()),
    };
    let mut server = Server::new(inbox, uid, READ_TIMEOUT);
    if let Some(mut consumer) = consumer {
        // One slot: publishes that arrive while the loop is busy wake it once.
        let (published, wake) = mpsc::sync_channel(1);
        // Writers of the root that skip the C1 socket wake the loop through the watch, or
        // within the poll when the root cannot be watched (ADR-0084).
        match deliver::watch(root) {
            Ok(watch) => {
                let written = published.clone();
                let spawned = thread::Builder::new()
                    .name("watch".to_owned())
                    .spawn(move || watch.run(&written));
                if let Err(e) = spawned {
                    return format!("watch thread: {e}");
                }
            }
            Err(e) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "ulei-inbox: owner inbox watch off ({}); polling every {} s",
                    e.kind(),
                    deliver::POLL.as_secs()
                );
                consumer = consumer.polling(deliver::POLL);
            }
        }
        let spawned = thread::Builder::new()
            .name("deliver".to_owned())
            .spawn(move || consumer.run(&wake));
        if let Err(e) = spawned {
            return format!("deliver thread: {e}");
        }
        server = server.signal_publish(published);
    }
    let mut out = std::io::stdout().lock();
    if let Err(e) =
        writeln!(out, "ulei-inbox ready {}", socket.display()).and_then(|()| out.flush())
    {
        return format!("stdout: {e}");
    }
    drop(out);
    let e = server.run(&listener);
    format!("accept: {e}")
}

/// The consumer of `root` at `pace` on the engine at `engine`, or `None` after the fixed line of
/// why the loop does not run.
fn consumer(root: &Path, engine: &Path, uid: u32, pace: Pace) -> Result<Option<Consumer>, String> {
    let client = Client::with_limits(engine, CONNECT_TIMEOUT, CALL_TIMEOUT, uid);
    match deliver::open(root, client, pace) {
        Ok(Ok(consumer)) => Ok(Some(consumer)),
        Ok(Err(off)) => {
            let _ = writeln!(std::io::stderr(), "{}", off.line());
            Ok(None)
        }
        Err(e) => Err(format!("{}: {e}", root.display())),
    }
}

fn usage() -> ExitCode {
    let _ = std::io::stderr().write_all(USAGE.as_bytes());
    ExitCode::from(USAGE_ERROR)
}
