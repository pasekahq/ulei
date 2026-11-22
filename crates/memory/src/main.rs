//! `ulei-memory serve [--config PATH]`: the archive daemon (ADR-0051 section 2).
//! `ulei-memory delete <id> --reason <text> [--config PATH]`: the owner's command that removes an
//! item and leaves its audit row. Every outcome of `delete` and every failure to start `serve` is
//! one stderr line.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ulei_memory::archive::{Archive, Caller};
use ulei_memory::serve::{Server, Settings, TIMEOUT, bind};

const USAGE: &str = "usage: ulei-memory serve [--config PATH]\n       \
                     ulei-memory delete <id> --reason <text> [--config PATH]\n";

enum Command {
    Serve(Option<PathBuf>),
    Delete {
        id: String,
        reason: String,
        flag: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let line = match command(&args) {
        Some(Command::Serve(flag)) => serve(flag.as_deref()),
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

/// Starts the daemon and serves until `accept` fails; the line saying why it stopped.
fn serve(flag: Option<&Path>) -> String {
    let settings = match Settings::read(flag) {
        Ok(settings) => settings,
        Err(e) => return e,
    };
    let archive = match Archive::open(&settings.archive) {
        Ok(archive) => archive,
        Err(e) => return format!("archive {}: {e}", settings.archive.display()),
    };
    let uid = rustix::process::geteuid().as_raw();
    let listener = match bind(&settings.socket, uid) {
        Ok(listener) => listener,
        Err(e) => return format!("{}: {e}", settings.socket.display()),
    };
    let mut out = std::io::stdout().lock();
    if let Err(e) =
        writeln!(out, "ulei-memory ready {}", settings.socket.display()).and_then(|()| out.flush())
    {
        return format!("stdout: {e}");
    }
    drop(out);
    let e = Server::new(archive, uid, TIMEOUT).run(&listener);
    format!("accept: {e}")
}

/// Deletes the item `id` of the configured archive as this process; the line naming what was
/// removed, or why nothing was.
fn delete(flag: Option<&Path>, id: &str, reason: &str) -> Result<String, String> {
    let settings = Settings::read(flag)?;
    let path = &settings.archive;
    // Opening a missing archive would create one.
    fs::metadata(path).map_err(|e| format!("archive {}: {e}", path.display()))?;
    let mut archive =
        Archive::open(path).map_err(|e| format!("archive {}: {e}", path.display()))?;
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
