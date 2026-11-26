// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Bot API update model: only the fields the ingress reads. Unknown fields are ignored and
//! update kinds without a field here parse to an update whose [`Update::kind`] is `None`.

use serde::Deserialize;
use serde_json::{Map, Value};

/// One Bot API `Update` as returned by `getUpdates`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Update {
    /// Monotonic update id; the poller acknowledges with `offset = update_id + 1`.
    pub update_id: i64,
    /// New incoming message.
    #[serde(default)]
    pub message: Option<Message>,
    /// New version of a message that was edited.
    #[serde(default)]
    pub edited_message: Option<Message>,
    /// A reaction on a message was changed by a user.
    #[serde(default)]
    pub message_reaction: Option<MessageReactionUpdated>,
    /// A button of an inline keyboard was pressed.
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

/// The kinds of update the ingress handles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UpdateKind<'a> {
    /// `message` or `edited_message`; `edited` tells which.
    Message {
        /// The message.
        message: &'a Message,
        /// It came as `edited_message`.
        edited: bool,
    },
    /// `message_reaction`.
    Reaction(&'a MessageReactionUpdated),
    /// `callback_query`: the guest gate's buttons go to the gate before the ingress (ADR-0083);
    /// the ingress itself acts on no button.
    Callback(&'a CallbackQuery),
}

impl Update {
    /// Parses one update from JSON bytes.
    ///
    /// # Errors
    /// The bytes are not JSON or lack `update_id`.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Parses one update from a JSON value (an element of the `getUpdates` result).
    ///
    /// # Errors
    /// The value is not an object with `update_id`.
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }

    /// The handled payload, or `None` for an update kind the ingress skips.
    pub fn kind(&self) -> Option<UpdateKind<'_>> {
        if let Some(message) = &self.message {
            return Some(UpdateKind::Message {
                message,
                edited: false,
            });
        }
        if let Some(message) = &self.edited_message {
            return Some(UpdateKind::Message {
                message,
                edited: true,
            });
        }
        if let Some(reaction) = &self.message_reaction {
            return Some(UpdateKind::Reaction(reaction));
        }
        self.callback_query.as_ref().map(UpdateKind::Callback)
    }
}

/// A press of an inline keyboard button (`CallbackQuery`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CallbackQuery {
    /// Unique identifier, for `answerCallbackQuery`.
    pub id: String,
    /// The user who pressed the button.
    #[serde(rename = "from")]
    pub from_user: User,
    /// The button's `callback_data`.
    #[serde(default)]
    pub data: Option<String>,
    /// The message with the button, kept raw: it may be an inaccessible message.
    #[serde(default)]
    pub message: Option<Value>,
}

/// A Telegram user or bot.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    /// Unique identifier.
    pub id: i64,
    /// The user is a bot.
    #[serde(default)]
    pub is_bot: bool,
    /// First name.
    #[serde(default)]
    pub first_name: String,
    /// Last name.
    #[serde(default)]
    pub last_name: Option<String>,
    /// Username without `@`.
    #[serde(default)]
    pub username: Option<String>,
}

impl User {
    /// First and last name joined by a space, as Telegram clients show it.
    pub fn full_name(&self) -> String {
        match &self.last_name {
            Some(last) if !last.is_empty() => format!("{} {last}", self.first_name),
            _ => self.first_name.clone(),
        }
    }
}

/// A chat.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Chat {
    /// Unique identifier.
    pub id: i64,
    /// `private`, `group`, `supergroup` or `channel`.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Title of groups and channels.
    #[serde(default)]
    pub title: Option<String>,
    /// Username without `@`.
    #[serde(default)]
    pub username: Option<String>,
    /// First name of the other party in a private chat.
    #[serde(default)]
    pub first_name: Option<String>,
    /// Last name of the other party in a private chat.
    #[serde(default)]
    pub last_name: Option<String>,
}

impl Chat {
    /// Name of a private chat (first and last name), if it has one.
    pub fn full_name(&self) -> Option<String> {
        let first = self.first_name.as_deref().filter(|f| !f.is_empty())?;
        Some(match self.last_name.as_deref() {
            Some(last) if !last.is_empty() => format!("{first} {last}"),
            _ => first.to_owned(),
        })
    }
}

/// One special entity in a text (only offsets and the fields the ingress reads).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MessageEntity {
    /// Entity type, for example `bot_command` or `custom_emoji`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Offset in UTF-16 code units.
    pub offset: i64,
    /// Length in UTF-16 code units.
    pub length: i64,
    /// Custom emoji id for `custom_emoji` entities.
    #[serde(default)]
    pub custom_emoji_id: Option<String>,
}

/// A downloadable file as embedded in a message (photo size, document, voice, ...).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FileRef {
    /// Id for `getFile`.
    pub file_id: String,
    /// Size in bytes, when Telegram knows it.
    #[serde(default)]
    pub file_size: Option<u64>,
    /// Original file name (documents, audio).
    #[serde(default)]
    pub file_name: Option<String>,
    /// MIME type as sent by the client.
    #[serde(default)]
    pub mime_type: Option<String>,
    /// Duration in seconds (voice, audio, video).
    #[serde(default)]
    pub duration: Option<u64>,
}

/// A sticker.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Sticker {
    /// Id for `getFile`.
    pub file_id: String,
    /// Emoji associated with the sticker.
    #[serde(default)]
    pub emoji: Option<String>,
    /// Name of the sticker set.
    #[serde(default)]
    pub set_name: Option<String>,
    /// Animated (TGS) sticker.
    #[serde(default)]
    pub is_animated: bool,
    /// Video (WEBM) sticker.
    #[serde(default)]
    pub is_video: bool,
}

/// A point on the map.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Location {
    /// Latitude.
    pub latitude: f64,
    /// Longitude.
    pub longitude: f64,
}

/// A venue.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Venue {
    /// Location of the venue.
    pub location: Location,
    /// Name.
    #[serde(default)]
    pub title: Option<String>,
    /// Address.
    #[serde(default)]
    pub address: Option<String>,
}

/// Origin of a forwarded message (`MessageOrigin`, all four variants in one struct).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MessageOrigin {
    /// `user`, `hidden_user`, `chat` or `channel`.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Unix time the original message was sent.
    #[serde(default)]
    pub date: Option<i64>,
    /// `user`: the original sender.
    #[serde(default)]
    pub sender_user: Option<User>,
    /// `hidden_user`: name of the sender who hides their account.
    #[serde(default)]
    pub sender_user_name: Option<String>,
    /// `chat`: the chat that sent the original message.
    #[serde(default)]
    pub sender_chat: Option<Chat>,
    /// `channel`: the channel the message was posted in.
    #[serde(default)]
    pub chat: Option<Chat>,
    /// Signature of the original post author.
    #[serde(default)]
    pub author_signature: Option<String>,
}

/// The part of a replied-to message the user quoted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TextQuote {
    /// Quoted text.
    pub text: String,
    /// Entities of the quote.
    #[serde(default)]
    pub entities: Vec<MessageEntity>,
}

/// A message (only the fields the ingress reads; every other field lands in `other`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Message {
    /// Message id inside the chat.
    pub message_id: i64,
    /// Unix time the message was sent.
    #[serde(default)]
    pub date: i64,
    /// Chat the message belongs to.
    pub chat: Chat,
    /// Sender; absent for channel posts.
    #[serde(default, rename = "from")]
    pub from_user: Option<User>,
    /// Text of a text message.
    #[serde(default)]
    pub text: Option<String>,
    /// Entities of `text`.
    #[serde(default)]
    pub entities: Vec<MessageEntity>,
    /// Caption of a media message.
    #[serde(default)]
    pub caption: Option<String>,
    /// Entities of `caption`.
    #[serde(default)]
    pub caption_entities: Vec<MessageEntity>,
    /// Bot API rich message (formatted lists, tables, headings) when `text` is absent.
    #[serde(default)]
    pub rich_message: Option<Value>,
    /// Photo sizes, smallest first.
    #[serde(default)]
    pub photo: Vec<FileRef>,
    /// General file.
    #[serde(default)]
    pub document: Option<FileRef>,
    /// Voice note.
    #[serde(default)]
    pub voice: Option<FileRef>,
    /// Audio file.
    #[serde(default)]
    pub audio: Option<FileRef>,
    /// Video.
    #[serde(default)]
    pub video: Option<FileRef>,
    /// Round video note.
    #[serde(default)]
    pub video_note: Option<FileRef>,
    /// Sticker.
    #[serde(default)]
    pub sticker: Option<Sticker>,
    /// Shared location.
    #[serde(default)]
    pub location: Option<Location>,
    /// Shared venue.
    #[serde(default)]
    pub venue: Option<Venue>,
    /// Shared contact (kept raw: the ingress only needs to know it is there).
    #[serde(default)]
    pub contact: Option<Value>,
    /// Origin of a forwarded message.
    #[serde(default)]
    pub forward_origin: Option<MessageOrigin>,
    /// Legacy forward fields of older Bot API versions.
    #[serde(default)]
    pub forward_from: Option<User>,
    /// Legacy: forwarded from a chat.
    #[serde(default)]
    pub forward_from_chat: Option<Chat>,
    /// Legacy: name of a sender who hides their account.
    #[serde(default)]
    pub forward_sender_name: Option<String>,
    /// Legacy: signature of the post author.
    #[serde(default)]
    pub forward_signature: Option<String>,
    /// Legacy: date of the original message.
    #[serde(default)]
    pub forward_date: Option<i64>,
    /// The message this one replies to.
    #[serde(default)]
    pub reply_to_message: Option<Box<Message>>,
    /// The quoted part of the replied-to message.
    #[serde(default)]
    pub quote: Option<TextQuote>,
    /// Album id shared by the items of one media group.
    #[serde(default)]
    pub media_group_id: Option<String>,
    /// Every other field of the message, by name.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// One reaction (`ReactionType`): an emoji, a custom emoji or a paid reaction.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReactionType {
    /// `emoji`, `custom_emoji` or `paid`.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// The emoji of an `emoji` reaction.
    #[serde(default)]
    pub emoji: Option<String>,
    /// The id of a `custom_emoji` reaction.
    #[serde(default)]
    pub custom_emoji_id: Option<String>,
}

/// A change of the reactions on a message (`MessageReactionUpdated`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MessageReactionUpdated {
    /// Chat of the message.
    pub chat: Chat,
    /// Id of the message.
    pub message_id: i64,
    /// The user who changed the reaction.
    #[serde(default)]
    pub user: Option<User>,
    /// The chat on behalf of which the reaction was changed (anonymous admins).
    #[serde(default)]
    pub actor_chat: Option<Chat>,
    /// Unix time of the change.
    #[serde(default)]
    pub date: i64,
    /// Reactions after the change.
    #[serde(default)]
    pub new_reaction: Vec<ReactionType>,
}
