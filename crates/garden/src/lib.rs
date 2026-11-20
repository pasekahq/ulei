//! Garden: typed facts about the owner's life with a valid-time and a transaction-time axis,
//! provenance and expiry, and hybrid retrieval over them (C7). See `AGENTS.md` and ADR-0042.
//!
//! The shared vocabulary is [`model`], storage on SQLite is [`store`]; retrieval, the edge and
//! the C7 API are added on top of them.

#![forbid(unsafe_code)]

pub mod model;
pub mod store;

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c1, c7};
}
