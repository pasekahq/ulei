//! `turn-reminder`: a configured text appended to the request copy of the owner's input on a
//! top-level owner turn of the main channel; the journal and the system prompt never carry it.

use super::{Origin, Policy};

/// Gives its text as the turn context of a [`Origin::TopLevelOwner`] turn; an empty text is off.
#[derive(Debug, Clone)]
pub struct TurnReminder {
    text: String,
}

impl TurnReminder {
    /// The policy with the reminder `text`, held for the life of the process.
    pub fn new(text: String) -> Self {
        Self { text }
    }
}

impl Policy for TurnReminder {
    fn name(&self) -> &'static str {
        "turn-reminder"
    }

    fn turn_context(&self, origin: Origin) -> &str {
        if origin == Origin::TopLevelOwner {
            &self.text
        } else {
            ""
        }
    }
}
