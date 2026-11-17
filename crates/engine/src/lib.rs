//! Own synchronous agent loop: tool loop, tools, sessions; all model calls go to the qb gateway.
//!
//! This crate talks to other modules only through `ulei-contracts` and the wire; see
//! `AGENTS.md` and `docs/architecture/adr/0010-*.md`.

#![forbid(unsafe_code)]

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c4, c6};
}
