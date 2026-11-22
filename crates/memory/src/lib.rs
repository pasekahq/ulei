//! Memory below the garden: the archive of raw items (immutable, compressed, FTS5), served as
//! C10 by `ulei-memory serve`, and later the forest made from it. The archive pulls the engine's
//! dialogue through C6 `raw_sessions` (ADR-0051). No behaviour yet.

#![forbid(unsafe_code)]

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c6, c10};
}
