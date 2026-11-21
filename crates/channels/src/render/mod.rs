//! Agent reply -> Telegram: the `MEDIA:` files of a final reply, Markdown to `MarkdownV2` with
//! a plain-text fallback, splitting at Telegram's 4096 UTF-16 unit limit, the per-reply send
//! plan, the intentional-silence filter and the typing cadence.
//!
//! Pure and deterministic: the transport executes the plans, the clock and the file checks are
//! injected.

mod markdown;
pub(crate) mod media;
mod plain;
pub(crate) mod pytext;
mod send;
mod silence;
mod split;
mod tables;
mod typing;

pub use markdown::{escape_markdown_v2, to_markdown_v2};
pub use media::{MediaFile, MediaReply, extract_media};
pub use plain::strip_markdown_v2;
pub use send::{
    Notifications, OutgoingText, ParseMode, ReplyToMode, SendOptions, SendPlan,
    is_markup_rejection, is_reply_target_missing, plan_reply,
};
pub use silence::is_intentional_silence;
pub use split::{MAX_MESSAGE_LEN, split_markdown_v2, split_message, utf16_len};
pub use typing::{
    TYPING_COOLDOWN, TYPING_INTERVAL, TYPING_SEND_TIMEOUT, TypingFailure, TypingRefresh,
};
