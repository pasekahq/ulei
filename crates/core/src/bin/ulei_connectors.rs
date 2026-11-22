//! `ulei-connectors`: the source connectors' runner (ADR-0056). `list` shows the connected
//! connectors, `run` runs one now as a child over pipes, `reset` drops one's cursor.

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ulei_contracts::config::Config;
use ulei_contracts::generated::c11::RunOutcome;
use ulei_core::cli::flags;
use ulei_core::config::string;
use ulei_core::connectors::{self, Discovery, Paths};
use ulei_core::inbox::own_uid;

const USAGE: &str = "usage: ulei-connectors list [--config PATH]
       ulei-connectors run NAME [--config PATH]
       ulei-connectors reset NAME [--config PATH]
";
/// Exit status of a usage error, an unusable config or a connector that cannot run.
const USAGE_ERROR: u8 = 2;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    match args.next().as_deref().and_then(OsStr::to_str) {
        Some("list") => match flags(args, ["--config"]) {
            Some([config]) => list(config.as_deref()),
            None => usage(),
        },
        Some(command @ ("run" | "reset")) => {
            let Some(name) = args.next().and_then(|n| n.into_string().ok()) else {
                return usage();
            };
            match (command, flags(args, ["--config"])) {
                ("run", Some([config])) => run(config.as_deref(), &name),
                (_, Some([config])) => reset(config.as_deref(), &name),
                (_, None) => usage(),
            }
        }
        Some("--help" | "-h") => match std::io::stdout().lock().write_all(USAGE.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(1),
        },
        _ => usage(),
    }
}

/// Prints `name transport every Ns` per connected connector and one stderr line per enabled
/// name that is not; exit 1 when any is not.
fn list(flag: Option<&Path>) -> ExitCode {
    let discovery = match Config::read(flag)
        .map_err(|e| e.to_string())
        .and_then(|config| found(&config))
    {
        Ok(discovery) => discovery,
        Err(e) => return refuse(&e),
    };
    let mut text = String::new();
    for connector in &discovery.connectors {
        let manifest = &connector.manifest;
        let _ = writeln!(
            text,
            "{} {} every {}s",
            connector.name,
            manifest.transport.as_str(),
            manifest.schedule.every
        );
    }
    let mut out = std::io::stdout().lock();
    if let Err(e) = out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        return fail(&format!("stdout: {e}"));
    }
    for (name, reason) in &discovery.refused {
        say(&format!("{name}: {reason}"));
    }
    if discovery.refused.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Runs `name` once, appends its run record to `connectors.log` and prints its line; exit 0
/// when the run ended `ok`.
fn run(flag: Option<&Path>, name: &str) -> ExitCode {
    let ready = Config::read(flag)
        .map_err(|e| e.to_string())
        .and_then(|config| {
            let discovery = found(&config)?;
            let log = PathBuf::from(string(&config, "connectors.log")?);
            let paths = Paths {
                state: string(&config, "connectors.state")?.into(),
                archive: string(&config, "memory.socket")?.into(),
            };
            Ok((config, discovery, log, paths))
        });
    let (config, discovery, log, paths) = match ready {
        Ok(ready) => ready,
        Err(e) => return refuse(&e),
    };
    let Some(connector) = discovery.connectors.iter().find(|c| c.name == name) else {
        return refuse(&match discovery.refused.iter().find(|(n, _)| n == name) {
            Some((_, reason)) => format!("{name}: {reason}"),
            None => format!("{name}: not in connectors.enabled"),
        });
    };
    let report = match own_uid()
        .map_err(|e| format!("uid: {e}"))
        .and_then(|uid| connectors::run(&config, &paths, connector, uid))
    {
        Ok(report) => report,
        Err(e) => return fail(&e),
    };
    say(&report.line());
    if let Err(e) = connectors::append(&log, &report.record) {
        return fail(&format!("connectors.log {}: {e}", log.display()));
    }
    if report.record.outcome == RunOutcome::Ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Drops the stored cursor of `name`.
fn reset(flag: Option<&Path>, name: &str) -> ExitCode {
    let state = match Config::read(flag)
        .map_err(|e| e.to_string())
        .and_then(|config| string(&config, "connectors.state"))
    {
        Ok(state) => PathBuf::from(state),
        Err(e) => return refuse(&e),
    };
    match connectors::reset(&state, name) {
        Ok(true) => say(&format!("{name}: cursor cleared")),
        Ok(false) => say(&format!("{name}: no cursor stored")),
        Err(e) => return fail(&e),
    }
    ExitCode::SUCCESS
}

/// Discovery over `connectors.dir` for `connectors.enabled`.
fn found(config: &Config) -> Result<Discovery, String> {
    let dir = PathBuf::from(string(config, "connectors.dir")?);
    let enabled = connectors::enabled(&string(config, "connectors.enabled")?);
    connectors::discover(&dir, &enabled)
}

fn say(line: &str) {
    let _ = writeln!(std::io::stderr(), "ulei-connectors: {line}");
}

fn usage() -> ExitCode {
    let _ = std::io::stderr().write_all(USAGE.as_bytes());
    ExitCode::from(USAGE_ERROR)
}

fn refuse(line: &str) -> ExitCode {
    say(line);
    ExitCode::from(USAGE_ERROR)
}

fn fail(line: &str) -> ExitCode {
    say(line);
    ExitCode::from(1)
}
