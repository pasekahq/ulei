// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The owner's inbox turns (ADR-0036 sections 6 and 7): the `inbox` thread holds C6
//! `inbox_turns` for the direct chat of each owner id, reads each handed-over turn quietly,
//! sends a consumed answer once and reports its `Delivery`.

use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ulei_contracts::generated::c6::{
    Delivery, DeliveryReport, ErrorCode, InboxTurns, InboxTurnsRequest, TurnOutcome,
};

use super::follow::{EMPTY_SENTINEL, Read, read_turn};
use super::outbound::{Closed, Outbox};
use crate::c6::{self, Event, session_key};
use crate::render::is_intentional_silence;
use crate::render::pytext::strip;
use crate::uds::Cancel;

/// Wait before a failed connect or a lost hold is tried again: the reference pump's period.
const RETRY: Duration = Duration::from_secs(2);

/// First wait before the owners are held again once the engine refused every one of them.
const REFUSED_FIRST: Duration = Duration::from_secs(5);

/// Longest wait between two rounds over refused owners: at most 12 log lines an hour.
const REFUSED_MAX: Duration = Duration::from_secs(300);

/// The silence marker of an event handled with nothing to tell the owner.
const DONE_MARKER: &str = "NO_REPLY_DONE";

/// Starts of the texts that stand in for a model that gave no answer.
const NO_TEXT_PREFIXES: [&str; 3] = [
    "\u{26a0}\u{fe0f} The model returned no response",
    "\u{26a0}\u{fe0f} The model produced only internal reasoning",
    "\u{274c} Model returned no content",
];

/// The running `inbox` thread.
pub struct Inbox {
    cancel: Cancel,
    stop: mpsc::Sender<()>,
    thread: JoinHandle<()>,
}

impl Inbox {
    /// Starts the `inbox` thread over `client`: it holds `inbox_turns` for the direct chat of
    /// each of `owner_ids` in turn, the next one once the engine refuses a key, and sends the
    /// answers through `outbox`. Once every key was refused it starts over after a wait that
    /// doubles from 5 s up to 5 min, so a refusal never ends the thread (and with it the
    /// owner's inbox deliveries) while the process keeps running.
    ///
    /// # Errors
    /// The thread could not be spawned.
    pub fn start(client: c6::Client, outbox: Outbox, owner_ids: &[i64]) -> io::Result<Self> {
        let cancel = Cancel::new();
        let (stop, stopped) = mpsc::channel();
        let holder = Holder {
            client,
            outbox,
            cancel: cancel.clone(),
            stopped,
        };
        let owners = owner_ids.to_vec();
        let thread = thread::Builder::new()
            .name("inbox".into())
            .spawn(move || holder.run(&owners))?;
        Ok(Self {
            cancel,
            stop,
            thread,
        })
    }

    /// Ends the thread at once, a held call included, and waits for it; a turn not read to its
    /// end is neither sent nor reported.
    ///
    /// # Errors
    /// [`Closed`] when the thread ended abnormally.
    pub fn finish(self) -> Result<(), Closed> {
        self.cancel.cancel();
        drop(self.stop);
        self.thread.join().map_err(|_| Closed)
    }
}

struct Holder {
    client: c6::Client,
    outbox: Outbox,
    cancel: Cancel,
    stopped: Receiver<()>,
}

/// How holding the key of one chat ended.
enum Held {
    /// The engine refused the key or released its hold.
    Refused,
    /// The thread is to end.
    Stopped,
}

impl Holder {
    fn run(&self, owners: &[i64]) {
        let mut wait = REFUSED_FIRST;
        loop {
            let round = Instant::now();
            for &chat_id in owners {
                match self.hold(chat_id) {
                    Held::Stopped => return,
                    Held::Refused => {}
                }
            }
            // A round that held for longer than the longest wait worked: the next refusal starts
            // the backoff over.
            if round.elapsed() > REFUSED_MAX {
                wait = REFUSED_FIRST;
            }
            eprintln!(
                "ulei-tg: the engine refused the inbox turns of every owner chat; \
                 holding again in {} s",
                wait.as_secs()
            );
            if !matches!(
                self.stopped.recv_timeout(wait),
                Err(RecvTimeoutError::Timeout)
            ) {
                return;
            }
            wait = next_wait(wait);
        }
    }

    /// Takes the inbox turns of `chat_id`'s session one by one until the engine refuses the key.
    fn hold(&self, chat_id: i64) -> Held {
        let request = InboxTurnsRequest {
            session: session_key(chat_id),
        };
        loop {
            let turn = match self.client.inbox_turns(&request, &self.cancel) {
                Ok(InboxTurns { turn: Some(turn) }) => turn,
                Ok(InboxTurns { turn: None }) => {
                    eprintln!("ulei-tg: inbox turns of chat {chat_id} taken by a newer hold");
                    return Held::Refused;
                }
                Err(c6::Error::Cancelled) => return Held::Stopped,
                Err(
                    c6::Error::Unreachable(_)
                    | c6::Error::Timeout
                    | c6::Error::Remote {
                        code: ErrorCode::Internal,
                        ..
                    },
                ) => {
                    if self.pause() {
                        continue;
                    }
                    return Held::Stopped;
                }
                Err(e) => {
                    eprintln!("ulei-tg: inbox turns of chat {chat_id} refused: {e}");
                    return Held::Refused;
                }
            };
            if !self.take(chat_id, &turn) {
                return Held::Stopped;
            }
        }
    }

    /// Waits [`RETRY`]; false when the thread is to end.
    fn pause(&self) -> bool {
        matches!(
            self.stopped.recv_timeout(RETRY),
            Err(RecvTimeoutError::Timeout)
        )
    }

    /// Reads inbox turn `turn` of `chat_id` to its end, sends its answer when it consumed its
    /// event and reports the delivery; a read that ends without `end` does neither. False when
    /// the thread is to end.
    fn take(&self, chat_id: i64, turn: &str) -> bool {
        let mut answer = None;
        let mut outcome = None;
        let read = read_turn(&self.client, &self.cancel, chat_id, turn, |event| {
            match event {
                Event::Final(text) => answer = Some(text.clone()),
                Event::Reaction { message_id, emoji } => {
                    self.outbox.react(chat_id, *message_id, emoji.as_str())?;
                }
                Event::End { outcome: ended, .. } => {
                    outcome = Some(*ended);
                    return Ok(true);
                }
                Event::Dropped => return Ok(true),
                Event::Start | Event::Interim(_) => {}
            }
            Ok(false)
        });
        match read {
            Read::Stopped => return false,
            Read::Lost => return true,
            Read::Done => {}
        }
        let Some(outcome) = outcome else {
            return true;
        };
        let delivery = match judge(outcome, answer.as_deref()) {
            Verdict::Report(delivery) => delivery,
            Verdict::Send(text) => match self.outbox.answer(chat_id, text) {
                Ok(true) => Delivery::Sent,
                Ok(false) => Delivery::SendFailed,
                Err(Closed) => return false,
            },
        };
        let report = DeliveryReport {
            session: session_key(chat_id),
            turn: turn.to_owned(),
            delivery,
        };
        match self.client.report_delivery(&report) {
            Ok(ack) if ack.ok => {}
            Ok(_) => eprintln!("ulei-tg: delivery of inbox turn {turn} not stored"),
            Err(e) => eprintln!("ulei-tg: reporting inbox turn {turn} failed: {e}"),
        }
        true
    }
}

/// The wait after `wait` between two rounds over refused owners.
fn next_wait(wait: Duration) -> Duration {
    wait.saturating_mul(2).min(REFUSED_MAX)
}

/// What to do with the answer of an ended inbox turn.
enum Verdict<'a> {
    /// Send nothing and report this.
    Report(Delivery),
    /// Send this answer once.
    Send(&'a str),
}

/// The ADR-0036 section 6 table over the `end` `outcome` and the `final` `answer`.
fn judge(outcome: TurnOutcome, answer: Option<&str>) -> Verdict<'_> {
    let Some(answer) = answer.filter(|answer| consumed(outcome, answer)) else {
        return Verdict::Report(Delivery::NoAnswer);
    };
    if strip(answer) == DONE_MARKER {
        Verdict::Report(Delivery::SilentDone)
    } else if is_intentional_silence(answer) {
        Verdict::Report(Delivery::Silent)
    } else {
        Verdict::Send(answer)
    }
}

/// Whether a turn that ended with `outcome` and answered `answer` consumed its event: it ran
/// to its answer and that answer is model text, not a stand-in for none.
fn consumed(outcome: TurnOutcome, answer: &str) -> bool {
    let text = strip(answer);
    matches!(outcome, TurnOutcome::Completed | TurnOutcome::Halted)
        && !text.is_empty()
        && text != EMPTY_SENTINEL
        && !NO_TEXT_PREFIXES
            .iter()
            .any(|prefix| text.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_over_refused_owners_doubles_up_to_five_minutes() {
        let mut wait = REFUSED_FIRST;
        let mut seen = vec![wait.as_secs()];
        for _ in 0..8 {
            wait = next_wait(wait);
            seen.push(wait.as_secs());
        }
        assert_eq!(seen, [5, 10, 20, 40, 80, 160, 300, 300, 300]);
    }
}
