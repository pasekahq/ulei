//! Owner channels (Telegram first): ingress, coalescing, delivery.
//!
//! This crate talks to other modules only through `ulei-contracts` and the wire; see
//! `AGENTS.md` and `docs/architecture/adr/0001-*.md`.

#![forbid(unsafe_code)]

pub mod alert;
pub mod bot_api;
mod c1;
pub mod c6;
pub mod ingress;
pub mod notify;
pub mod render;
pub mod stt;
pub mod tg;
pub mod uds;

/// Wire types of the contracts this module speaks (`[package.metadata.ulei] contracts`).
pub mod wire {
    pub use ulei_contracts::generated::{c1, c6};
}
