//! `ulei-priv list` and `ulei-priv run` (ADR-0073 section 6).

use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde_json::Value;

use crate::wire::{self, Request};

/// Exit status of a refusal.
pub const EXIT_REFUSED: u8 = 77;
/// Exit status when the daemon cannot be reached or breaks the protocol.
pub const EXIT_UNAVAILABLE: u8 = 69;
/// Bytes of a reply header line.
const MAX_HEADER: u64 = 1024 * 1024;

/// What a call ends in: the process's exit status.
pub type Status = u8;

fn unavailable(socket: &Path, why: &dyn std::fmt::Display) -> Status {
    eprintln!(
        "ulei-priv: daemon unavailable at {}: {why}",
        socket.display()
    );
    EXIT_UNAVAILABLE
}

/// Sends `req` and returns the reader positioned after the header, and the header.
fn call(socket: &Path, req: &Request) -> Result<(BufReader<UnixStream>, Value), Status> {
    let mut stream = UnixStream::connect(socket).map_err(|e| unavailable(socket, &e))?;
    // The daemon refuses some callers before it reads: a failed write may still have a reply.
    let sent = stream.write_all(&wire::encode(req));
    let mut reader = BufReader::new(stream);
    let mut header = Vec::new();
    let read = (&mut reader)
        .take(MAX_HEADER)
        .read_until(b'\n', &mut header);
    if header.last() != Some(&b'\n') {
        return Err(match (sent, read) {
            (Err(e), _) | (_, Err(e)) => unavailable(socket, &e),
            _ => unavailable(socket, &"the reply ended early"),
        });
    }
    let value: Value = serde_json::from_slice(&header).map_err(|e| unavailable(socket, &e))?;
    match value["decision"].as_str() {
        Some("ok") => Ok((reader, value)),
        Some("refused") => {
            let reason = value["reason"].as_str().unwrap_or("unknown");
            eprintln!("ulei-priv: refused: {reason}");
            Err(EXIT_REFUSED)
        }
        _ => Err(unavailable(socket, &"a reply without a decision")),
    }
}

/// Prints every call this caller may make, one per line.
pub fn list(socket: &Path) -> Status {
    let value = match call(socket, &Request::List) {
        Ok((_, v)) => v,
        Err(status) => return status,
    };
    let mut text = String::new();
    for cmd in value["commands"].as_array().into_iter().flatten() {
        let Some(id) = cmd["id"].as_str() else {
            continue;
        };
        let choices: Vec<&str> = cmd["arg"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if choices.is_empty() {
            let _ = writeln!(text, "ulei-priv run {id}");
        }
        for choice in choices {
            let _ = writeln!(text, "ulei-priv run {id} {choice}");
        }
    }
    match io::stdout().lock().write_all(text.as_bytes()) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Runs `id` with `args` and relays its output; the status is the child's.
pub fn run(socket: &Path, id: &str, args: &[String]) -> Status {
    let req = Request::Run {
        id: id.to_owned(),
        args: args.to_vec(),
    };
    let (mut reader, value) = match call(socket, &req) {
        Ok(found) => found,
        Err(status) => return status,
    };
    let count = |key: &str| value[key].as_u64().unwrap_or(0);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let read = (&mut reader)
        .take(count("stdout_bytes"))
        .read_to_end(&mut stdout)
        .and_then(|_| {
            (&mut reader)
                .take(count("stderr_bytes"))
                .read_to_end(&mut stderr)
        });
    if let Err(e) = read {
        return unavailable(socket, &e);
    }
    if u64::try_from(stdout.len() + stderr.len()).ok()
        != Some(count("stdout_bytes") + count("stderr_bytes"))
    {
        return unavailable(socket, &"the output ended early");
    }
    let _ = io::stdout().lock().write_all(&stdout);
    let mut err = io::stderr().lock();
    let _ = err.write_all(&stderr);
    if value["truncated"].as_bool() == Some(true) {
        let _ = writeln!(err, "ulei-priv: output truncated");
    }
    if value["timed_out"].as_bool() == Some(true) {
        let _ = writeln!(err, "ulei-priv: timed out, killed");
    }
    if let Some(code) = value["exit"].as_i64() {
        return u8::try_from(code).unwrap_or(1);
    }
    if let Some(sig) = value["signal"].as_i64() {
        return u8::try_from(128 + sig).unwrap_or(1);
    }
    unavailable(socket, &"a reply without exit or signal")
}
