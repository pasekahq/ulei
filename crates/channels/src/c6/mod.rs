//! The engine over C6 (ADR-0033): the wire client and the owner chat's session that implements
//! the ingress [`crate::ingress::Engine`] seam.

mod client;
mod session;

pub use client::{Client, Error};
pub use session::{CHANNEL, ChatSession, session_key};
