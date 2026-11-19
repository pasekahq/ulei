// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Owner reactions: normalised, remembered until the next turn shows them to the agent as
//! context, and a delayed wake-up turn when the owner reacted without writing.

use std::fmt::Write as _;
use std::time::Duration;

use super::update::MessageReactionUpdated;

/// Who wrote a message the owner reacted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Author {
    /// The bot (the agent's reply).
    Bot,
    /// The owner.
    Owner,
}

/// Looks up the text of a chat message by its Telegram id.
///
/// Seam: the outbound renderer's index of sent messages and the engine's session journal (C8).
pub trait MessageTexts {
    /// Text and author of message `message_id` in `chat_id`, when known.
    fn text_of(&self, chat_id: i64, message_id: i64) -> Option<(String, Author)>;
}

/// Texts and delay of the reaction wake-up; deployment wording comes from config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionConfig {
    /// Wait after the first reaction before waking the agent, collecting a batch.
    pub wake_delay: Duration,
    /// User message of the wake-up turn (a system event, not the owner's words).
    pub wake_text: String,
    /// First line of the reaction context block.
    pub context_header: String,
}

impl Default for ReactionConfig {
    fn default() -> Self {
        Self {
            wake_delay: Duration::from_secs(15),
            wake_text: "[Системное событие, не сообщение владельца: владелец поставил реакцию в Telegram. \
                        Детали — в блоке реакций ниже. Если реакция ничего не требует (просто «ок», \
                        лайк на отчёт) — ответь ровно NO_REPLY. Пиши владельцу, только если реакция \
                        явно ждёт действия.]"
                .to_owned(),
            context_header: "Реакции владельца в Telegram с прошлого хода (текстом не писали, \
                             только поставили реакцию):"
                .to_owned(),
        }
    }
}

/// Characters of the reacted-to message's text a reaction keeps.
pub(crate) const REACTED_TEXT_CHARS: usize = 300;

/// One owner reaction as remembered for the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReactionRecord {
    /// Local time `yyyy-mm-ddTHH:MM`.
    pub(crate) ts: String,
    pub(crate) chat_id: i64,
    pub(crate) message_id: i64,
    pub(crate) emojis: Vec<String>,
    pub(crate) custom_emoji_ids: Vec<String>,
    pub(crate) text: Option<(String, Author)>,
}

impl ReactionRecord {
    /// Normalises a reaction update: at most 64 reactions, emoji cut to 64 characters, custom
    /// emoji ids to 128.
    pub(crate) fn new(
        update: &MessageReactionUpdated,
        ts: String,
        text: Option<(String, Author)>,
    ) -> Self {
        let mut emojis = Vec::new();
        let mut custom_emoji_ids = Vec::new();
        for r in update.new_reaction.iter().take(64) {
            if let Some(e) = r.emoji.as_deref().filter(|e| !e.is_empty()) {
                emojis.push(e.chars().take(64).collect());
            }
            if let Some(id) = &r.custom_emoji_id {
                custom_emoji_ids.push(id.chars().take(128).collect());
            }
        }
        let text = text
            .map(|(t, a)| (t.chars().take(REACTED_TEXT_CHARS).collect::<String>(), a))
            .filter(|(t, _)| !t.is_empty());
        Self {
            ts,
            chat_id: update.chat.id,
            message_id: update.message_id,
            emojis,
            custom_emoji_ids,
            text,
        }
    }

    /// A reaction was set (not only removed); only that wakes the agent.
    pub(crate) fn wakes(&self) -> bool {
        !self.emojis.is_empty() || !self.custom_emoji_ids.is_empty()
    }

    fn line(&self) -> String {
        let mut em = self.emojis.concat();
        if !self.custom_emoji_ids.is_empty() {
            let _ = write!(
                em,
                " премиум-эмодзи (id {}, картинку не вижу)",
                self.custom_emoji_ids.join(",")
            );
        }
        let em = match em.trim() {
            "" => "(снял реакцию)".to_owned(),
            t => t.to_owned(),
        };
        let id = self.message_id;
        let what = match &self.text {
            Some((text, author)) => {
                let who = match author {
                    Author::Bot => "моё сообщение",
                    Author::Owner => "его же сообщение",
                };
                let flat: String = text
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(200)
                    .collect();
                format!("{who} #{id} «{flat}»")
            }
            None => format!("сообщение #{id} (текст не найден, id {id})"),
        };
        let ts: String = self.ts.chars().take(16).collect();
        format!("- {ts} {em} на {what}")
    }
}

/// How many unseen reactions a context block shows: the last ten, as the reference does.
pub(crate) const UNSEEN_SHOWN: usize = 10;

/// The context block for the reactions not yet shown (the last ten of them).
pub(crate) fn context_block(header: &str, unseen: &[ReactionRecord]) -> Option<String> {
    if unseen.is_empty() {
        return None;
    }
    let from = unseen.len().saturating_sub(UNSEEN_SHOWN);
    let lines: Vec<String> = unseen[from..].iter().map(ReactionRecord::line).collect();
    Some(format!("{header}\n{}", lines.join("\n")))
}
