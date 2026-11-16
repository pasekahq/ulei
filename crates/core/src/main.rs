//! `ulei`: command-line entry of the ecosystem core.
//!
//! Skeleton (step S0): `--version` and `contracts`. It exists so that the CI budgets
//! (static binary size, cold start, peak RSS) are measured from the first commit.

#![forbid(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "usage: ulei [--version | contracts | --help]\n";

fn main() -> ExitCode {
    let arg = std::env::args_os().nth(1);
    let mut out = std::io::stdout().lock();
    let result = match arg.as_ref().and_then(|a| a.to_str()) {
        None | Some("--version" | "-V") => writeln!(out, "ulei {}", ulei_core::VERSION),
        Some("contracts") => ulei_contracts::CONTRACTS
            .iter()
            .try_for_each(|c| writeln!(out, "{} {} {}", c.id, c.version, c.file)),
        Some("--help" | "-h") => out.write_all(USAGE.as_bytes()),
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
