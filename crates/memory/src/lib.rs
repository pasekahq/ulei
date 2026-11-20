//! Memory: the forest, the cold archive of raw sessions read through C6. The garden and its
//! retrieval are the crate `ulei-garden` (ADR-0043).
//!
//! Skeleton (step S0): no behaviour yet. This crate talks to other modules only through
//! `ulei-contracts` and the wire; see `AGENTS.md` and `docs/architecture/adr/0001-*.md`.

#![forbid(unsafe_code)]

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c1, c7};
}
