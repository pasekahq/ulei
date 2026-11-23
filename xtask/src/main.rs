//! Repository tooling for ulei. Run as `cargo xtask <command>`.
//!
//! Commands:
//! - `arch`                     crate-graph boundaries, source bans (ADR-0001) and the Python
//!   tree's rules (ADR-0068)
//! - `module-map [--check]`     write or verify docs/architecture/module-map.md
//! - `gen-contracts [--check]`  write or verify Rust types generated from contracts/
//! - `contracts [--base REV]`   metaschema, fixtures, index/CHANGELOG, generated code,
//!   and (with a base) schema compatibility against REV
//! - `adr [--base REV]`         ADR format, and (with a base) "new crate/dependency/pin/contract
//!   needs an ADR with a matching `Affects:` line"
//! - `check [--base REV]`       all of the above in check mode

mod adr;
mod arch;
mod codegen;
mod contracts;
mod map;
mod pytree;
mod util;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let check = args.iter().any(|a| a == "--check");
    let base = args
        .iter()
        .position(|a| a == "--base")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
        .filter(|b| !b.is_empty() && b.chars().any(|c| c != '0'));
    let root = util::root();
    let result = match cmd {
        "arch" => arch::run(&root),
        "module-map" => map::run(&root, check),
        "gen-contracts" => codegen::run(&root, check),
        "contracts" => contracts::run(&root, base),
        "adr" => adr::run(&root, base),
        "check" => arch::run(&root)
            .and_then(|()| map::run(&root, true))
            .and_then(|()| contracts::run(&root, base))
            .and_then(|()| adr::run(&root, base)),
        _ => {
            eprintln!(
                "usage: cargo xtask <arch | module-map [--check] | gen-contracts [--check] | contracts [--base REV] | adr [--base REV] | check [--base REV]>"
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => {
            eprintln!("xtask {cmd}: ok");
            ExitCode::SUCCESS
        }
        Err(errors) => {
            for e in &errors {
                eprintln!("xtask {cmd}: error: {e}");
            }
            eprintln!("xtask {cmd}: {} error(s)", errors.len());
            ExitCode::FAILURE
        }
    }
}
