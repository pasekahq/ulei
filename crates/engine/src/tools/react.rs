// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The `react_to_message` tool: the agent's emoji reaction on an owner message of the channel
//! (ADR-0088). The engine holds no channel credential, so the tool records the reaction and the
//! turn hands it to the channel as a C6 `reaction` event, which the channel sets on the message.
//! Without `message_id` it reacts to the message the turn answers; it never infers an id.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::{Args, Context, Output, REACT_TO_MESSAGE, json_object, json_str, object_schema};

/// Most reactions one turn records; a later call fails.
pub const MAX_REACTIONS: usize = 16;
/// Most characters of an emoji.
const MAX_EMOJI_CHARS: usize = 64;

const DESCRIPTION: &str = "React to an owner message in the chat with a single emoji, the way \
    a person would tapback. Reach for it when a reaction is what a person would do: a plan you \
    are on board with gets a 👍, a small verified task that needs no text reply gets a 👍 or ✅ \
    on the owner's original request, and then the reply can be exactly NO_REPLY. Use it like a \
    person would: occasionally, not on every message, and never as a status signal. NEVER \
    narrate or explain a reaction. Without `message_id` it reacts to the owner message this \
    turn answers. Pass `message_id` only with an id you saw for an owner message (the \
    `[message_id: N]` line of an owner message, or the `#N` of a reaction line); never guess \
    or compute an id, and never react to an event. A different emoji replaces your earlier \
    reaction on that message. The reaction is set when the turn ends.";

const NO_SOURCE: &str =
    "No source message id; find the original owner message first and pass its message_id";

/// One reaction a turn recorded: the channel's message id and the emoji.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    /// The channel's id of the message.
    pub message_id: u64,
    /// The emoji.
    pub emoji: String,
}

/// The reactions of one turn, shared by its tool and the runner that hands them to the server.
#[derive(Debug, Clone, Default)]
pub struct Reactions(Arc<Mutex<Vec<Reaction>>>);

impl Reactions {
    /// The recorded reactions in call order; the record is empty afterwards.
    pub fn take(&self) -> Vec<Reaction> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn push(&self, reaction: Reaction) -> Result<(), String> {
        let mut held = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.len() >= MAX_REACTIONS {
            return Err(format!(
                "at most {MAX_REACTIONS} reactions per turn; this one was not set"
            ));
        }
        held.push(reaction);
        Ok(())
    }
}

/// The tool of one turn: the message the turn answers, when it has one, and the turn's record.
pub struct ReactToMessage {
    source: Option<u64>,
    reactions: Reactions,
}

impl ReactToMessage {
    /// The tool of a turn answering message `source` of the channel (none for an event or a
    /// wake-up), recording into `reactions`.
    pub fn new(source: Option<u64>, reactions: Reactions) -> Self {
        Self { source, reactions }
    }

    fn call(&self, args: &Args<'_>) -> Result<Output, String> {
        let emoji = args.str("emoji")?.trim();
        if emoji.is_empty() {
            return Err("emoji is required: one emoji such as 👍".to_owned());
        }
        if emoji.chars().count() > MAX_EMOJI_CHARS || emoji.chars().any(char::is_whitespace) {
            return Err("emoji must be a single emoji such as 👍".to_owned());
        }
        let message_id = match message_id(args)? {
            Some(id) => id,
            None => self.source.ok_or_else(|| NO_SOURCE.to_owned())?,
        };
        self.reactions.push(Reaction {
            message_id,
            emoji: emoji.to_owned(),
        })?;
        Ok(Output::ok(json_object([
            ("success", "true".to_owned()),
            ("message_id", message_id.to_string()),
            ("emoji", json_str(emoji)),
        ])))
    }
}

/// The explicit `message_id`: an integer, or a string of digits, at least 1.
fn message_id(args: &Args<'_>) -> Result<Option<u64>, String> {
    let invalid = || "message_id must be a positive integer message id".to_owned();
    let Some(value) = args.present("message_id") else {
        return Ok(None);
    };
    let id = match value {
        Value::Number(n) => n.as_u64().ok_or_else(invalid)?,
        Value::String(s) if !s.trim().is_empty() => {
            s.trim().parse::<u64>().map_err(|_| invalid())?
        }
        _ => return Err(invalid()),
    };
    if id == 0 || i64::try_from(id).is_err() {
        return Err(invalid());
    }
    Ok(Some(id))
}

impl super::Tool for ReactToMessage {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: REACT_TO_MESSAGE.into(),
            description: DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "emoji": {
                        "type": "string",
                        "description": "The emoji to react with (e.g. '👍', '❤️', '🔥')."
                    },
                    "message_id": {
                        "type": "integer",
                        "description": "Optional. The id of the owner message to react to, as shown in its `[message_id: N]` line. Omit to react to the message this turn answers, which is almost always what you want."
                    }
                }),
                &["emoji"],
            ),
            native: None,
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        self.call(&Args(input)).unwrap_or_else(Output::error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(tool: &ReactToMessage, input: Value) -> Output {
        let Value::Object(map) = input else {
            unreachable!("object input")
        };
        tool.call(&Args(&map)).unwrap_or_else(Output::error)
    }

    #[test]
    fn without_message_id_it_reacts_to_the_message_the_turn_answers() {
        let reactions = Reactions::default();
        let tool = ReactToMessage::new(Some(42), reactions.clone());
        let out = call(&tool, json!({"emoji": "👍"}));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            reactions.take(),
            [Reaction {
                message_id: 42,
                emoji: "👍".to_owned()
            }]
        );
        assert!(reactions.take().is_empty());
    }

    #[test]
    fn an_explicit_id_wins_and_a_digit_string_is_taken() {
        let reactions = Reactions::default();
        let tool = ReactToMessage::new(Some(42), reactions.clone());
        assert!(!call(&tool, json!({"emoji": "✅", "message_id": 7})).is_error);
        assert!(!call(&tool, json!({"emoji": "✅", "message_id": "8"})).is_error);
        let ids: Vec<u64> = reactions.take().iter().map(|r| r.message_id).collect();
        assert_eq!(ids, [7, 8]);
    }

    #[test]
    fn a_turn_without_a_source_message_needs_an_id() {
        let reactions = Reactions::default();
        let tool = ReactToMessage::new(None, reactions.clone());
        let out = call(&tool, json!({"emoji": "👍"}));
        assert!(out.is_error);
        assert!(
            out.content.contains("No source message id"),
            "{}",
            out.content
        );
        assert!(reactions.take().is_empty());
    }

    #[test]
    fn invalid_input_records_nothing() {
        let reactions = Reactions::default();
        let tool = ReactToMessage::new(Some(42), reactions.clone());
        for input in [
            json!({}),
            json!({"emoji": ""}),
            json!({"emoji": "👍 👎"}),
            json!({"emoji": "👍", "message_id": 0}),
            json!({"emoji": "👍", "message_id": -3}),
            json!({"emoji": "👍", "message_id": "abc"}),
            json!({"emoji": "👍", "message_id": 1.5}),
        ] {
            assert!(call(&tool, input.clone()).is_error, "{input}");
        }
        assert!(reactions.take().is_empty());
    }

    #[test]
    fn a_turn_records_at_most_sixteen() {
        let reactions = Reactions::default();
        let tool = ReactToMessage::new(Some(42), reactions.clone());
        for _ in 0..MAX_REACTIONS {
            assert!(!call(&tool, json!({"emoji": "👍"})).is_error);
        }
        assert!(call(&tool, json!({"emoji": "👍"})).is_error);
        assert_eq!(reactions.take().len(), MAX_REACTIONS);
    }
}
