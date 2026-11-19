//! The `ulei-tg` daemon: settings from `ulei.toml` and the channel's lifecycle (ADR-0032).

mod channel;
mod settings;
mod toml;

pub use channel::{Channel, Counts, Dispatch, RunError};
pub use settings::{CONFIG_ENV, Settings, SettingsError};
