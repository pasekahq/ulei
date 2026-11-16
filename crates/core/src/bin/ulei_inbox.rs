//! `ulei-inbox`: everything that opens `events.sqlite3` (ADR-0007), starting with the
//! `serve` daemon.

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ulei_core::inbox::{Inbox, own_uid};
use ulei_core::serve::{READ_TIMEOUT, Server, bind};

const USAGE: &str = "usage: ulei-inbox serve --root DIR --socket PATH\n";

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    match args.next().as_deref().and_then(OsStr::to_str) {
        Some("serve") => {}
        Some("--help" | "-h") => {
            return match std::io::stdout().lock().write_all(USAGE.as_bytes()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::from(1),
            };
        }
        _ => return usage(),
    }
    let (mut root, mut socket) = (None, None);
    while let Some(flag) = args.next() {
        let slot = match flag.to_str() {
            Some("--root") => &mut root,
            Some("--socket") => &mut socket,
            _ => return usage(),
        };
        let Some(value) = args.next() else {
            return usage();
        };
        if slot.replace(PathBuf::from(value)).is_some() {
            return usage();
        }
    }
    let (Some(root), Some(socket)) = (root, socket) else {
        return usage();
    };
    let failure = serve(&root, &socket);
    let _ = writeln!(std::io::stderr(), "ulei-inbox: {failure}");
    ExitCode::from(1)
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
