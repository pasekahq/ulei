//! `ulei-inbox`: everything that opens `events.sqlite3` (ADR-0007): the `serve` daemon and
//! the `shadow` pass.

#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ulei_core::c6::{CALL_TIMEOUT, CONNECT_TIMEOUT, Client};
use ulei_core::deliver::{self, Consumer, Pace};
use ulei_core::inbox::shadow::{self, Failure};
use ulei_core::inbox::{Inbox, own_uid};
use ulei_core::serve::{READ_TIMEOUT, Server, bind};

const USAGE: &str = "usage: ulei-inbox serve --root DIR --socket PATH [--deliver C6_SOCKET \
[--deliver-tick-ms N] [--lease-ttl-ms N] [--retry-delay-ms N]]
       ulei-inbox shadow --live-root DIR --state DIR --out FILE.jsonl
";
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
                "--root",
                "--socket",
                "--deliver",
                "--deliver-tick-ms",
                "--lease-ttl-ms",
                "--retry-delay-ms",
            ],
        ) {
            Some([Some(root), Some(socket), deliver, tick, ttl, delay]) => {
                match pace_of(deliver.is_some(), [&tick, &ttl, &delay]) {
                    Some(pace) => fail(&serve(&root, &socket, deliver.as_deref(), pace)),
                    None => usage(),
                }
            }
            _ => usage(),
        },
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

/// The values of the flags `names` in that order, each given at most once; `None` for any
/// other argument.
fn flags<const N: usize>(
    mut args: impl Iterator<Item = OsString>,
    names: [&str; N],
) -> Option<[Option<PathBuf>; N]> {
    let mut values: [Option<PathBuf>; N] = std::array::from_fn(|_| None);
    while let Some(flag) = args.next() {
        let slot = names.iter().position(|name| flag.to_str() == Some(name))?;
        if values[slot].replace(PathBuf::from(args.next()?)).is_some() {
            return None;
        }
    }
    Some(values)
}

/// The pace of the values of `--deliver-tick-ms`, `--lease-ttl-ms` and `--retry-delay-ms`,
/// [`Pace::DEFAULT`] for each one absent; `None` when one is given without `--deliver` or is
/// not a number in its range.
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
fn serve(root: &Path, socket: &Path, deliver: Option<&Path>, pace: Pace) -> String {
    let inbox = match Inbox::open(root) {
        Ok(inbox) => inbox,
        Err(e) => return format!("{}: {e}", root.display()),
    };
    let uid = match own_uid() {
        Ok(uid) => uid,
        Err(e) => return format!("effective uid: {e}"),
    };
    let consumer = match deliver.map(|engine| consumer(root, engine, uid, pace)) {
        Some(Ok(consumer)) => consumer,
        Some(Err(e)) => return e,
        None => None,
    };
    let listener = match bind(socket) {
        Ok(listener) => listener,
        Err(e) => return format!("{}: {e}", socket.display()),
    };
    let mut server = Server::new(inbox, uid, READ_TIMEOUT);
    if let Some(consumer) = consumer {
        // One slot: publishes that arrive while the loop is busy wake it once.
        let (published, wake) = mpsc::sync_channel(1);
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
    ExitCode::from(2)
}
