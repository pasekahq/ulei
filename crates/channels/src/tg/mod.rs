//! The `ulei-tg` daemon: settings from `ulei.toml`, the channel's lifecycle and the delivery to
//! the owner (ADR-0032).

mod channel;
mod follow;
mod link;
mod outbound;
mod sent;
mod settings;
mod toml;

pub use channel::{Channel, Dispatch, RunError};
pub use follow::EMPTY_ANSWER;
pub use link::{Counts, ENGINE_FAILED, Link};
pub use outbound::{Closed, Outbound, Outbox};
pub use sent::SentTexts;
pub use settings::{CONFIG_ENV, Settings, SettingsError};
