//! Own synchronous agent loop: tool loop, tools, sessions; all model calls go to the qb gateway.
//!
//! This crate talks to other modules only through `ulei-contracts` and the wire; see
//! `AGENTS.md` and `docs/architecture/adr/0010-*.md`.

#![forbid(unsafe_code)]

pub mod agent;
pub mod archive;
pub mod compress;
pub mod config;
pub mod garden;
pub mod gateway;
pub mod images;
pub mod journal;
pub mod mcp;
pub mod night;
pub mod policy;
pub mod prompt;
pub mod serve;
pub mod skills;
mod sort;
pub mod tools;
pub mod trace;
#[cfg(feature = "web")]
pub mod web;
pub mod zone;

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c4g, c6, c7, c8, c10};
}
