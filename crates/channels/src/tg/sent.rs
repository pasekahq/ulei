//! The index of the messages the bot sent, the ingress's [`MessageTexts`] (ADR-0032 section 7).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use crate::ingress::{Author, MessageTexts, REACTED_TEXT_CHARS};

/// Messages the index keeps; the oldest goes first.
const SENT_KEPT: usize = 256;

/// The last [`SENT_KEPT`] messages of the bot and of the owner by chat and id, shared by the
/// sending thread and the ingress (ADR-0088: the owner's too, so a reaction to the owner's own
/// message shows its text).
#[derive(Clone, Debug, Default)]
pub struct SentTexts(Arc<Mutex<VecDeque<Sent>>>);

#[derive(Debug)]
struct Sent {
    chat_id: i64,
    message_id: i64,
    text: String,
    author: Author,
}

impl SentTexts {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records message `message_id` the bot sent to `chat_id` with the plain `text` it shows,
    /// cut to what a reaction keeps of it.
    pub(crate) fn record(&self, chat_id: i64, message_id: i64, text: &str) {
        self.keep(chat_id, message_id, text, Author::Bot);
    }

    fn keep(&self, chat_id: i64, message_id: i64, text: &str, author: Author) {
        let mut sent = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if sent.len() == SENT_KEPT {
            sent.pop_front();
        }
        sent.push_back(Sent {
            chat_id,
            message_id,
            text: text.chars().take(REACTED_TEXT_CHARS).collect(),
            author,
        });
    }
}

impl MessageTexts for SentTexts {
    fn text_of(&self, chat_id: i64, message_id: i64) -> Option<(String, Author)> {
        let sent = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        sent.iter()
            .rev()
            .find(|s| s.chat_id == chat_id && s.message_id == message_id)
            .map(|s| (s.text.clone(), s.author))
    }

    fn record_owner(&self, chat_id: i64, message_id: i64, text: &str) {
        self.keep(chat_id, message_id, text, Author::Owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_and_bot_messages_are_found_with_their_author_and_an_edit_wins() {
        let texts = SentTexts::new();
        texts.record(1, 10, "bot reply");
        texts.record_owner(1, 11, "owner request");
        texts.record_owner(1, 11, "owner request, edited");
        assert_eq!(
            texts.text_of(1, 10),
            Some(("bot reply".to_owned(), Author::Bot))
        );
        assert_eq!(
            texts.text_of(1, 11),
            Some(("owner request, edited".to_owned(), Author::Owner))
        );
        assert_eq!(texts.text_of(2, 11), None);
    }
}
