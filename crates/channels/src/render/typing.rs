// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The `typing` indicator cadence while the agent works on a turn, as a schedule over an
//! injected clock.
//!
//! The reference runs this as an async loop per turn; here the channel's thread asks
//! [`TypingRefresh::next_due`] for its next deadline and sleeps until then or until other
//! I/O, so an idle channel never wakes up.

use std::time::{Duration, Instant};

/// Refresh interval: Telegram drops the indicator after about 5 s.
pub const TYPING_INTERVAL: Duration = Duration::from_secs(2);
/// Upper bound for one `sendChatAction` call so a slow call cannot stall the cadence.
pub const TYPING_SEND_TIMEOUT: Duration = Duration::from_millis(1500);
/// Default pause after a transient `sendChatAction` failure without `retry_after`.
pub const TYPING_COOLDOWN: Duration = Duration::from_secs(30);
const COOLDOWN_MIN: Duration = Duration::from_secs(1);
const COOLDOWN_MAX: Duration = Duration::from_secs(300);

/// How a `sendChatAction` call failed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TypingFailure {
    /// HTTP status (or Bot API `error_code`), when the call got a response.
    pub status: Option<u16>,
    /// `parameters.retry_after` of a flood-control answer.
    pub retry_after: Option<Duration>,
    /// Error description or I/O error text.
    pub description: String,
    /// The call failed below HTTP (connect, read, timeout).
    pub io: bool,
}

impl TypingFailure {
    /// Worth a cooldown: flood control, 429 or 5xx, rate-limit or timeout wording, or I/O.
    pub fn is_transient(&self) -> bool {
        if self.retry_after.is_some() || self.io {
            return true;
        }
        if self.status.is_some_and(|s| s == 429 || s >= 500) {
            return true;
        }
        let text = self.description.to_lowercase();
        [
            "too many requests",
            "rate limit",
            "timed out",
            "timeout",
            "temporar",
        ]
        .iter()
        .any(|m| text.contains(m))
    }
}

/// Typing refresh state of the owner chat for one turn.
#[derive(Debug, Clone)]
pub struct TypingRefresh {
    next: Option<Instant>,
    paused: bool,
    cooldown: Duration,
    cooldown_until: Option<Instant>,
}

impl TypingRefresh {
    /// Starts refreshing; the first chat action is due at `now`. `cooldown` is the pause after
    /// a transient failure without `retry_after` (default [`TYPING_COOLDOWN`]).
    pub fn start(now: Instant, cooldown: Duration) -> Self {
        Self {
            next: Some(now),
            paused: false,
            cooldown: cooldown.clamp(COOLDOWN_MIN, COOLDOWN_MAX),
            cooldown_until: None,
        }
    }

    /// When the caller must call [`Self::poll`] next; `None` once stopped.
    pub fn next_due(&self) -> Option<Instant> {
        self.next
    }

    /// At or after the deadline, returns whether to send a `typing` chat action now and
    /// schedules the next tick. Paused or cooling-down ticks send nothing but keep the cadence.
    pub fn poll(&mut self, now: Instant) -> bool {
        match self.next {
            Some(due) if now >= due => {}
            _ => return false,
        }
        self.next = Some(now + TYPING_INTERVAL);
        !self.paused && !self.in_cooldown(now)
    }

    /// True while chat actions are suppressed after a transient failure. An extra chat action
    /// outside the cadence (after an intermediate reply, [`super::SendPlan::retrigger_typing`])
    /// is skipped while this holds.
    pub fn in_cooldown(&self, now: Instant) -> bool {
        self.cooldown_until.is_some_and(|until| now < until)
    }

    /// Records the outcome of the chat action sent after [`Self::poll`] returned true; the
    /// next tick is due one interval after `now`, the time the call returned.
    pub fn sent(&mut self, now: Instant, outcome: Result<(), &TypingFailure>) {
        if self.next.is_none() {
            return;
        }
        self.next = Some(now + TYPING_INTERVAL);
        match outcome {
            Ok(()) => self.cooldown_until = None,
            Err(failure) if failure.is_transient() => {
                let delay = failure.retry_after.unwrap_or(self.cooldown);
                self.cooldown_until = Some(now + delay.clamp(COOLDOWN_MIN, COOLDOWN_MAX));
            }
            Err(_) => {}
        }
    }

    /// Suspends chat actions (for example while the final answer is being delivered).
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /// Resumes chat actions at the next tick.
    pub fn resume(&mut self) {
        self.paused = false;
    }

    /// Ends the refresh for this turn. Telegram has no call that clears the indicator; it
    /// lapses on its own or when a message arrives.
    pub fn stop(&mut self) {
        self.next = None;
        self.paused = false;
    }

    /// True until [`Self::stop`].
    pub fn is_running(&self) -> bool {
        self.next.is_some()
    }
}
