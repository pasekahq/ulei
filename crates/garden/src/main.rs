//! `ulei-garden serve [--config PATH]`: the garden daemon, the one process that opens the live
//! store (ADR-0047 section 2). Every failure to start is one stderr line: exit 2 when the config
//! is refused (no restart fixes it, ADR-0075), else exit 1, a producer key file that does not
//! exist yet included.
//!
//! `ulei-garden places import FILE [--dry-run] [--config PATH]`: a client of the daemon's socket
//! that writes a places file into the bed (ADR-0080 section 4). It prints one JSON object on
//! stdout: the report, exit 0; or an error, every refused line included, exit 1.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use serde_json::Value;

use ulei_garden::embed::supervisor::Supervisor;
use ulei_garden::miss::Inbox;
use ulei_garden::places_import::{self, ImportError, Socket};
use ulei_garden::profile::Profile;
use ulei_garden::serve::{EmbedderSettings, Server, Settings, TIMEOUT, bind, open};
use ulei_garden::store::Store;
use ulei_garden::worker::{self, Clock, SystemClock, Wake, Worker};

const USAGE: &str = "usage: ulei-garden serve [--config PATH]\n       ulei-garden places import FILE [--dry-run] [--config PATH]\n";

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if let [places, import, rest @ ..] = args.as_slice()
        && places == "places"
        && import == "import"
    {
        if let Some((file, dry_run, flag)) = import_args(rest) {
            return places_import(&file, dry_run, flag.as_deref());
        }
        let _ = std::io::stderr().write_all(USAGE.as_bytes());
        return ExitCode::from(2);
    }
    let flag = match args.as_slice() {
        [command] if command == "serve" => None,
        [command, flag, path] if command == "serve" && flag == "--config" => {
            Some(PathBuf::from(path))
        }
        [help] if help == "--help" || help == "-h" => {
            return match std::io::stdout().lock().write_all(USAGE.as_bytes()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::from(1),
            };
        }
        _ => {
            let _ = std::io::stderr().write_all(USAGE.as_bytes());
            return ExitCode::from(2);
        }
    };
    let settings = match Settings::read(flag.as_deref()) {
        Ok(settings) => settings,
        Err(refusal) => {
            let _ = writeln!(std::io::stderr(), "garden: {refusal}");
            // A refused config is no failure a restart fixes; a producer key the inbox has not
            // created yet is (ADR-0075).
            return ExitCode::from(if refusal.lasting { 2 } else { 1 });
        }
    };
    let line = serve(&settings);
    let _ = writeln!(std::io::stderr(), "garden: {line}");
    ExitCode::from(1)
}

/// The file, `--dry-run` and the `--config` path of `places import`, in any order.
fn import_args(args: &[OsString]) -> Option<(PathBuf, bool, Option<PathBuf>)> {
    let (mut file, mut dry_run, mut flag) = (None, false, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--dry-run" && !dry_run {
            dry_run = true;
        } else if arg == "--config" && flag.is_none() {
            flag = Some(PathBuf::from(args.next()?));
        } else if file.is_none() && !arg.to_string_lossy().starts_with("--") {
            file = Some(PathBuf::from(arg));
        } else {
            return None;
        }
    }
    Some((file?, dry_run, flag))
}

/// Checks the places file `file` and, unless `dry_run`, writes it through the socket of the
/// config `flag` names; prints the one JSON answer.
fn places_import(file: &Path, dry_run: bool, flag: Option<&Path>) -> ExitCode {
    let (code, value) = match import(file, dry_run, flag) {
        Ok((clean, report)) => (u8::from(!clean), report),
        Err(e) => (1, e.to_json()),
    };
    let mut out = std::io::stdout().lock();
    match writeln!(out, "{value}").and_then(|()| out.flush()) {
        Ok(()) => ExitCode::from(code),
        Err(_) => ExitCode::from(1),
    }
}

/// The report of the dry run or the write, and whether the daemon wrote every line.
fn import(file: &Path, dry_run: bool, flag: Option<&Path>) -> Result<(bool, Value), ImportError> {
    let failed = |m: String| ImportError::Failed(m);
    let text =
        std::fs::read_to_string(file).map_err(|e| failed(format!("{}: {e}", file.display())))?;
    let name = file
        .file_name()
        .ok_or_else(|| failed(format!("{}: no file name", file.display())))?
        .to_string_lossy();
    let plan = places_import::check(&text, &name, SystemClock.now())?;
    if dry_run {
        return Ok((true, plan.counts()));
    }
    let socket = ulei_garden::serve::socket(flag).map_err(failed)?;
    let written = places_import::write(&plan, &mut Socket(socket))?;
    Ok((written.refused.is_empty(), written.to_json()))
}

/// Starts the daemon with `settings` and serves until `accept` fails; the line saying why it
/// stopped.
fn serve(settings: &Settings) -> String {
    let wake = Arc::new(Wake::default());
    // The content check of the engine's weight file (ADR-0086 open question 15); none for ONNX.
    let check = settings.embedder.as_ref().and_then(EmbedderSettings::check);
    let helper = match &settings.embedder {
        None => None,
        Some(embedder) => {
            let start = embedder.clone();
            match Supervisor::start(move || start.start(), Arc::clone(&wake)) {
                Ok(helper) => Some(Arc::new(helper)),
                Err(e) => return format!("embedder: {}: {e}", embedder.helper.display()),
            }
        }
    };
    let garden = match open(settings, helper.clone()) {
        Ok(garden) => garden,
        Err(e) => return format!("store {}: {e}", settings.store.display()),
    };
    let profile = settings
        .core
        .as_ref()
        .map(|core| Arc::new(Profile::new(core.file.clone(), core.char_limit)));
    if let Some(profile) = &profile
        && let Err(e) = profile.render(garden.store(), SystemClock.now())
    {
        return format!("core profile {}: {e}", profile.file().display());
    }
    let uid = rustix::process::geteuid().as_raw();
    let listener = match bind(&settings.socket, uid) {
        Ok(listener) => listener,
        Err(e) => return format!("{}: {e}", settings.socket.display()),
    };
    let inbox = Inbox::new(settings.inbox_socket.clone(), settings.producer_key.clone());
    let background = match Store::open(&settings.store).and_then(|mut s| {
        s.set_ttl_table(settings.ttl.clone());
        let worker = Worker::new(
            s,
            settings.schedule,
            SystemClock,
            inbox,
            settings.second_level.source(),
            settings.api.edge,
        )?;
        let worker = match helper {
            Some(helper) => worker.with_helper(helper),
            None => worker,
        };
        let worker = match check {
            Some(check) => worker.with_check(check),
            None => worker,
        };
        Ok(match &profile {
            Some(profile) => worker.with_profile(Arc::clone(profile)),
            None => worker,
        })
    }) {
        Ok(background) => background,
        Err(e) => return format!("store {}: {e}", settings.store.display()),
    };
    if let Err(e) = worker::spawn(background, Arc::clone(&wake)) {
        return format!("worker: {e}");
    }
    let mut out = std::io::stdout().lock();
    if let Err(e) =
        writeln!(out, "ulei-garden ready {}", settings.socket.display()).and_then(|()| out.flush())
    {
        return format!("stdout: {e}");
    }
    drop(out);
    let server = Server::new(garden, uid, TIMEOUT, wake, SystemClock);
    let mut server = match profile {
        Some(profile) => server.with_profile(profile),
        None => server,
    };
    let e = server.run(&listener);
    format!("accept: {e}")
}
