// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The readers of submitted turns (ADR-0034): one `follow` thread per chat reads each turn's
//! C6 `turn_events` in the order the turns were accepted and renders them through the
//! [`Outbox`].

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

use ulei_contracts::generated::c6::{TurnEventsRequest, TurnOutcome, TurnState};

use super::link::ENGINE_FAILED;
use super::outbound::{Closed, Outbox};
use crate::c6::{self, Cancel, Event, session_key};
use crate::render::is_intentional_silence;

/// The answer of a model that gave no visible text after its retries.
pub(super) const EMPTY_SENTINEL: &str = "(empty)";

/// What the owner sees instead of the `(empty)` answer of a model that gave no visible text.
pub const EMPTY_ANSWER: &str = "⚠️ The model returned no response after processing tool results. \
     This can happen with some models — try again or rephrase your question.";

/// A turn the engine accepted; its answers reply to `reply_to`.
struct Followed {
    turn: String,
    reply_to: Option<i64>,
}

/// The `follow` thread of each chat that submitted a turn.
pub(super) struct Followers {
    client: c6::Client,
    outbox: Outbox,
    chats: HashMap<i64, Reader>,
}

struct Reader {
    turns: mpsc::Sender<Followed>,
    cancel: Cancel,
    thread: JoinHandle<()>,
}

impl Followers {
    pub(super) fn new(client: c6::Client, outbox: Outbox) -> Self {
        Self {
            client,
            outbox,
            chats: HashMap::new(),
        }
    }

    /// Queues the accepted `turn` of `chat_id` for the chat's reader; its answers reply to
    /// `reply_to`.
    ///
    /// # Errors
    /// The chat's reader thread could not be spawned.
    pub(super) fn follow(
        &mut self,
        chat_id: i64,
        reply_to: Option<i64>,
        turn: String,
    ) -> io::Result<()> {
        let reader = match self.chats.entry(chat_id) {
            Entry::Occupied(reader) => reader.into_mut(),
            Entry::Vacant(slot) => {
                let (turns, inbox) = mpsc::channel();
                let cancel = Cancel::new();
                let mut follow = Follow {
                    client: self.client.clone(),
                    outbox: self.outbox.clone(),
                    cancel: cancel.clone(),
                    chat_id,
                };
                let thread = thread::Builder::new()
                    .name("follow".into())
                    .spawn(move || follow.run(&inbox))?;
                slot.insert(Reader {
                    turns,
                    cancel,
                    thread,
                })
            }
        };
        // A reader ends early only when the delivery threads are gone: nothing could be shown.
        let _ = reader.turns.send(Followed { turn, reply_to });
        Ok(())
    }

    /// Ends every reader at once, a held `turn_events` included, and waits for the threads;
    /// turns not read to their end stay unread.
    pub(super) fn stop(self) {
        for reader in self.chats.values() {
            reader.cancel.cancel();
        }
        for (chat_id, reader) in self.chats {
            drop(reader.turns);
            if reader.thread.join().is_err() {
                eprintln!("ulei-tg: reader of chat {chat_id} ended abnormally");
            }
        }
    }
}

struct Follow {
    client: c6::Client,
    outbox: Outbox,
    cancel: Cancel,
    chat_id: i64,
}

/// How reading one turn ended.
pub(super) enum Read {
    /// `end`, `dropped`, or no more events.
    Done,
    /// The engine went away or the turn is gone from it.
    Lost,
    /// The reader was cancelled or the delivery threads are gone.
    Stopped,
}

impl Follow {
    fn run(&mut self, inbox: &Receiver<Followed>) {
        for followed in inbox {
            let mut typing = false;
            let read = self.read(&followed, &mut typing);
            let ended = if typing {
                self.outbox.turn_ended(self.chat_id)
            } else {
                Ok(())
            };
            let noticed = match read {
                Read::Lost => self.reply(followed.reply_to, ENGINE_FAILED),
                Read::Done | Read::Stopped => Ok(()),
            };
            if matches!(read, Read::Stopped) || ended.and(noticed).is_err() {
                return;
            }
        }
    }

    /// Reads the events of `followed` to the turn's end; `typing` holds while its indicator
    /// runs.
    fn read(&self, followed: &Followed, typing: &mut bool) -> Read {
        read_turn(
            &self.client,
            &self.cancel,
            self.chat_id,
            &followed.turn,
            |event| self.render(event, followed.reply_to, typing),
        )
    }

    /// Shows one event to the owner; true when it ends the turn.
    fn render(
        &self,
        event: &Event,
        reply_to: Option<i64>,
        typing: &mut bool,
    ) -> Result<bool, Closed> {
        match event {
            Event::Start => {
                self.outbox.turn_started(self.chat_id)?;
                *typing = true;
                Ok(false)
            }
            Event::Interim(text) => {
                self.outbox
                    .deliver(self.chat_id, None, text.as_str(), false)?;
                Ok(false)
            }
            Event::Final(text) => {
                self.stop_typing(typing)?;
                if is_intentional_silence(text) {
                    return Ok(false);
                }
                let answer = if text == EMPTY_SENTINEL {
                    EMPTY_ANSWER
                } else {
                    text
                };
                self.reply(reply_to, answer)?;
                Ok(false)
            }
            Event::End { outcome, line } => {
                self.stop_typing(typing)?;
                if let Some(text) = outcome_text(*outcome, line.as_deref()) {
                    self.reply(reply_to, text)?;
                }
                Ok(true)
            }
            Event::Dropped => {
                self.stop_typing(typing)?;
                Ok(true)
            }
        }
    }

    fn stop_typing(&self, typing: &mut bool) -> Result<(), Closed> {
        if std::mem::take(typing) {
            self.outbox.turn_ended(self.chat_id)?;
        }
        Ok(())
    }

    fn reply(&self, reply_to: Option<i64>, text: impl Into<String>) -> Result<(), Closed> {
        self.outbox.deliver(self.chat_id, reply_to, text, true)
    }
}

/// Reads the events of `turn` of the session of `chat_id` in order and hands each to `each`,
/// which says whether it ends the turn, until the turn's end or a failure.
pub(super) fn read_turn(
    client: &c6::Client,
    cancel: &Cancel,
    chat_id: i64,
    turn: &str,
    mut each: impl FnMut(&Event) -> Result<bool, Closed>,
) -> Read {
    let mut after = 0;
    loop {
        let request = TurnEventsRequest {
            session: session_key(chat_id),
            turn: turn.to_owned(),
            after,
        };
        let page = match client.turn_events(&request, cancel) {
            Ok(page) => page,
            // ADR-0031 section 2: a caller that bounds its wait calls again.
            Err(c6::Error::Timeout) => continue,
            Err(c6::Error::Cancelled) => return Read::Stopped,
            Err(e) => {
                eprintln!("ulei-tg: reading turn {turn} of chat {chat_id} failed: {e}");
                return Read::Lost;
            }
        };
        if page.events.is_empty() {
            match page.state {
                TurnState::Unknown => {
                    eprintln!("ulei-tg: turn {turn} of chat {chat_id} is unknown to the engine");
                    return Read::Lost;
                }
                TurnState::Ended | TurnState::Dropped => return Read::Done,
                TurnState::Queued | TurnState::Running => continue,
            }
        }
        for event in &page.events {
            after += 1;
            match each(event) {
                Ok(false) => {}
                Ok(true) => return Read::Done,
                Err(Closed) => return Read::Stopped,
            }
        }
    }
}

/// The owner's text for how a turn ended, the reference's normalisation of an empty answer:
/// the quota line as is, the failure texts for `failed` and `context_exhausted`, nothing for
/// the outcomes whose answer is `final` and for `interrupted`.
fn outcome_text(outcome: TurnOutcome, line: Option<&str>) -> Option<String> {
    match outcome {
        TurnOutcome::QuotaExhausted => line.map(str::to_owned),
        TurnOutcome::Failed | TurnOutcome::ContextExhausted => line.map(failure_text),
        TurnOutcome::Completed
        | TurnOutcome::MaxSteps
        | TurnOutcome::Halted
        | TurnOutcome::Interrupted => None,
    }
}

/// The failed-run branch of the reference's empty-answer normalisation over the turn's line.
fn failure_text(error: &str) -> String {
    let lower = error.to_lowercase();
    if lower.contains("session storage") {
        return if lower.contains("disk") {
            "⚠️ Session storage was temporarily unavailable, so this turn was stopped to protect \
             your conversation history. Please check available disk space, then send your \
             message again."
        } else {
            "⚠️ Session storage was temporarily unavailable, so this turn was stopped to protect \
             your conversation history. Your message should already be saved — please send it \
             again in a moment."
        }
        .to_owned();
    }
    let context = [
        "context",
        "token",
        "too large",
        "too long",
        "exceed",
        "payload",
    ];
    if context.iter().any(|p| lower.contains(p)) {
        return "⚠️ Session too large for the model's context window.\n\
                Use /compact to compress the conversation, or /reset to start fresh."
            .to_owned();
    }
    let head: String = error.chars().take(300).collect();
    format!("The request failed: {head}\nTry again or use /reset to start a fresh session.")
}
