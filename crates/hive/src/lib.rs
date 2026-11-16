//! Hive: card queue, heads, acceptance and delivery of results (C3).
//!
//! Skeleton (step S0): no behaviour yet. This crate talks to other modules only through
//! `ulei-contracts` and the wire; see `AGENTS.md` and `docs/architecture/adr/0001-*.md`.

#![forbid(unsafe_code)]

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c1, c3};
}
