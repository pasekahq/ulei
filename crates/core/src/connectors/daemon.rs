//! `ulei-connectors serve` (ADR-0056 section 4): the schedule of the connected connectors and
//! of C10 `sync` on the wall clock, each run on its own thread, a record per run, and the stop
//! of the running units on SIGTERM or SIGINT. The daemon sleeps until the nearest due time or
//! the end of a run.

use std::io::{self, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use signal_hook::consts::{SIGINT, SIGTERM};
use ulei_contracts::config::{Config, Value};
use ulei_contracts::generated::c10::SyncResponse;

use super::schedule::{Action, Schedule};
use super::{Connector, Paths, Report, Runs, append, busy, cursor, owner_grants, put, run};
use crate::serve::now;

/// Seconds between `sync` calls when `connectors.sync_every` is unset.
const SYNC_EVERY: u32 = 900;

/// What wakes the daemon besides a due time.
pub enum Wake {
    /// A run of this connector ended, or could not read the clock.
    Ended(String, Result<Box<Report>, String>),
    /// A `sync` call returned.
    Synced(Result<SyncResponse, String>),
    /// SIGTERM or SIGINT.
    Stop,
}

/// Sends [`Wake::Stop`] to `wake` on SIGTERM and SIGINT: the handler writes to a socket pair
/// whose reader thread sleeps in `read`.
///
/// # Errors
/// The pair, a handler or the thread cannot be set up.
pub fn stop_on_signals(wake: Sender<Wake>) -> io::Result<()> {
    let (mut reader, writer) = UnixStream::pair()?;
    signal_hook::low_level::pipe::register(SIGTERM, writer.try_clone()?)?;
    signal_hook::low_level::pipe::register(SIGINT, writer)?;
    thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let mut byte = [0; 1];
            loop {
                match reader.read(&mut byte) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if wake.send(Wake::Stop).is_err() {
                            return;
                        }
                    }
                }
            }
        })?;
    Ok(())
}

/// `connectors.sync_every`, seconds between `sync` calls, 900 when unset.
///
/// # Errors
/// It is not an integer from 1 to `u32::MAX`.
pub fn sync_every(config: &Config) -> Result<u32, String> {
    const KEY: &str = "connectors.sync_every";
    match config.get(KEY).map(|e| &e.value) {
        None => Ok(SYNC_EVERY),
        Some(Value::Integer(n)) => u32::try_from(*n)
            .ok()
            .filter(|&n| n > 0)
            .ok_or_else(|| format!("{KEY} {n} is not a positive number of seconds")),
        Some(_) => Err(format!("{KEY} must be an integer")),
    }
}

/// One stderr line of the runner.
pub fn say(line: &str) {
    let _ = writeln!(io::stderr(), "ulei-connectors: {line}");
}

/// Schedules `connectors` and `sync` every `sync_every` seconds, writes the ready line once the
/// schedule is set, runs until [`Wake::Stop`] arrives on `wakes`, appends each run's record to
/// `log`, then stops the running units, waits for their records and returns.
///
/// # Errors
/// The clock is before the Unix epoch, or every sender of `wakes` is gone.
pub fn serve(
    config: &Config,
    paths: &Paths,
    log: &Path,
    connectors: &[Connector],
    sync_every: u32,
    uid: u32,
    (sender, wakes): (Sender<Wake>, Receiver<Wake>),
) -> Result<(), String> {
    let clock = |e| format!("clock: {e}");
    let mut schedule = Schedule::new(f64::from(sync_every), now().map_err(clock)?);
    for connector in connectors {
        let state = cursor::read(&paths.state.join(&connector.name)).unwrap_or_else(|reason| {
            // Its run refuses with this reason; it is scheduled so that the refusal is logged.
            say(&format!("{}: {reason}", connector.name));
            cursor::State::default()
        });
        schedule.add(
            &connector.name,
            seconds(connector.manifest.schedule.every),
            state.last_start,
            state.not_before,
        );
    }
    say(&format!(
        "ready: {} connectors, sync every {sync_every} s",
        connectors.len()
    ));
    let runs = Runs::default();
    let mut stopping = false;
    loop {
        let at = now().map_err(clock)?;
        if !stopping {
            for action in schedule.due(at) {
                match action {
                    Action::Start(name) => {
                        let Some(connector) = connectors.iter().find(|c| c.name == name) else {
                            continue;
                        };
                        let (config, paths, connector) =
                            (config.clone(), paths.clone(), connector.clone());
                        let (sender, runs) = (sender.clone(), runs.clone());
                        thread::spawn(move || {
                            let report = run(&config, &paths, &connector, uid, &runs).map(Box::new);
                            let _ = sender.send(Wake::Ended(connector.name, report));
                        });
                    }
                    Action::Busy(name) => {
                        record(log, &busy(&name, at, owner_grants(config, &name)));
                    }
                    Action::Sync => {
                        let (archive, sender) = (paths.archive.clone(), sender.clone());
                        thread::spawn(move || {
                            let _ = sender.send(Wake::Synced(put::sync(&archive, uid)));
                        });
                    }
                }
            }
        } else if schedule.running() == 0 {
            return Ok(());
        }
        let wake = if stopping {
            wakes.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            let wait = (schedule.next() - now().map_err(clock)?).max(0.0);
            wakes.recv_timeout(Duration::from_secs_f64(wait))
        };
        match wake {
            Ok(Wake::Ended(name, Ok(report))) => {
                record(log, &report);
                schedule.finished(&name, report.not_before);
            }
            Ok(Wake::Ended(name, Err(reason))) => {
                say(&format!("{name}: {reason}"));
                schedule.finished(&name, None);
            }
            Ok(Wake::Synced(answer)) => {
                schedule.synced();
                match answer {
                    Ok(SyncResponse { synced: true, .. }) => {}
                    Ok(SyncResponse { put, synced: false }) => say(&format!(
                        "sync: put {put}, synced false; the archive did not reach the engine"
                    )),
                    Err(reason) => say(&format!("sync: {reason}")),
                }
            }
            Ok(Wake::Stop) if !stopping => {
                stopping = true;
                say(&format!("stopping: {} runs in flight", schedule.running()));
                for failed in runs.stop_all() {
                    say(&failed);
                }
            }
            Ok(Wake::Stop) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err("the wake channel closed".to_owned());
            }
        }
    }
}

/// Prints the run's line and appends its record to `log`; a log failure is one more line.
fn record(log: &Path, report: &Report) {
    say(&report.line());
    if let Err(e) = append(log, &report.record) {
        say(&format!("connectors.log {}: {e}", log.display()));
    }
}

/// `n` seconds as a float, saturating at `u32::MAX`.
pub(super) fn seconds(n: u64) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}
