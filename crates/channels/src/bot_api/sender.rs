// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Outbound calls with the reference's retry ladders and a flood limiter.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::client::Client;
use super::clock::Clock;
use super::error::Error;
use super::types::{SentMessage, TextMessage, Upload};

/// Text sent to the chat when a reply could not be delivered after every retry.
pub const DELIVERY_FAILED_NOTICE: &str = "\u{26a0}\u{fe0f} Message delivery failed after multiple attempts. \
     Please try again \u{2014} your request was processed but the response could not be sent.";

/// Attempts of one `sendMessage` (the first one included).
const SEND_ATTEMPTS: u32 = 3;
/// Retries of a whole delivery after the first failed one.
const DELIVERY_RETRIES: u32 = 2;
/// First back-off of a delivery retry; doubles each time.
const DELIVERY_BASE_DELAY: Duration = Duration::from_secs(2);
const REPLY_TARGET_GONE: &str = "message to be replied not found";

/// Sends through a shared [`Client`], pacing and retrying like the reference.
pub struct Sender {
    client: Client,
    clock: Arc<dyn Clock>,
    limiter: Mutex<FloodLimiter>,
}

impl Sender {
    /// A sender with Telegram's flood limits (1 message/s per chat, 30/s in total).
    pub fn new(client: Client, clock: Arc<dyn Clock>) -> Self {
        Self {
            client,
            clock,
            limiter: Mutex::new(FloodLimiter::telegram()),
        }
    }

    /// The client underneath, for calls that need no pacing.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// One `sendMessage` with up to 3 attempts: transport errors back off 1 s then 2 s,
    /// flood control waits `retry_after`, a deleted reply target is dropped, and a
    /// response timeout or any other refusal is returned at once (it may have arrived).
    ///
    /// # Errors
    ///
    /// The error that ended the attempts.
    pub fn send_message(&self, message: &TextMessage) -> Result<SentMessage, Error> {
        let mut message = message.clone();
        let mut attempt = 0;
        loop {
            self.pace(message.chat_id);
            let err = match self.client.send_message(&message) {
                Ok(sent) => return Ok(sent),
                Err(err) => err,
            };
            attempt += 1;
            let more = attempt < SEND_ATTEMPTS;
            if err.is_bad_request() {
                let gone = err.description.to_lowercase().contains(REPLY_TARGET_GONE);
                if !(gone && message.reply_to.is_some() && more) {
                    return Err(err);
                }
                message.reply_to = None;
            } else if err.is_network_error() {
                if err.is_timeout() || !more {
                    return Err(err);
                }
                self.clock.sleep(Duration::from_secs(1 << (attempt - 1)));
            } else {
                let wait = err.retry_after().or_else(|| {
                    let text = err.description.to_lowercase();
                    text.contains("retry after")
                        .then_some(Duration::from_secs(1))
                });
                match wait {
                    Some(wait) if more => self.clock.sleep(wait),
                    _ => return Err(err),
                }
            }
        }
    }

    /// One upload, paced like a message; failures are returned without a retry.
    ///
    /// # Errors
    ///
    /// The classified [`Error`] of the upload.
    pub fn send_media(&self, upload: &Upload) -> Result<SentMessage, Error> {
        self.pace(upload.chat_id);
        self.client.send_media(upload)
    }

    /// Delivers through `send` with up to 2 more tries 2 s and 4 s apart (plus up to 1 s
    /// jitter, or the server's `retry_after` once) unless the failure is a response timeout
    /// or a too-long message. After the last failed retry `notify` receives
    /// [`DELIVERY_FAILED_NOTICE`]. The error that ends delivery is returned either way.
    ///
    /// # Errors
    ///
    /// The error of the last call of `send`.
    pub fn send_with_retry<T>(
        &self,
        mut send: impl FnMut() -> Result<T, Error>,
        notify: impl FnOnce(&str),
    ) -> Result<T, Error> {
        let mut err = match send() {
            Ok(done) => return Ok(done),
            Err(err) => err,
        };
        if !delivery_retryable(&err) {
            return Err(err);
        }
        let mut server_wait = err.retry_after();
        for attempt in 0..DELIVERY_RETRIES {
            let delay = server_wait
                .take()
                .unwrap_or(DELIVERY_BASE_DELAY * (1 << attempt));
            self.clock.sleep(delay + self.clock.jitter());
            err = match send() {
                Ok(done) => return Ok(done),
                Err(err) => err,
            };
            server_wait = err.retry_after();
            if !delivery_retryable(&err) {
                return Err(err);
            }
        }
        notify(DELIVERY_FAILED_NOTICE);
        Err(err)
    }

    /// Blocks until the flood limits let one more message to `chat_id` go out.
    fn pace(&self, chat_id: i64) {
        loop {
            let wait = {
                let mut limiter = self.limiter.lock().unwrap_or_else(PoisonError::into_inner);
                let now = self.clock.now();
                let wait = limiter.delay(chat_id, now);
                if wait.is_zero() {
                    limiter.record(chat_id, now);
                }
                wait
            };
            if wait.is_zero() {
                return;
            }
            self.clock.sleep(wait);
        }
    }
}

/// The reference marks every Telegram send failure retryable except a response timeout
/// (it may have been delivered) and a too-long message.
fn delivery_retryable(err: &Error) -> bool {
    let timed_out = err.is_timeout() || err.description.to_lowercase().contains("timed out");
    !err.is_message_too_long() && (err.is_connect_timeout() || !timed_out)
}

/// Telegram's broadcast limits as a deterministic sliding window over injected time.
#[derive(Debug)]
pub struct FloodLimiter {
    per_chat: Duration,
    global_count: usize,
    window: Duration,
    recent: VecDeque<Instant>,
    last_by_chat: HashMap<i64, Instant>,
}

impl FloodLimiter {
    /// At most one message per chat per `per_chat`, and `global_count` per `window`.
    pub fn new(per_chat: Duration, global_count: usize, window: Duration) -> Self {
        Self {
            per_chat,
            global_count: global_count.max(1),
            window,
            recent: VecDeque::new(),
            last_by_chat: HashMap::new(),
        }
    }

    /// Telegram's documented limits: 1 message/s per chat, 30 messages/s in total.
    pub fn telegram() -> Self {
        Self::new(Duration::from_secs(1), 30, Duration::from_secs(1))
    }

    /// How long a message to `chat_id` must wait at `now`; zero when it may go.
    pub fn delay(&self, chat_id: i64, now: Instant) -> Duration {
        let chat_wait = self
            .last_by_chat
            .get(&chat_id)
            .map_or(Duration::ZERO, |last| {
                (*last + self.per_chat).saturating_duration_since(now)
            });
        let in_window = self
            .recent
            .iter()
            .filter(|at| now.saturating_duration_since(**at) < self.window)
            .count();
        let global_wait = if in_window >= self.global_count {
            self.recent
                .iter()
                .rev()
                .nth(self.global_count - 1)
                .map_or(Duration::ZERO, |at| {
                    (*at + self.window).saturating_duration_since(now)
                })
        } else {
            Duration::ZERO
        };
        chat_wait.max(global_wait)
    }

    /// Records a message to `chat_id` sent at `now`, forgetting what left every window.
    pub fn record(&mut self, chat_id: i64, now: Instant) {
        let horizon = self.window.max(self.per_chat);
        while self
            .recent
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= horizon)
        {
            self.recent.pop_front();
        }
        self.last_by_chat
            .retain(|_, at| now.saturating_duration_since(*at) < horizon);
        self.recent.push_back(now);
        self.last_by_chat.insert(chat_id, now);
    }
}
