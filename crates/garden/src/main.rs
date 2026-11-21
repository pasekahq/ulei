//! `ulei-garden serve [--config PATH]`: the garden daemon, the one process that opens the live
//! store (ADR-0047 section 2). Every failure to start is one stderr line and exit 1.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use ulei_garden::embed::Embedder;
use ulei_garden::miss::Inbox;
use ulei_garden::serve::{Server, Settings, TIMEOUT, bind, open};
use ulei_garden::store::Store;
use ulei_garden::worker::{self, SystemClock, Wake, Worker};

const USAGE: &str = "usage: ulei-garden serve [--config PATH]\n";

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
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
    let line = serve(flag.as_deref());
    let _ = writeln!(std::io::stderr(), "garden: {line}");
    ExitCode::from(1)
}

/// Starts the daemon and serves until `accept` fails; the line saying why it stopped.
fn serve(flag: Option<&Path>) -> String {
    let settings = match Settings::read(flag) {
        Ok(settings) => settings,
        Err(e) => return e,
    };
    let helper = match &settings.embedder {
        None => None,
        Some(embedder) => match embedder.start() {
            Ok(helper) => Some(Arc::new(helper)),
            Err(e) => return format!("embedder: {}: {e}", embedder.helper.display()),
        },
    };
    let dense = helper.clone().map(|h| Box::new(h) as Box<dyn Embedder>);
    let garden = match open(&settings, dense) {
        Ok(garden) => garden,
        Err(e) => return format!("store {}: {e}", settings.store.display()),
    };
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
        Ok(match helper {
            Some(helper) => worker.with_embedder(Box::new(helper)),
            None => worker,
        })
    }) {
        Ok(background) => background,
        Err(e) => return format!("store {}: {e}", settings.store.display()),
    };
    let wake = Arc::new(Wake::default());
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
    let e = Server::new(garden, uid, TIMEOUT, wake, SystemClock).run(&listener);
    format!("accept: {e}")
}
