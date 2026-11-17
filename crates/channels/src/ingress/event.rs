// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! One normalised inbound message before coalescing, and the rules that merge several of them.

use super::forward::render_forward_batch;
use super::media::Attachment;

/// What a message (or a merged turn) mainly is; drives how attachments become text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// Text (also mixed media).
    Text,
    /// Location or venue pin.
    Location,
    /// Photo or image document.
    Photo,
    /// Video.
    Video,
    /// Audio file (never transcribed).
    Audio,
    /// Voice note (transcribed).
    Voice,
    /// Any other file.
    Document,
    /// Sticker.
    Sticker,
    /// Slash command.
    Command,
}

/// The message a turn replies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyContext {
    /// Id of the replied-to message.
    pub message_id: i64,
    /// Its text (or the quoted part), when known.
    pub text: Option<String>,
}

/// One inbound message, normalised.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Event {
    pub(crate) chat_id: i64,
    pub(crate) message_id: i64,
    pub(crate) update_id: i64,
    pub(crate) date: i64,
    pub(crate) kind: MessageKind,
    pub(crate) text: String,
    pub(crate) attachments: Vec<Attachment>,
    pub(crate) reply: Option<ReplyContext>,
    pub(crate) forwarded: bool,
    /// `(forwarded, text)` of each message merged by the text batcher.
    pub(crate) chunks: Option<Vec<(bool, String)>>,
    pub(crate) last_chunk_len: usize,
}

impl Event {
    /// Text starting with `/` counts as a command and never coalesces.
    pub(crate) fn is_command(&self) -> bool {
        self.text.trim_start().starts_with('/')
    }

    fn own_chunks(&self) -> Vec<(bool, String)> {
        self.chunks
            .clone()
            .unwrap_or_else(|| vec![(self.forwarded, self.text.clone())])
    }

    /// Text batcher: appends a follow-up message of the same chat.
    pub(crate) fn append_text_chunk(&mut self, next: Event) {
        let mut chunks = self
            .chunks
            .take()
            .unwrap_or_else(|| vec![(false, self.text.clone())]);
        chunks.push((next.forwarded, next.text.clone()));
        if chunks.iter().any(|(fwd, _)| *fwd) {
            self.text = render_forward_batch(&chunks);
        } else if !next.text.is_empty() {
            self.text = if self.text.is_empty() {
                next.text.clone()
            } else {
                format!("{}\n{}", self.text, next.text)
            };
        }
        self.chunks = Some(chunks);
        self.last_chunk_len = next.text.chars().count();
        self.attachments.extend(next.attachments);
    }

    /// Album and photo-burst batcher: adds the attachments and a new caption.
    pub(crate) fn append_media_item(&mut self, next: Event) {
        self.attachments.extend(next.attachments);
        if !next.text.is_empty() {
            self.text = merge_caption(&self.text, &next.text);
        }
    }
}

/// Adds a caption unless it already is one of the blank-line separated captions.
pub(crate) fn merge_caption(existing: &str, new: &str) -> String {
    if existing.is_empty() {
        return new.to_owned();
    }
    if existing.split("\n\n").any(|c| c.trim() == new.trim()) {
        return existing.to_owned();
    }
    format!("{existing}\n\n{new}").trim().to_owned()
}

/// Whether `next` may join the pending turn made of `parts`.
pub(crate) fn can_coalesce(parts: &[Event], next: &Event) -> bool {
    if next.is_command() || parts.iter().any(Event::is_command) {
        return false;
    }
    // Audio files are typed per message, so they never mix with other media.
    let mut media_kinds: Vec<MessageKind> = parts
        .iter()
        .filter(|p| !p.attachments.is_empty())
        .map(|p| p.kind)
        .collect();
    if !next.attachments.is_empty() {
        media_kinds.push(next.kind);
    }
    media_kinds.sort_by_key(|k| *k as u8);
    media_kinds.dedup();
    !(media_kinds.contains(&MessageKind::Audio) && media_kinds.len() > 1)
}

/// One event from the coalesced parts, in arrival order.
pub(crate) fn merge_coalesced(mut parts: Vec<Event>) -> Event {
    if parts.len() == 1 {
        return parts.remove(0);
    }
    let chunks: Vec<(bool, String)> = parts.iter().flat_map(Event::own_chunks).collect();
    let text = if chunks.iter().any(|(fwd, _)| *fwd) {
        render_forward_batch(&chunks)
    } else {
        chunks
            .iter()
            .map(|(_, t)| t.as_str())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let mut all_kinds: Vec<MessageKind> = parts.iter().map(|p| p.kind).collect();
    all_kinds.dedup();
    let mut media_kinds: Vec<MessageKind> = parts
        .iter()
        .filter(|p| !p.attachments.is_empty())
        .map(|p| p.kind)
        .collect();
    media_kinds.sort_by_key(|k| *k as u8);
    media_kinds.dedup();
    let first_kind = parts[0].kind;
    let kind = if all_kinds.iter().all(|k| *k == first_kind) {
        first_kind
    } else if media_kinds.len() == 1 {
        media_kinds[0]
    } else {
        MessageKind::Text
    };
    let reply = parts.iter().find_map(|p| p.reply.clone());
    let forwarded = parts.iter().any(|p| p.forwarded);
    let last = &parts[parts.len() - 1];
    let (message_id, update_id) = (last.message_id, last.update_id);
    let attachments = parts
        .iter()
        .flat_map(|p| p.attachments.iter().cloned())
        .collect();
    let first = parts.swap_remove(0);
    Event {
        text,
        kind,
        attachments,
        message_id,
        update_id,
        reply,
        forwarded,
        chunks: Some(chunks),
        ..first
    }
}
