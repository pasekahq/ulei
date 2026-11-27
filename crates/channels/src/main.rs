//! `ulei-tg`: the owner's Telegram channel (ADR-0032). SIGTERM and SIGINT stop it after the
//! pending work is handed on, the offset is confirmed, the queued engine calls are made, the
//! readers of the turns and the inbox thread stop and the queued messages are sent.
//! `ulei-tg notify` sends one technical notice from stdin or a file (ADR-0097) and exits.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use signal_hook::consts::{SIGINT, SIGTERM};
use ulei_channels::bot_api::Client;
use ulei_channels::bot_api::{Sender, Stop, SystemClock};
use ulei_channels::c6;
use ulei_channels::notify::{self, MAX_TEXT_BYTES};
use ulei_channels::tg::{Channel, Inbox, Link, Outbound, SentTexts, Settings, Transport};

const USAGE: &str = "usage: ulei-tg [--config PATH] | ulei-tg --version\n       \
                     ulei-tg notify [--config PATH] [--file PATH]\n";

const EXIT_FAILURE: u8 = 1;
const EXIT_USAGE: u8 = 2;
/// `notify` refused the config or the text before any message went out.
const EXIT_REFUSED: u8 = 3;
/// Largest input `notify` reads; room for whitespace around a text of `MAX_TEXT_BYTES`.
const MAX_INPUT_BYTES: usize = 2 * MAX_TEXT_BYTES;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "notify") {
        return notify_main(&args[1..]);
    }
    let path = match args.as_slice() {
        [] => None,
        [flag] if flag == "--version" => {
            return match writeln!(io::stdout().lock(), "ulei-tg {}", env!("CARGO_PKG_VERSION")) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::from(EXIT_FAILURE),
            };
        }
        [flag, path] if flag == "--config" => Some(Path::new(path)),
        _ => return usage(),
    };
    match serve(path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(line) => {
            let _ = writeln!(io::stderr().lock(), "ulei-tg: {line}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

/// `ulei-tg notify [--config PATH] [--file PATH]`: the receipt on stdout and exit 0, else one
/// stderr line and `EXIT_REFUSED` before any send or `EXIT_FAILURE` after one may have gone out.
fn notify_main(args: &[OsString]) -> ExitCode {
    let (mut config, mut file) = (None, None);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let slot = match flag.to_str() {
            Some("--config") => &mut config,
            Some("--file") => &mut file,
            _ => return usage(),
        };
        match rest.next() {
            Some(value) if slot.is_none() => *slot = Some(Path::new(value)),
            _ => return usage(),
        }
    }
    let (code, line) = match send_notice(config, file) {
        Ok(receipt) => match writeln!(io::stdout().lock(), "{receipt}") {
            Ok(()) => return ExitCode::SUCCESS,
            Err(e) => (EXIT_FAILURE, format!("stdout: {e}")),
        },
        Err(failed) => failed,
    };
    let _ = writeln!(io::stderr().lock(), "ulei-tg notify: {line}");
    ExitCode::from(code)
}

/// The receipt line of the notice read from `file` or stdin, or the exit code and the line
/// saying why it was not sent whole.
fn send_notice(config: Option<&Path>, file: Option<&Path>) -> Result<String, (u8, String)> {
    let refused = |line: String| (EXIT_REFUSED, line);
    let transport = Transport::load(config).map_err(|e| refused(e.to_string()))?;
    let bytes = match file {
        Some(path) => File::open(path)
            .and_then(read_bounded)
            .map_err(|e| refused(format!("{}: {e}", path.display())))?,
        None => read_bounded(io::stdin().lock()).map_err(|e| refused(format!("stdin: {e}")))?,
    };
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(refused(format!(
            "the input is over {MAX_INPUT_BYTES} bytes"
        )));
    }
    let text = String::from_utf8(bytes).map_err(|_| refused("the text is not UTF-8".to_owned()))?;
    let sender = Sender::new(Client::new(transport.bot), Arc::new(SystemClock));
    match notify::notify(&sender, &transport.owner_ids, &text) {
        Ok(receipt) => Ok(receipt.to_line()),
        Err(e) if e.is_refusal() => Err(refused(e.to_string())),
        Err(e) => Err((EXIT_FAILURE, e.to_string())),
    }
}

/// The bytes of `input` up to one past `MAX_INPUT_BYTES`, so an over-long input is seen
/// without reading all of it.
fn read_bounded(input: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input
        .take(MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn usage() -> ExitCode {
    let _ = io::stderr().write_all(USAGE.as_bytes());
    ExitCode::from(EXIT_USAGE)
}

/// Runs the channel until a signal or a fatal error; the line saying why it failed.
fn serve(path: Option<&Path>) -> Result<(), String> {
    let settings = Settings::load(path).map_err(|e| e.to_string())?;
    let stop = Stop::new();
    stop_on_signals(&stop).map_err(|e| format!("signal handler: {e}"))?;
    let engine = c6::Client::new(
        &settings.engine_socket,
        settings.engine_connect_timeout,
        settings.engine_call_timeout,
    )
    .map_err(|e| format!("engine client: {e}"))?;
    let zone = settings.zone.clone();
    let owner_ids = settings.owner_ids.clone();
    let echo_transcripts = settings.stt_echo_transcripts;
    let texts = SentTexts::new();
    let channel = Channel::start(settings, stop, Box::new(texts.clone()))
        .map_err(|e| format!("getMe: {e}"))?;
    let sender = Arc::new(Sender::new(channel.client().clone(), Arc::new(SystemClock)));
    let outbound = Outbound::start(sender, texts).map_err(|e| format!("outbound thread: {e}"))?;
    let inbox = Inbox::start(engine.clone(), outbound.outbox(), &owner_ids)
        .map_err(|e| format!("inbox thread: {e}"))?;
    let mut link = Link::start(engine, outbound.outbox(), zone, echo_transcripts)
        .map_err(|e| format!("engine thread: {e}"))?;
    let mut out = io::stdout().lock();
    writeln!(out, "ulei-tg ready")
        .and_then(|()| out.flush())
        .map_err(|e| format!("stdout: {e}"))?;
    drop(out);
    let result = channel.run(&mut link);
    let counts = link.counts();
    let linked = link.finish();
    let held = inbox.finish();
    let finished = outbound.finish();
    let _ = writeln!(
        io::stderr().lock(),
        "ulei-tg: dispatched {} turns, {} commands, {} notices",
        counts.turns,
        counts.commands,
        counts.notices
    );
    result.map_err(|e| e.to_string())?;
    linked.map_err(|e| format!("engine thread: {e}"))?;
    held.map_err(|e| format!("inbox thread: {e}"))?;
    finished.map_err(|e| e.to_string())
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
