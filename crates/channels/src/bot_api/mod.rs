//! Telegram Bot API transport: a blocking client, the long-poll loop and paced sending.
//!
//! The update payload stays opaque here (`serde_json::Value` plus its `update_id`); typed
//! updates belong to the ingress. No error, log line or `Debug` output carries the token.

mod client;
mod clock;
mod config;
mod error;
mod files;
mod multipart;
mod poller;
mod sender;
mod types;

pub use client::{ALL_UPDATE_TYPES, Client};
pub use clock::{Clock, Stop, SystemClock};
pub use config::{Config, DEFAULT_BASE_URL, DEFAULT_FILE_BASE_URL};
pub use error::{Error, ErrorKind, redact};
pub use poller::Poller;
pub use sender::{DELIVERY_FAILED_NOTICE, FloodLimiter, Sender};
pub use types::{BotUser, FileInfo, MediaKind, SentMessage, TextMessage, Update, Upload};
