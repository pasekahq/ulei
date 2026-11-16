//! Glue to class-1 products: qb (C4) and the A2A gate (C5), mapped onto ecosystem contracts.
//!
//! Skeleton (step S0): no behaviour yet. This crate talks to other modules only through
//! `ulei-contracts` and the wire; see `AGENTS.md` and `docs/architecture/adr/0001-*.md`.

#![forbid(unsafe_code)]

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c1, c4, c5, c7};
}
