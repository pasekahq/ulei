//! Agent reply -> Telegram: Markdown to `MarkdownV2` with a plain-text fallback, splitting at
//! Telegram's 4096 UTF-16 unit limit and the per-reply send plan.
//!
//! Pure and deterministic: the transport executes the plans.

mod markdown;
mod plain;
mod pytext;
mod send;
mod split;
mod tables;

pub use markdown::{escape_markdown_v2, to_markdown_v2};
pub use plain::strip_markdown_v2;
pub use send::{
    Notifications, OutgoingText, ParseMode, ReplyToMode, SendOptions, SendPlan,
    is_markup_rejection, is_reply_target_missing, plan_reply,
};
pub use split::{MAX_MESSAGE_LEN, split_markdown_v2, split_message, utf16_len};
