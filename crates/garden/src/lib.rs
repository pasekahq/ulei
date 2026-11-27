//! Garden: typed facts about the owner's life with a valid-time and a transaction-time axis,
//! provenance and expiry, and hybrid retrieval over them (C7). See `AGENTS.md` and ADR-0043.
//!
//! The shared vocabulary is [`model`], storage on SQLite is [`store`]; retrieval, the edge and
//! the C7 API build on them. [`matrix`] holds the daemon's vectors in memory, [`rerank`]
//! reorders the fused candidates, [`serve`] serves that API on the daemon's socket, [`worker`]
//! runs the daemon's background passes, [`second`] looks up a search miss and [`miss`]
//! publishes the misses it does not answer to the owner inbox, [`profile`] renders the pinned
//! core profile to its file, [`places`] writes the places bed, [`places_search`] answers it,
//! [`places_import`] reads a places file into it and [`places_cli`] runs the secretary's commands
//! on it, both over C7 through [`places_wire`]. [`translit`] spells a name in ASCII so that its Latin and Cyrillic spellings meet, and [`morph`] stems Russian words so
//! that their forms meet.

#![forbid(unsafe_code)]

pub mod api;
pub mod client;
pub mod edge;
pub mod embed;
pub mod eval;
pub mod matrix;
pub mod miss;
pub mod model;
pub mod morph;
pub mod places;
pub mod places_cli;
pub mod places_import;
pub mod places_search;
pub mod places_wire;
pub mod profile;
pub mod rerank;
pub mod search;
pub mod second;
pub mod secret;
pub mod serve;
pub mod store;
pub mod translit;
pub mod worker;

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c1, c7};
}
