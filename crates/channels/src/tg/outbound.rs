// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Delivery to the owner on a thread of its own (ADR-0032 section 2): texts through the send
//! plan and the shared [`Sender`], the typing indicator of running turns, and the index of
//! sent messages.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use super::sent::SentTexts;
use crate::bot_api::{self, Client, Config, ErrorKind, Sender, TextMessage};
use crate::render::{
    OutgoingText, SendOptions, SendPlan, TYPING_COOLDOWN, TYPING_SEND_TIMEOUT, TypingFailure,
    TypingRefresh, is_markup_rejection, plan_reply,
};

/// Head of the resend after a delivery failed for a reason other than the network.
const FORMATTING_FAILED: &str = "(Response formatting failed, plain text:)\n\n";
/// Characters of the reply the resend after a formatting failure carries.
const FORMATTING_FALLBACK_CHARS: usize = 3500;

/// The delivery thread is gone; nothing more is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the delivery thread ended abnormally")
    }
}

impl std::error::Error for Closed {}

enum Job {
    Deliver {
        chat_id: i64,
        reply_to: Option<i64>,
        text: String,
        notify: bool,
    },
    TurnStarted(i64),
    TurnEnded(i64),
}

/// A handle on the delivery thread; cheap to clone and usable from any thread. Calls only
/// queue work, which the thread runs in order.
#[derive(Clone)]
pub struct Outbox {
    jobs: mpsc::Sender<Job>,
}

impl Outbox {
    /// Delivers `text` (the agent's Markdown) to `chat_id`, the first message replying to
    /// `reply_to`. `notify` marks a turn's final answer: it rings and leaves the typing
    /// indicator alone; an interim text is silent and sends `typing` again. A blank text sends
    /// nothing.
    ///
    /// # Errors
    /// [`Closed`] when the delivery thread is gone.
    pub fn deliver(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        text: impl Into<String>,
        notify: bool,
    ) -> Result<(), Closed> {
        self.queue(Job::Deliver {
            chat_id,
            reply_to,
            text: text.into(),
            notify,
        })
    }

    /// A turn of `chat_id` started: `typing` goes out at once and every two seconds until
    /// [`Self::turn_ended`], unless the chat's cooldown from an earlier failure still runs.
    ///
    /// # Errors
    /// [`Closed`] when the delivery thread is gone.
    pub fn turn_started(&self, chat_id: i64) -> Result<(), Closed> {
        self.queue(Job::TurnStarted(chat_id))
    }

    /// The turn of `chat_id` ended: the typing refresh stops and the chat's cooldown is kept for
    /// its next turn. Call it before delivering the final answer.
    ///
    /// # Errors
    /// [`Closed`] when the delivery thread is gone.
    pub fn turn_ended(&self, chat_id: i64) -> Result<(), Closed> {
        self.queue(Job::TurnEnded(chat_id))
    }

    fn queue(&self, job: Job) -> Result<(), Closed> {
        self.jobs.send(job).map_err(|_| Closed)
    }
}

/// The delivery thread of `ulei-tg` and its first [`Outbox`].
pub struct Outbound {
    outbox: Outbox,
    worker: JoinHandle<()>,
}

impl Outbound {
    /// Starts the delivery thread over `sender`; every message it sends is recorded in `texts`.
    ///
    /// # Errors
    /// The thread could not be spawned.
    pub fn start(sender: Arc<Sender>, texts: SentTexts) -> io::Result<Self> {
        let config = sender.client().config();
        let options = SendOptions {
            disable_link_previews: config.disable_link_previews,
            ..SendOptions::default()
        };
        // A `typing` call is abandoned after the cadence's bound, as the reference cancels it.
        let typing = Client::new(Config {
            connect_timeout: config.connect_timeout.min(TYPING_SEND_TIMEOUT),
            read_timeout: TYPING_SEND_TIMEOUT,
            ..config.clone()
        });
        let (jobs, inbox) = mpsc::channel();
        let mut worker = Worker {
            sender,
            typing,
            options,
            texts,
            chats: HashMap::new(),
        };
        let worker = thread::Builder::new()
            .name("outbound".into())
            .spawn(move || worker.run(&inbox))?;
        Ok(Self {
            outbox: Outbox { jobs },
            worker,
        })
    }

    /// A handle for the callers that deliver turn replies and command replies.
    pub fn outbox(&self) -> Outbox {
        self.outbox.clone()
    }

    /// Waits until everything queued is sent; the thread ends once every [`Outbox`] is dropped.
    ///
    /// # Errors
    /// [`Closed`] when the thread ended abnormally.
    pub fn finish(self) -> Result<(), Closed> {
        drop(self.outbox);
        self.worker.join().map_err(|_| Closed)
    }
}

struct Worker {
    sender: Arc<Sender>,
    typing: Client,
    options: SendOptions,
    texts: SentTexts,
    /// Typing state per chat: running during a turn, kept stopped while its cooldown lasts.
    chats: HashMap<i64, TypingRefresh>,
}

impl Worker {
    /// Runs jobs and typing ticks until every [`Outbox`] is dropped; with no turn running it
    /// sleeps in `recv` without a deadline.
    fn run(&mut self, inbox: &Receiver<Job>) {
        loop {
            let next = self
                .chats
                .values()
                .filter_map(TypingRefresh::next_due)
                .min();
            let job = match next {
                Some(due) => inbox.recv_timeout(due.saturating_duration_since(Instant::now())),
                None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match job {
                Ok(Job::Deliver {
                    chat_id,
                    reply_to,
                    text,
                    notify,
                }) => self.deliver(chat_id, reply_to, &text, notify),
                Ok(Job::TurnStarted(chat_id)) => self.turn_started(chat_id),
                Ok(Job::TurnEnded(chat_id)) => self.turn_ended(chat_id),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.tick();
        }
    }

    /// The reference's `_send_with_retry` around `send`: transient failures are retried and
    /// end in the failure notice, any other failure but a response timeout resends the start
    /// of the reply once.
    fn deliver(&mut self, chat_id: i64, reply_to: Option<i64>, text: &str, notify: bool) {
        let plan = plan_reply(text, reply_to, notify, &self.options);
        if plan.messages.is_empty() {
            return;
        }
        let sender = Arc::clone(&self.sender);
        let mut calls = 0;
        let mut gave_up = None;
        let result = sender.send_with_retry(
            || {
                calls += 1;
                self.send(chat_id, &plan)
            },
            |notice| gave_up = Some(notice.to_owned()),
        );
        let Err(err) = result else { return };
        eprintln!("ulei-tg: delivery to chat {chat_id} failed: {err}");
        let resend = match gave_up {
            Some(notice) => notice,
            None if calls == 1 && err.description.to_lowercase().contains("timed out") => return,
            None => {
                let head: String = text.chars().take(FORMATTING_FALLBACK_CHARS).collect();
                format!("{FORMATTING_FAILED}{head}")
            }
        };
        let plan = plan_reply(&resend, reply_to, notify, &self.options);
        if let Err(err) = self.send(chat_id, &plan) {
            eprintln!("ulei-tg: delivery to chat {chat_id} failed: {err}");
        }
    }

    /// The reference's `send`: every chunk as `MarkdownV2`, as plain text when Telegram
    /// rejects the markup; `typing` again after an interim reply.
    fn send(&mut self, chat_id: i64, plan: &SendPlan) -> Result<(), bot_api::Error> {
        for message in &plan.messages {
            self.send_chunk(chat_id, message)?;
        }
        if plan.retrigger_typing {
            self.retrigger_typing(chat_id);
        }
        Ok(())
    }

    fn send_chunk(&self, chat_id: i64, message: &OutgoingText) -> Result<(), bot_api::Error> {
        let plain = message.plain_fallback();
        let sent = match self.sender.send_message(&text_message(chat_id, message)) {
            Ok(sent) => sent,
            Err(err)
                if message.parse_mode.is_some()
                    && is_markup_rejection(&err.description, &message.text) =>
            {
                self.sender.send_message(&text_message(chat_id, &plain))?
            }
            Err(err) => return Err(err),
        };
        self.texts.record(chat_id, sent.message_id, &plain.text);
        Ok(())
    }

    /// The reference's `send_typing` outside the cadence: skipped during the chat's cooldown,
    /// and its outcome updates that cooldown.
    fn retrigger_typing(&mut self, chat_id: i64) {
        let now = Instant::now();
        let typing = self.chats.entry(chat_id).or_insert_with(|| {
            let mut idle = TypingRefresh::start(now, TYPING_COOLDOWN);
            idle.stop();
            idle
        });
        if typing.in_cooldown(now) {
            return;
        }
        let failure = self
            .sender
            .client()
            .send_chat_action(chat_id, "typing")
            .err()
            .map(|err| typing_failure(&err));
        let done = Instant::now();
        typing.record(done, failure.as_ref().map_or(Ok(()), Err));
        if !typing.is_running() && !typing.in_cooldown(done) {
            self.chats.remove(&chat_id);
        }
    }

    fn turn_started(&mut self, chat_id: i64) {
        let now = Instant::now();
        let carried = match self.chats.get(&chat_id) {
            Some(typing) if typing.is_running() => return,
            Some(typing) => typing.cooldown_until(),
            None => None,
        };
        let typing = TypingRefresh::start(now, TYPING_COOLDOWN).with_cooldown_until(carried);
        self.chats.insert(chat_id, typing);
    }

    fn turn_ended(&mut self, chat_id: i64) {
        let Some(typing) = self.chats.get_mut(&chat_id) else {
            return;
        };
        typing.stop();
        if !typing.in_cooldown(Instant::now()) {
            self.chats.remove(&chat_id);
        }
    }

    /// Sends the `typing` actions that are due, each bounded by [`TYPING_SEND_TIMEOUT`].
    fn tick(&mut self) {
        for (&chat_id, typing) in &mut self.chats {
            if !typing.poll(Instant::now()) {
                continue;
            }
            let outcome = self.typing.send_chat_action(chat_id, "typing");
            let done = Instant::now();
            match outcome {
                Ok(()) => typing.sent(done, Ok(())),
                Err(err) if matches!(err.kind, ErrorKind::Timeout | ErrorKind::ConnectTimeout) => {
                    typing.abandoned(done);
                }
                Err(err) => typing.sent(done, Err(&typing_failure(&err))),
            }
        }
    }
}

fn text_message(chat_id: i64, message: &OutgoingText) -> TextMessage {
    TextMessage {
        chat_id,
        text: message.text.clone(),
        parse_mode: message.parse_mode.map(|mode| mode.as_str().to_owned()),
        reply_to: message.reply_to,
        disable_notification: message.disable_notification,
    }
}

fn typing_failure(err: &bot_api::Error) -> TypingFailure {
    TypingFailure {
        status: match err.kind {
            ErrorKind::RetryAfter(_) => Some(429),
            ErrorKind::Server(status) => Some(status),
            _ => None,
        },
        retry_after: err.retry_after(),
        description: err.description.clone(),
        io: matches!(err.kind, ErrorKind::Network | ErrorKind::Io),
    }
}
