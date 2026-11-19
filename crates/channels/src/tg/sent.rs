//! The index of the messages the bot sent, the ingress's [`MessageTexts`] (ADR-0032 section 7).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use crate::ingress::{Author, MessageTexts, REACTED_TEXT_CHARS};

/// Messages the index keeps; the oldest goes first.
const SENT_KEPT: usize = 256;

/// The last [`SENT_KEPT`] bot messages by chat and id, shared by the sending thread and the
/// ingress. Owner messages are never recorded.
#[derive(Clone, Debug, Default)]
pub struct SentTexts(Arc<Mutex<VecDeque<Sent>>>);

#[derive(Debug)]
struct Sent {
    chat_id: i64,
    message_id: i64,
    text: String,
}

impl SentTexts {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records message `message_id` the bot sent to `chat_id` with the plain `text` it shows,
    /// cut to what a reaction keeps of it.
    pub(crate) fn record(&self, chat_id: i64, message_id: i64, text: &str) {
        let mut sent = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if sent.len() == SENT_KEPT {
            sent.pop_front();
        }
        sent.push_back(Sent {
            chat_id,
            message_id,
            text: text.chars().take(REACTED_TEXT_CHARS).collect(),
        });
    }
}

impl MessageTexts for SentTexts {
    fn text_of(&self, chat_id: i64, message_id: i64) -> Option<(String, Author)> {
        let sent = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        sent.iter()
            .rev()
            .find(|s| s.chat_id == chat_id && s.message_id == message_id)
            .map(|s| (s.text.clone(), Author::Bot))
    }
}
