//! Own synchronous agent loop: tool loop, tools, sessions; all model calls go to the qb gateway.
//!
//! This crate talks to other modules only through `ulei-contracts` and the wire; see
//! `AGENTS.md` and `docs/architecture/adr/0010-*.md`.

#![forbid(unsafe_code)]

pub mod agent;
pub mod compress;
pub mod config;
pub mod gateway;
pub mod journal;
pub mod mcp;
pub mod policy;
pub mod prompt;
pub mod serve;
pub mod skills;
mod sort;
pub mod tools;
#[cfg(feature = "web")]
pub mod web;
pub mod zone;

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c4g, c6, c8};
}
