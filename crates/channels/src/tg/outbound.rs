// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Delivery to the owner (ADR-0032 section 2, ADR-0034): texts through the send plan and the
//! shared [`Sender`] on the `outbound` thread, the typing indicator of running turns on the
//! `typing` thread, and the index of sent messages.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};

use super::sent::SentTexts;
use crate::bot_api::{self, Client, Clock, Config, ErrorKind, Sender, TextMessage};
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

struct Delivery {
    chat_id: i64,
    reply_to: Option<i64>,
    text: String,
    notify: bool,
    /// Told once a single send without the delivery retry went out or failed; `None` delivers
    /// with the retry and the failure notice.
    once: Option<mpsc::Sender<()>>,
}

enum Typing {
    Started(i64),
    /// The turn's end; `done` is told once no more `typing` of the turn can be sent.
    Ended(i64, mpsc::Sender<()>),
    /// The `typing` after an interim reply; `done` is told once it was sent or skipped.
    Retrigger(i64, mpsc::Sender<()>),
}

/// A handle on the delivery and typing threads; cheap to clone and usable from any thread.
/// Calls only queue work, which each thread runs in order.
#[derive(Clone)]
pub struct Outbox {
    deliveries: mpsc::Sender<Delivery>,
    typing: mpsc::Sender<Typing>,
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
        self.deliveries
            .send(Delivery {
                chat_id,
                reply_to,
                text: text.into(),
                notify,
                once: None,
            })
            .map_err(|_| Closed)
    }

    /// Sends `text` to `chat_id` as one silent message that replies to nothing, through the
    /// reference's plain `send` without the delivery retry or the failure notice. Returns once it
    /// went out or its failure was logged.
    ///
    /// # Errors
    /// [`Closed`] when the delivery thread is gone.
    pub fn echo(&self, chat_id: i64, text: impl Into<String>) -> Result<(), Closed> {
        let (done, sent) = mpsc::channel();
        self.deliveries
            .send(Delivery {
                chat_id,
                reply_to: None,
                text: text.into(),
                notify: false,
                once: Some(done),
            })
            .map_err(|_| Closed)?;
        sent.recv().map_err(|_| Closed)
    }

    /// A turn of `chat_id` started: `typing` goes out at once and every two seconds of the
    /// sender's clock until [`Self::turn_ended`], unless the chat's cooldown from an earlier
    /// failure still runs. A delivery waiting for its retry does not hold the cadence back.
    ///
    /// # Errors
    /// [`Closed`] when the typing thread is gone.
    pub fn turn_started(&self, chat_id: i64) -> Result<(), Closed> {
        self.typing
            .send(Typing::Started(chat_id))
            .map_err(|_| Closed)
    }

    /// The turn of `chat_id` ended: the typing refresh stops and the chat's cooldown is kept for
    /// its next turn. Returns once the typing thread has stopped it, so a final answer delivered
    /// after this call is never followed by a `typing` of the turn. With no turn of the chat
    /// running it changes nothing.
    ///
    /// # Errors
    /// [`Closed`] when the typing thread is gone.
    pub fn turn_ended(&self, chat_id: i64) -> Result<(), Closed> {
        let (done, stopped) = mpsc::channel();
        self.typing
            .send(Typing::Ended(chat_id, done))
            .map_err(|_| Closed)?;
        stopped.recv().map_err(|_| Closed)
    }
}

/// The delivery and typing threads of `ulei-tg` and their first [`Outbox`].
pub struct Outbound {
    outbox: Outbox,
    delivery: JoinHandle<()>,
    typing: JoinHandle<()>,
}

impl Outbound {
    /// Starts the delivery and typing threads over `sender`, whose clock times the typing
    /// cadence; every message they send is recorded in `texts`.
    ///
    /// # Errors
    /// A thread could not be spawned.
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
        let clock = Arc::clone(sender.clock());
        let shared = sender.client().clone();
        let (typing_jobs, typing_inbox) = mpsc::channel();
        let (deliveries, inbox) = mpsc::channel::<Delivery>();
        let worker = Worker {
            sender,
            options,
            texts,
            typing: typing_jobs.clone(),
        };
        let delivery = thread::Builder::new()
            .name("outbound".into())
            .spawn(move || {
                for job in inbox {
                    match job.once {
                        None => worker.deliver(job.chat_id, job.reply_to, &job.text, job.notify),
                        Some(done) => {
                            worker.send_once(job.chat_id, job.reply_to, &job.text, job.notify);
                            // The caller waits for it unless it is gone.
                            let _ = done.send(());
                        }
                    }
                }
            })?;
        let mut cadence = Cadence {
            client: typing,
            shared,
            clock,
            chats: HashMap::new(),
        };
        let typing = thread::Builder::new()
            .name("typing".into())
            .spawn(move || cadence.run(&typing_inbox))?;
        Ok(Self {
            outbox: Outbox {
                deliveries,
                typing: typing_jobs,
            },
            delivery,
            typing,
        })
    }

    /// A handle for the callers that deliver turn replies and command replies.
    pub fn outbox(&self) -> Outbox {
        self.outbox.clone()
    }

    /// Waits until everything queued is sent; the threads end once every [`Outbox`] is dropped.
    ///
    /// # Errors
    /// [`Closed`] when a thread ended abnormally.
    pub fn finish(self) -> Result<(), Closed> {
        drop(self.outbox);
        let delivered = self.delivery.join();
        let typed = self.typing.join();
        delivered.and(typed).map_err(|_| Closed)
    }
}

/// The `typing` thread: the cadence of every running turn and the `typing` after an interim
/// reply.
struct Cadence {
    /// Cadence calls, bounded by [`TYPING_SEND_TIMEOUT`].
    client: Client,
    /// The client of the deliveries, for the `typing` after an interim reply.
    shared: Client,
    clock: Arc<dyn Clock>,
    /// Typing state per chat: running during a turn, kept stopped while its cooldown lasts.
    chats: HashMap<i64, TypingRefresh>,
}

impl Cadence {
    /// Runs jobs and typing ticks until every [`Outbox`] is dropped; with no turn running it
    /// sleeps in `recv` without a deadline.
    fn run(&mut self, inbox: &Receiver<Typing>) {
        loop {
            let next = self
                .chats
                .values()
                .filter_map(TypingRefresh::next_due)
                .min();
            let job = match next {
                Some(due) => inbox.recv_timeout(due.saturating_duration_since(self.clock.now())),
                None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match job {
                Ok(Typing::Started(chat_id)) => self.turn_started(chat_id),
                Ok(Typing::Ended(chat_id, done)) => {
                    self.turn_ended(chat_id);
                    // The caller waits for it unless it is gone.
                    let _ = done.send(());
                }
                Ok(Typing::Retrigger(chat_id, done)) => {
                    self.retrigger(chat_id);
                    // The delivery thread waits for it unless it is gone.
                    let _ = done.send(());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.tick();
        }
    }

    fn turn_started(&mut self, chat_id: i64) {
        let now = self.clock.now();
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
        self.forget_idle(chat_id);
    }

    /// The reference's `send_typing` outside the cadence: skipped during the chat's cooldown,
    /// and its outcome updates that cooldown.
    fn retrigger(&mut self, chat_id: i64) {
        let now = self.clock.now();
        let typing = self.chats.entry(chat_id).or_insert_with(|| {
            let mut idle = TypingRefresh::start(now, TYPING_COOLDOWN);
            idle.stop();
            idle
        });
        if typing.in_cooldown(now) {
            return;
        }
        let failure = self
            .shared
            .send_chat_action(chat_id, "typing")
            .err()
            .map(|err| typing_failure(&err));
        typing.record(self.clock.now(), failure.as_ref().map_or(Ok(()), Err));
        self.forget_idle(chat_id);
    }

    /// Drops the entry of `chat_id` once it neither runs nor cools down.
    fn forget_idle(&mut self, chat_id: i64) {
        let now = self.clock.now();
        if self
            .chats
            .get(&chat_id)
            .is_some_and(|typing| !typing.is_running() && !typing.in_cooldown(now))
        {
            self.chats.remove(&chat_id);
        }
    }

    /// Sends the `typing` actions that are due, each bounded by [`TYPING_SEND_TIMEOUT`].
    fn tick(&mut self) {
        for (&chat_id, typing) in &mut self.chats {
            if !typing.poll(self.clock.now()) {
                continue;
            }
            let outcome = self.client.send_chat_action(chat_id, "typing");
            let done = self.clock.now();
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

/// The `outbound` thread: deliveries in order.
struct Worker {
    sender: Arc<Sender>,
    options: SendOptions,
    texts: SentTexts,
    typing: mpsc::Sender<Typing>,
}

impl Worker {
    /// The reference's `_send_with_retry` around `send`: transient failures are retried and
    /// end in the failure notice, any other failure but a response timeout resends the start
    /// of the reply once.
    fn deliver(&self, chat_id: i64, reply_to: Option<i64>, text: &str, notify: bool) {
        let plan = plan_reply(text, reply_to, notify, &self.options);
        if plan.messages.is_empty() {
            return;
        }
        let mut calls = 0;
        let mut gave_up = None;
        let result = self.sender.send_with_retry(
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

    /// One [`Self::send`] of the plan; a failure is only logged.
    fn send_once(&self, chat_id: i64, reply_to: Option<i64>, text: &str, notify: bool) {
        let plan = plan_reply(text, reply_to, notify, &self.options);
        if let Err(err) = self.send(chat_id, &plan) {
            eprintln!("ulei-tg: message to chat {chat_id} failed: {err}");
        }
    }

    /// The reference's `send`: every chunk as `MarkdownV2`, as plain text when Telegram
    /// rejects the markup; `typing` again after an interim reply.
    fn send(&self, chat_id: i64, plan: &SendPlan) -> Result<(), bot_api::Error> {
        for message in &plan.messages {
            self.send_chunk(chat_id, message)?;
        }
        if plan.retrigger_typing {
            // Waiting keeps it before the next message, which clears the indicator.
            let (done, sent) = mpsc::channel();
            if self.typing.send(Typing::Retrigger(chat_id, done)).is_ok() {
                let _ = sent.recv();
            }
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
