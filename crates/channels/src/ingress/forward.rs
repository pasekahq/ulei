// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Labels of forwarded messages: `[Переслано от <author>, dd.mm.yyyy HH:MM]` in the
//! configured zone, and the rendering of batches that mix forwarded and own chunks.

use super::localtime::Zone;
use super::update::{Chat, Message, User};

const UNKNOWN_SENDER: &str = "неизвестного отправителя";
const OWN_CHUNK_MARKER: &str = "[Сообщение владельца]";

fn person_label(user: &User) -> String {
    let name = user.full_name();
    let name = name.trim();
    match user.username.as_deref().filter(|u| !u.is_empty()) {
        Some(username) if name.is_empty() => format!("@{username}"),
        Some(username) => format!("{name} (@{username})"),
        None if name.is_empty() => format!("id {}", user.id),
        None => name.to_owned(),
    }
}

fn chat_label(chat: &Chat, signature: Option<&str>) -> String {
    let title = chat
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .or_else(|| chat.full_name());
    let username = chat.username.as_deref().filter(|u| !u.is_empty());
    let label = match (title, username) {
        (Some(title), Some(username)) => format!("{title} (@{username})"),
        (Some(title), None) => title,
        (None, Some(username)) => format!("@{username}"),
        (None, None) => format!("чат {}", chat.id),
    };
    match signature.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sig) => format!("{label}, автор {sig}"),
        None => label,
    }
}

/// `[Переслано от X, dd.mm.yyyy HH:MM]` for a forwarded message, `None` otherwise.
///
/// Reads `forward_origin` and falls back to the legacy `forward_*` fields.
pub(crate) fn forward_label(message: &Message, zone: &Zone) -> Option<String> {
    let (author, date) = if let Some(origin) = &message.forward_origin {
        let kind = origin.kind.as_str();
        if !matches!(kind, "user" | "hidden_user" | "chat" | "channel") && origin.date.is_none() {
            return None;
        }
        let author = match kind {
            "user" => origin.sender_user.as_ref().map(person_label),
            "hidden_user" => origin.sender_user_name.clone().filter(|n| !n.is_empty()),
            "chat" => origin
                .sender_chat
                .as_ref()
                .map(|c| chat_label(c, origin.author_signature.as_deref())),
            "channel" => origin
                .chat
                .as_ref()
                .map(|c| chat_label(c, origin.author_signature.as_deref())),
            _ => None,
        };
        (author, origin.date)
    } else {
        let date = message.forward_date?;
        let author = if let Some(user) = &message.forward_from {
            Some(person_label(user))
        } else if let Some(name) = message
            .forward_sender_name
            .clone()
            .filter(|n| !n.is_empty())
        {
            Some(name)
        } else {
            message
                .forward_from_chat
                .as_ref()
                .map(|c| chat_label(c, message.forward_signature.as_deref()))
        };
        (author, Some(date))
    };
    let author = author.unwrap_or_else(|| UNKNOWN_SENDER.to_owned());
    Some(match date {
        Some(date) => format!("[Переслано от {author}, {}]", zone.local(date).dmy_hm()),
        None => format!("[Переслано от {author}]"),
    })
}

/// Text of a batch with at least one forwarded chunk: forwarded chunks keep their own label,
/// each run of own chunks is opened by `[Сообщение владельца]`; chunks are joined by a blank line.
pub(crate) fn render_forward_batch(chunks: &[(bool, String)]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut prev_forwarded: Option<bool> = None;
    for (forwarded, text) in chunks {
        if text.is_empty() {
            continue;
        }
        if *forwarded {
            parts.push(text.clone());
        } else if prev_forwarded == Some(false) && !parts.is_empty() {
            if let Some(last) = parts.last_mut() {
                last.push('\n');
                last.push_str(text);
            }
        } else {
            parts.push(format!("{OWN_CHUNK_MARKER}\n{text}"));
        }
        prev_forwarded = Some(*forwarded);
    }
    parts.join("\n\n")
}
