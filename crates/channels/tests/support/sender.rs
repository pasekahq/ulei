//! A Bot API sender of the fake server whose pauses a fake clock records, and the fake's answer
//! to one sent message.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use ulei_channels::bot_api::{Client, Config, Sender};

use super::fake_bot_api::{FakeBotApi, Reply, ok};
use super::fake_clock::FakeClock;

/// A sender of `fake` with a 2 s read timeout; its clock adds 250 ms of jitter to each pause.
pub(crate) fn sender(fake: &FakeBotApi) -> (Sender, Arc<FakeClock>) {
    let mut config = Config::new("123456:TEST", Some(&fake.base_url()));
    config.read_timeout = Duration::from_secs(2);
    let clock = Arc::new(FakeClock::new(Duration::from_millis(250)));
    (Sender::new(Client::new(config), clock.clone()), clock)
}

/// The `sendMessage` answer of message `id`.
pub(crate) fn sent(id: i64) -> Reply {
    ok(json!({ "message_id": id, "chat": { "id": 1 } }))
}
