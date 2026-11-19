//! `ulei-tg`: the owner's Telegram channel (ADR-0032). SIGTERM and SIGINT stop it after the
//! pending work is handed on and the offset is confirmed.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use signal_hook::consts::{SIGINT, SIGTERM};
use ulei_channels::bot_api::Stop;
use ulei_channels::tg::{CONFIG_ENV, Channel, Counts, Settings};

const USAGE: &str = "usage: ulei-tg [--config PATH] | ulei-tg --version\n";

const EXIT_FAILURE: u8 = 1;
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let path = match args.as_slice() {
        [] => std::env::var_os(CONFIG_ENV).map(PathBuf::from),
        [flag] if flag == "--version" => {
            return match writeln!(io::stdout().lock(), "ulei-tg {}", env!("CARGO_PKG_VERSION")) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::from(EXIT_FAILURE),
            };
        }
        [flag, path] if flag == "--config" => Some(PathBuf::from(path)),
        _ => {
            let _ = io::stderr().write_all(USAGE.as_bytes());
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match serve(path.as_deref()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(line) => {
            let _ = writeln!(io::stderr().lock(), "ulei-tg: {line}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

/// Runs the channel until a signal or a fatal error; the line saying why it failed.
fn serve(path: Option<&Path>) -> Result<(), String> {
    let settings = Settings::load(path).map_err(|e| e.to_string())?;
    let stop = Stop::new();
    stop_on_signals(&stop).map_err(|e| format!("signal handler: {e}"))?;
    let channel = Channel::start(settings, stop).map_err(|e| format!("getMe: {e}"))?;
    let mut out = io::stdout().lock();
    writeln!(out, "ulei-tg ready")
        .and_then(|()| out.flush())
        .map_err(|e| format!("stdout: {e}"))?;
    drop(out);
    let mut counts = Counts::default();
    let result = channel.run(&mut counts);
    let _ = writeln!(
        io::stderr().lock(),
        "ulei-tg: dispatched {} turns, {} commands, {} notices",
        counts.turns,
        counts.commands,
        counts.notices
    );
    result.map_err(|e| e.to_string())
}

/// Makes SIGTERM and SIGINT request `stop`: the handler writes to a socket pair whose reader
/// thread sleeps in `read`. A repeated signal requests the same stop again.
fn stop_on_signals(stop: &Stop) -> io::Result<()> {
    let (mut reader, writer) = UnixStream::pair()?;
    signal_hook::low_level::pipe::register(SIGTERM, writer.try_clone()?)?;
    signal_hook::low_level::pipe::register(SIGINT, writer)?;
    let stop = stop.clone();
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let mut byte = [0; 1];
            loop {
                match reader.read(&mut byte) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Ok(0) | Err(_) => return,
                    Ok(_) => stop.request(),
                }
            }
        })?;
    Ok(())
}
