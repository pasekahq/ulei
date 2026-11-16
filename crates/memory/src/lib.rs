//! Memory: garden (facts), retrieval and the forest (cold session archive) as one module (C7).
//!
//! Skeleton (step S0): no behaviour yet. This crate talks to other modules only through
//! `ulei-contracts` and the wire; see `AGENTS.md` and `docs/architecture/adr/0001-*.md`.

#![forbid(unsafe_code)]

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c1, c7};
}
