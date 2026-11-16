//! `ulei-inbox`: everything that opens `events.sqlite3` (ADR-0007): the `serve` daemon and
//! the `shadow` pass.

#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use ulei_core::inbox::shadow::{self, Failure};
use ulei_core::inbox::{Inbox, own_uid};
use ulei_core::serve::{READ_TIMEOUT, Server, bind};

const USAGE: &str = "usage: ulei-inbox serve --root DIR --socket PATH
       ulei-inbox shadow --live-root DIR --state DIR --out FILE.jsonl
";
/// Exit status of a shadow pass that met a busy or hot live database (`EX_TEMPFAIL`).
const RETRY_LATER: u8 = 75;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    match args.next().as_deref().and_then(OsStr::to_str) {
        Some("serve") => match flags(args, ["--root", "--socket"]) {
            Some([root, socket]) => fail(&serve(&root, &socket)),
            None => usage(),
        },
        Some("shadow") => match flags(args, ["--live-root", "--state", "--out"]) {
            Some([live_root, state, out]) => run_shadow(&live_root, &state, &out),
            None => usage(),
        },
        Some("--help" | "-h") => match std::io::stdout().lock().write_all(USAGE.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(1),
        },
        _ => usage(),
    }
}

/// The values of exactly the flags `names`, each given once, in that order.
fn flags<const N: usize>(
    mut args: impl Iterator<Item = OsString>,
    names: [&str; N],
) -> Option<[PathBuf; N]> {
    let mut values: [Option<PathBuf>; N] = std::array::from_fn(|_| None);
    while let Some(flag) = args.next() {
        let slot = names.iter().position(|name| flag.to_str() == Some(name))?;
        if values[slot].replace(PathBuf::from(args.next()?)).is_some() {
            return None;
        }
    }
    values
        .into_iter()
        .collect::<Option<Vec<_>>>()?
        .try_into()
        .ok()
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

/// Opens the store, binds the socket, prints the ready line and serves until accept fails.
fn serve(root: &Path, socket: &Path) -> String {
    let inbox = match Inbox::open(root) {
        Ok(inbox) => inbox,
        Err(e) => return format!("{}: {e}", root.display()),
    };
    let uid = match own_uid() {
        Ok(uid) => uid,
        Err(e) => return format!("effective uid: {e}"),
    };
    let listener = match bind(socket) {
        Ok(listener) => listener,
        Err(e) => return format!("{}: {e}", socket.display()),
    };
    let mut out = std::io::stdout().lock();
    if let Err(e) =
        writeln!(out, "ulei-inbox ready {}", socket.display()).and_then(|()| out.flush())
    {
        return format!("stdout: {e}");
    }
    drop(out);
    let e = Server::new(inbox, uid, READ_TIMEOUT).run(&listener);
    format!("accept: {e}")
}

fn usage() -> ExitCode {
    let _ = std::io::stderr().write_all(USAGE.as_bytes());
    ExitCode::from(2)
}
