//! Core of the ecosystem: owner inbox and events (C1), module registry, config, notify fallback, MCP facade (C2), `ulei` CLI.
//!
//! This crate talks to other modules only through `ulei-contracts` and the wire; see
//! `AGENTS.md` and `docs/architecture/adr/0001-*.md`.

#![forbid(unsafe_code)]

pub mod c6;
pub mod deliver;
pub mod inbox;
pub mod prompt;
pub mod serve;

/// Version of the core crate (and of the `ulei` binary).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c1, c2, c6};
}
