//! The `ulei-tg` daemon: settings from `ulei.toml`, the channel's lifecycle, the delivery to
//! the owner (ADR-0032) and the owner's inbox turns (ADR-0036).

mod channel;
mod follow;
mod inbox;
mod link;
mod outbound;
mod sent;
mod settings;

pub use channel::{Channel, Dispatch, RunError};
pub use follow::EMPTY_ANSWER;
pub use inbox::Inbox;
pub use link::{Counts, ENGINE_FAILED, Link, spawn_vital};
pub use outbound::{Closed, Outbound, Outbox};
pub use sent::SentTexts;
pub use settings::{Settings, SettingsError};
