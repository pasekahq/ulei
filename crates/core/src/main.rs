//! `ulei`: command-line entry of the ecosystem core.
//!
//! `--version` and `contracts` are answered here; `inbox ...` and `status ...` run the
//! `ulei-inbox` binary next to this one (ADR-0007, ADR-0040), so this binary links no `SQLite`.

#![forbid(unsafe_code)]

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

const USAGE: &str =
    "usage: ulei [--version | contracts | inbox ARGS... | status [--config PATH] | --help]\n";

fn main() -> ExitCode {
    let arg = std::env::args_os().nth(1);
    let mut out = std::io::stdout().lock();
    let result = match arg.as_ref().and_then(|a| a.to_str()) {
        None | Some("--version" | "-V") => writeln!(out, "ulei {}", ulei_core::VERSION),
        Some("contracts") => ulei_contracts::CONTRACTS
            .iter()
            .try_for_each(|c| writeln!(out, "{} {} {}", c.id, c.version, c.file)),
        Some("--help" | "-h") => out.write_all(USAGE.as_bytes()),
        Some("inbox") => return forward_inbox(2),
        Some("status") => return forward_inbox(1),
        Some(_) => {
            let _ = std::io::stderr().write_all(USAGE.as_bytes());
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}

/// Replaces this process with `ulei-inbox` from the directory of this executable, never from
/// `PATH`, passing the arguments from position `from` on unchanged; returns only on failure.
fn forward_inbox(from: usize) -> ExitCode {
    let sibling = match std::env::current_exe() {
        Ok(exe) => exe.with_file_name("ulei-inbox"),
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "ulei: own executable path: {e}");
            return ExitCode::from(1);
        }
    };
    let e = Command::new(&sibling)
        .args(std::env::args_os().skip(from))
        .exec();
    let _ = writeln!(
        std::io::stderr(),
        "ulei: cannot run {}: {e}",
        sibling.display()
    );
    ExitCode::from(1)
}
