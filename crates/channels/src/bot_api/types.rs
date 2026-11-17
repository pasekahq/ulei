// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Requests and results of the Bot API calls the channel makes.

use std::path::PathBuf;

use serde_json::Value;

/// One update as received; the payload stays opaque to the transport.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    /// Telegram's `update_id`; the poller confirms `last + 1`.
    pub update_id: i64,
    /// The whole update object, `update_id` included.
    pub payload: Value,
}

/// The bot's own account (`getMe`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotUser {
    /// Bot user id.
    pub id: i64,
    /// Bot user name without `@`.
    pub username: Option<String>,
}

/// A message the bot sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SentMessage {
    /// Its `message_id` in the chat.
    pub message_id: i64,
}

/// A file Telegram can serve (`getFile`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    /// Id for further calls.
    pub file_id: String,
    /// Size in bytes, when Telegram knows it.
    pub file_size: Option<u64>,
    /// Path under the file base; absent when the file cannot be downloaded.
    pub file_path: Option<String>,
}

/// A `sendMessage` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextMessage {
    /// Target chat.
    pub chat_id: i64,
    /// Message text, at most 4096 UTF-16 units (the caller splits).
    pub text: String,
    /// `HTML`, `MarkdownV2`, ... or `None` for plain text.
    pub parse_mode: Option<String>,
    /// Message to reply to.
    pub reply_to: Option<i64>,
    /// Deliver without a notification sound.
    pub disable_notification: bool,
}

impl TextMessage {
    /// A plain-text message to `chat_id`.
    pub fn plain(chat_id: i64, text: impl Into<String>) -> Self {
        Self {
            chat_id,
            text: text.into(),
            parse_mode: None,
            reply_to: None,
            disable_notification: false,
        }
    }
}

/// Which upload method and field a file goes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    /// `sendDocument`, field `document`.
    Document,
    /// `sendPhoto`, field `photo`.
    Photo,
    /// `sendVoice`, field `voice` (OGG/Opus).
    Voice,
}

impl MediaKind {
    pub(crate) fn method(self) -> &'static str {
        match self {
            Self::Document => "sendDocument",
            Self::Photo => "sendPhoto",
            Self::Voice => "sendVoice",
        }
    }

    pub(crate) fn field(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Photo => "photo",
            Self::Voice => "voice",
        }
    }
}

/// A file upload streamed from disk as `multipart/form-data`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    /// Upload method.
    pub kind: MediaKind,
    /// Target chat.
    pub chat_id: i64,
    /// Local file to send.
    pub path: PathBuf,
    /// Name shown in Telegram; defaults to the file name of `path`.
    pub file_name: Option<String>,
    /// Caption; cut to 1024 characters.
    pub caption: Option<String>,
    /// Parse mode of the caption.
    pub parse_mode: Option<String>,
    /// Message to reply to.
    pub reply_to: Option<i64>,
    /// Deliver without a notification sound.
    pub disable_notification: bool,
}
