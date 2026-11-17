// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The send plan of an agent reply: which `sendMessage` calls to make, in order, and how to
//! react to Telegram rejecting one. The transport executes it; nothing here does I/O.

use super::markdown::to_markdown_v2;
use super::plain::strip_markdown_v2;
use super::split::split_markdown_v2;

/// Which chunks of a reply thread to the message being answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplyToMode {
    /// Never set `reply_to_message_id`.
    Off,
    /// Only the first chunk replies.
    #[default]
    First,
    /// Every chunk replies.
    All,
}

/// Whether messages ring the owner's phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Notifications {
    /// Every message notifies.
    All,
    /// Messages are delivered silently unless the reply is marked to notify (the final answer).
    #[default]
    Important,
}

/// Channel settings that shape outgoing text messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SendOptions {
    /// Reply threading of multi-chunk replies.
    pub reply_to_mode: ReplyToMode,
    /// Send every message with link previews disabled.
    pub disable_link_previews: bool,
    /// Notification policy.
    pub notifications: Notifications,
}

/// `parse_mode` of a `sendMessage` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMode {
    /// `MarkdownV2`.
    MarkdownV2,
}

impl ParseMode {
    /// The Bot API value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MarkdownV2 => "MarkdownV2",
        }
    }
}

/// One `sendMessage` call: `text` with `parse_mode` (none = plain text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingText {
    /// Message text, at most 4096 UTF-16 units (except replies split into 100+ chunks, see
    /// [`super::split_message`]).
    pub text: String,
    /// Parse mode; `None` sends plain text.
    pub parse_mode: Option<ParseMode>,
    /// `reply_to_message_id`.
    pub reply_to: Option<i64>,
    /// `link_preview_options.is_disabled`.
    pub disable_link_preview: bool,
    /// `disable_notification`.
    pub disable_notification: bool,
}

impl OutgoingText {
    /// The same message as plain text, to resend after Telegram rejected the markup
    /// (see [`is_markup_rejection`]).
    #[must_use]
    pub fn plain_fallback(&self) -> Self {
        Self {
            text: strip_markdown_v2(&self.text),
            parse_mode: None,
            ..self.clone()
        }
    }
}

/// Messages for one agent reply plus what to do once all are delivered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SendPlan {
    /// `sendMessage` calls in order; empty for a blank reply.
    pub messages: Vec<OutgoingText>,
    /// Send a `typing` chat action after the last message: Telegram clears the indicator on
    /// delivery, and an intermediate reply means the agent is still working. Skip it while
    /// [`super::TypingRefresh::in_cooldown`] and report its outcome with
    /// [`super::TypingRefresh::record`].
    pub retrigger_typing: bool,
}

/// Plans the delivery of `content` (the agent's Markdown). `reply_to` is the owner message
/// being answered; `notify` marks the final answer of a turn.
pub fn plan_reply(
    content: &str,
    reply_to: Option<i64>,
    notify: bool,
    options: &SendOptions,
) -> SendPlan {
    if content.trim_matches(super::pytext::is_space).is_empty() {
        return SendPlan::default();
    }
    let disable_notification = options.notifications == Notifications::Important && !notify;
    let messages = split_markdown_v2(&to_markdown_v2(content))
        .into_iter()
        .enumerate()
        .map(|(i, text)| OutgoingText {
            text,
            parse_mode: Some(ParseMode::MarkdownV2),
            reply_to: reply_to.filter(|_| thread_reply(options.reply_to_mode, i)),
            disable_link_preview: options.disable_link_previews,
            disable_notification,
        })
        .collect();
    SendPlan {
        messages,
        retrigger_typing: !notify,
    }
}

fn thread_reply(mode: ReplyToMode, chunk_index: usize) -> bool {
    match mode {
        ReplyToMode::Off => false,
        ReplyToMode::All => true,
        ReplyToMode::First => chunk_index == 0,
    }
}

/// True when a failed `MarkdownV2` send of `sent_text` should be retried once as
/// [`OutgoingText::plain_fallback`]: the error mentions parsing or markdown, or the text
/// carries custom emoji the bot may not use.
pub fn is_markup_rejection(error: &str, sent_text: &str) -> bool {
    let error = error.to_lowercase();
    error.contains("parse")
        || error.contains("markdown")
        || (sent_text.contains("tg://emoji?id=")
            && ["emoji", "premium", "not enough rights"]
                .iter()
                .any(|w| error.contains(w)))
}

/// True when Telegram refused a reply because the replied-to message is gone; the message is
/// then resent without `reply_to`.
pub fn is_reply_target_missing(error: &str) -> bool {
    error
        .to_lowercase()
        .contains("message to be replied not found")
}
