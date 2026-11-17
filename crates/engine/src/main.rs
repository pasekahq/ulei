//! `ulei-engine`: the ecosystem's own agent loop.
//!
//! Answers `--version`; every other invocation is a usage error.

#![forbid(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "usage: ulei-engine --version\n";

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    if first.as_deref().and_then(|a| a.to_str()) == Some("--version") && args.next().is_none() {
        return match writeln!(
            std::io::stdout().lock(),
            "ulei-engine {}",
            env!("CARGO_PKG_VERSION")
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(1),
        };
    }
    let _ = std::io::stderr().write_all(USAGE.as_bytes());
    ExitCode::from(2)
}
