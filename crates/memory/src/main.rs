//! `ulei-memory serve [--config PATH]`: the archive daemon (ADR-0051 sections 2 and 3), which pulls
//! the engine's dialogue once before it reports ready; a refused config exits 2, any other failure
//! to start 1.
//! `ulei-memory delete <id> --reason <text> [--config PATH]`: the owner's command that removes an
//! item and leaves its audit row. Every outcome of `delete` and every failure to start `serve` is
//! one stderr line.
//! `ulei-memory import [--skip N] [--config PATH]`: puts each line of stdin, a C10 `PutRequest`,
//! through the daemon's socket; its counts are the last stderr line.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ulei_memory::archive::{Archive, Caller};
use ulei_memory::import::{self, Client};
use ulei_memory::pull::Engine;
use ulei_memory::serve::{self as daemon, Server, Settings, TIMEOUT, bind};

const USAGE: &str = "usage: ulei-memory serve [--config PATH]\n       \
                     ulei-memory delete <id> --reason <text> [--config PATH]\n       \
                     ulei-memory import [--skip N] [--config PATH] < items.jsonl\n";

enum Command {
    Serve(Option<PathBuf>),
    Delete {
        id: String,
        reason: String,
        flag: Option<PathBuf>,
    },
    Import {
        skip: u64,
        flag: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let line = match command(&args) {
        Some(Command::Serve(flag)) => match Settings::read(flag.as_deref()) {
            Ok(settings) => serve(&settings),
            // A refused config is no failure a restart fixes (ADR-0070).
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "memory: {e}");
                return ExitCode::from(2);
            }
        },
        Some(Command::Import { skip, flag }) => return run_import(flag.as_deref(), skip),
        Some(Command::Delete { id, reason, flag }) => match delete(flag.as_deref(), &id, &reason) {
            Ok(line) => {
                let _ = writeln!(std::io::stderr(), "memory: {line}");
                return ExitCode::SUCCESS;
            }
            Err(line) => line,
        },
        None if matches!(args.as_slice(), [h] if h == "--help" || h == "-h") => {
            return match std::io::stdout().lock().write_all(USAGE.as_bytes()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::from(1),
            };
        }
        None => {
            let _ = std::io::stderr().write_all(USAGE.as_bytes());
            return ExitCode::from(2);
        }
    };
    let _ = writeln!(std::io::stderr(), "memory: {line}");
    ExitCode::from(1)
}

/// The command `args` name; `None` for anything else, a non-UTF-8 id or reason included.
fn command(args: &[OsString]) -> Option<Command> {
    if let [import, rest @ ..] = args
        && import == "import"
    {
        return import_command(rest);
    }
    let (args, flag) = match args {
        [rest @ .., config, path] if config == "--config" => (rest, Some(PathBuf::from(path))),
        _ => (args, None),
    };
    match args {
        [serve] if serve == "serve" => Some(Command::Serve(flag)),
        [delete, id, reason_flag, reason] if delete == "delete" && reason_flag == "--reason" => {
            Some(Command::Delete {
                id: id.to_str()?.to_owned(),
                reason: reason.to_str()?.to_owned(),
                flag,
            })
        }
        _ => None,
    }
}

/// `import`'s flags `--skip N` and `--config PATH`, each at most once, in any order.
fn import_command(mut args: &[OsString]) -> Option<Command> {
    let (mut skip, mut flag) = (None, None);
    while let [name, value, rest @ ..] = args {
        if name == "--skip" && skip.is_none() {
            skip = Some(value.to_str()?.parse::<u64>().ok()?);
        } else if name == "--config" && flag.is_none() {
            flag = Some(PathBuf::from(value));
        } else {
            return None;
        }
        args = rest;
    }
    args.is_empty().then(|| Command::Import {
        skip: skip.unwrap_or(0),
        flag,
    })
}

/// Puts stdin's lines after the first `skip` through the configured socket; exit 0 when the
/// input was read to its end, 1 when the config is refused or the import stopped.
fn run_import(flag: Option<&Path>, skip: u64) -> ExitCode {
    let mut err = std::io::stderr();
    let socket = match daemon::socket(flag) {
        Ok(socket) => socket,
        Err(e) => {
            let _ = writeln!(err, "import: {e}");
            return ExitCode::from(1);
        }
    };
    let client = Client::new(socket);
    let mut input = std::io::stdin().lock();
    match import::import(&mut input, skip, &client, &mut err) {
        Ok(counts) => {
            let _ = writeln!(err, "import: {counts}");
            ExitCode::SUCCESS
        }
        Err((counts, stopped)) => {
            let _ = writeln!(err, "import: {stopped}");
            let _ = writeln!(err, "import: {counts}");
            ExitCode::from(1)
        }
    }
}

/// Starts the daemon and serves until `accept` fails; the line saying why it stopped.
fn serve(settings: &Settings) -> String {
    let uid = rustix::process::geteuid().as_raw();
    let archive = match Archive::open(&settings.archive, uid) {
        Ok(archive) => archive,
        Err(e) => return format!("archive {}: {e}", settings.archive.display()),
    };
    let listener = match bind(&settings.socket, uid) {
        Ok(listener) => listener,
        Err(e) => return format!("{}: {e}", settings.socket.display()),
    };
    let engine = Engine::new(settings.engine.clone(), TIMEOUT);
    let mut server = Server::new(archive, engine, uid, TIMEOUT);
    server.pull();
    let mut out = std::io::stdout().lock();
    if let Err(e) =
        writeln!(out, "ulei-memory ready {}", settings.socket.display()).and_then(|()| out.flush())
    {
        return format!("stdout: {e}");
    }
    drop(out);
    let e = server.run(&listener);
    format!("accept: {e}")
}

/// Deletes the item `id` of the configured archive as this process; the line naming what was
/// removed, or why nothing was.
fn delete(flag: Option<&Path>, id: &str, reason: &str) -> Result<String, String> {
    let settings = Settings::read(flag)?;
    let path = &settings.archive;
    // Opening a missing archive would create one.
    fs::metadata(path).map_err(|e| format!("archive {}: {e}", path.display()))?;
    let mut archive = Archive::open(path, rustix::process::geteuid().as_raw())
        .map_err(|e| format!("archive {}: {e}", path.display()))?;
    let caller = Caller {
        pid: u64::from(std::process::id()),
        uid: u64::from(rustix::process::getuid().as_raw()),
    };
    let deletion = archive
        .delete(id, reason, caller)
        .map_err(|e| format!("delete {id}: {e}"))?;
    Ok(format!(
        "deleted {id}: {} bytes, {} entries; the audit row keeps the reason, pid {} and uid {}",
        deletion.len,
        deletion.entries.len(),
        deletion.pid,
        deletion.uid
    ))
}
