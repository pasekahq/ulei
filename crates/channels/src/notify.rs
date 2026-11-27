//! Direct technical notices to the owner (ADR-0097): plain text straight through the Bot API to
//! every owner chat, with no engine turn and no inbox event, answered by a receipt or an error.

use std::fmt;

use serde_json::json;

use crate::bot_api::{Error, Sender, TextMessage};
use crate::render::{MAX_MESSAGE_LEN, split_message};

/// Largest notice in bytes of UTF-8; with Telegram's limit it is at most nine messages.
pub const MAX_TEXT_BYTES: usize = 32 * 1024;

/// What a notice became: the messages sent, per owner chat in the order of `owner_ids`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// One entry per owner chat.
    pub chats: Vec<ChatReceipt>,
}

/// The messages of one owner chat, in sending order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatReceipt {
    /// The chat the parts went to.
    pub chat_id: i64,
    /// Telegram's `message_id` of each part.
    pub message_ids: Vec<i64>,
}

impl Receipt {
    /// The receipt as one line of JSON without a newline.
    pub fn to_line(&self) -> String {
        let chats: Vec<_> = self
            .chats
            .iter()
            .map(|c| json!({ "chat_id": c.chat_id, "message_ids": c.message_ids }))
            .collect();
        json!({ "chats": chats }).to_string()
    }
}

/// Why a notice was refused or not sent whole. No variant carries the text or the token.
#[derive(Debug)]
pub enum NotifyError {
    /// The text is empty or only whitespace; nothing was sent.
    Empty,
    /// The text is longer than [`MAX_TEXT_BYTES`]; nothing was sent.
    TooLong,
    /// No owner id to send to; nothing was sent.
    NoOwner,
    /// A message failed after the delivery retries; `sent` of `total` messages went out before.
    Send {
        /// Messages sent before the failure.
        sent: usize,
        /// Messages the notice needed over every owner chat.
        total: usize,
        /// The error that ended the delivery.
        error: Error,
    },
}

impl NotifyError {
    /// Whether the notice was refused before any message went out.
    pub fn is_refusal(&self) -> bool {
        !matches!(self, Self::Send { .. })
    }
}

impl fmt::Display for NotifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the text is empty"),
            Self::TooLong => write!(f, "the text is over {MAX_TEXT_BYTES} bytes"),
            Self::NoOwner => f.write_str("no owner id to send to"),
            Self::Send { sent, total, error } => {
                write!(f, "sent {sent} of {total} messages, then {error}")
            }
        }
    }
}

impl std::error::Error for NotifyError {}

/// Sends `text` without surrounding whitespace as plain text to each of `owner_ids` in order,
/// split at Telegram's limit, each message under the sender's retry ladders and delivery retries.
/// The first message that fails after them ends the notice.
///
/// # Errors
/// [`NotifyError`]: a refusal before any send, or the failure and how many messages went out.
pub fn notify(sender: &Sender, owner_ids: &[i64], text: &str) -> Result<Receipt, NotifyError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(NotifyError::Empty);
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(NotifyError::TooLong);
    }
    if owner_ids.is_empty() {
        return Err(NotifyError::NoOwner);
    }
    let parts = split_message(text, MAX_MESSAGE_LEN);
    let total = parts.len() * owner_ids.len();
    let mut sent = 0;
    let mut chats = Vec::with_capacity(owner_ids.len());
    for &chat_id in owner_ids {
        let mut message_ids = Vec::with_capacity(parts.len());
        for part in &parts {
            let message = TextMessage::plain(chat_id, part.as_str());
            // The caller gets the error; the daemon's failure notice speaks of a reply.
            let delivered = sender.send_with_retry(|| sender.send_message(&message), |_| {});
            let message_id = delivered
                .map_err(|error| NotifyError::Send { sent, total, error })?
                .message_id;
            message_ids.push(message_id);
            sent += 1;
        }
        chats.push(ChatReceipt {
            chat_id,
            message_ids,
        });
    }
    Ok(Receipt { chats })
}
