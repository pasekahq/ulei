//! Garden: typed facts about the owner's life with a valid-time and a transaction-time axis,
//! provenance and expiry, and hybrid retrieval over them (C7). See `AGENTS.md` and ADR-0043.
//!
//! The shared vocabulary is [`model`], storage on SQLite is [`store`]; retrieval, the edge and
//! the C7 API are added on top of them, [`serve`] serves that API on the daemon's socket and
//! [`worker`] runs the daemon's background passes.

#![forbid(unsafe_code)]

pub mod api;
pub mod edge;
pub mod embed;
pub mod eval;
pub mod model;
pub mod search;
pub mod secret;
pub mod serve;
pub mod store;
pub mod worker;

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c1, c7};
}
