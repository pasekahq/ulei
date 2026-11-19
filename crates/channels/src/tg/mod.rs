//! The `ulei-tg` daemon: settings from `ulei.toml`, the channel's lifecycle and the delivery to
//! the owner (ADR-0032).

mod channel;
mod outbound;
mod sent;
mod settings;
mod toml;

pub use channel::{Channel, Dispatch, RunError};
pub use outbound::{Closed, Counts, Outbound, Outbox};
pub use sent::SentTexts;
pub use settings::{CONFIG_ENV, Settings, SettingsError};
