//! Memory below the garden: the archive of raw items (immutable, compressed, FTS5), served as
//! C10 by `ulei-memory serve`, and later the forest made from it. The archive pulls the engine's
//! dialogue through C6 `raw_sessions` (ADR-0051), [`pull`]. The store is [`archive::Archive`];
//! the daemon that serves it is [`serve`]; [`import`] puts a stream of items through it.

#![forbid(unsafe_code)]

pub mod archive;
pub mod import;
pub mod pull;
pub mod serve;

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c6, c10};
}
