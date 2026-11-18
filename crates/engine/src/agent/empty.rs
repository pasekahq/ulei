// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Replies without visible text: the post-tool nudge, the thinking prefill, the retries with
//! their deterministic-empty cut-off, and the `(empty)` end of the turn.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::time::Duration;

use ulei_contracts::generated::c4g::{BlockType, Message, StopReason};

use super::{Step, think};
use crate::tools::py_strip;

/// The user message sent after an empty reply to tool results.
pub(crate) const EMPTY_TOOL_RESPONSE_NUDGE: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task.";
/// Text of the assistant message that stands for an empty reply.
pub(super) const EMPTY_REPLY: &str = "(empty)";
/// Most retries of one streak of empty replies.
const EMPTY_RETRY_BUDGET: u32 = 3;
/// Most thinking-only replies answered by sending the same messages again at once.
const MAX_PREFILLS: u32 = 2;
/// Messages from the end of the conversation in which a tool result makes a reply post-tool.
const RECENT_MESSAGES: usize = 5;
const BACKOFF_BASE_SECS: f64 = 5.0;
const BACKOFF_MAX_SECS: f64 = 60.0;
const JITTER_RATIO: f64 = 0.5;

/// What the loop does after a reply without visible text.
#[derive(Debug, Clone, Copy)]
pub(super) enum Recovery {
    /// Send `(empty)` for the reply and then the nudge, both kept out of the history.
    Nudge,
    /// Keep the reply as a prefill stub and send the same messages again at once.
    Prefill,
    /// Wait this long, then send the same messages again.
    Retry(Duration),
    /// End the turn with an `(empty)` answer.
    GiveUp,
}

/// One empty reply of the current streak.
struct Attempt {
    vendor: String,
    model: String,
    reason: StopReason,
    /// Usage came with input tokens and no output token.
    zero_output: bool,
}

/// The empty-reply state of one turn.
pub(super) struct Guard {
    /// Whether two zero-output replies of one route and stop reason end the retries.
    enabled: bool,
    retries: u32,
    prefills: u32,
    nudged: bool,
    attempts: Vec<Attempt>,
}

impl Guard {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            retries: 0,
            prefills: 0,
            nudged: false,
            attempts: Vec::new(),
        }
    }

    /// A reply with visible text: the next empty reply starts a new streak.
    pub(super) fn visible(&mut self) {
        self.retries = 0;
        self.prefills = 0;
    }

    /// Tool calls ran after `prefilled` scaffolding was dropped: the nudge may fire again.
    pub(super) fn tools_ran(&mut self, prefilled: bool) {
        if prefilled {
            self.visible();
        }
        self.nudged = false;
    }

    /// The recovery from `reply`, whose `text` has nothing visible, after `sent` in `step`.
    pub(super) fn recover(
        &mut self,
        reply: &Message,
        text: &str,
        sent: &[Message],
        step: &Step,
        reason: StopReason,
    ) -> Recovery {
        let inline = think::has_inline(text);
        let after_tools = sent
            .iter()
            .rev()
            .take(RECENT_MESSAGES)
            .any(|m| m.content.iter().any(|b| b.r#type == BlockType::ToolResult));
        if after_tools && !self.nudged && !inline {
            self.nudged = true;
            return Recovery::Nudge;
        }
        let thinking = inline
            || reply
                .content
                .iter()
                .any(|b| b.r#type == BlockType::Thinking);
        if thinking && self.prefills < MAX_PREFILLS {
            self.prefills += 1;
            return Recovery::Prefill;
        }
        if self.retries == 0 {
            self.attempts.clear();
        }
        self.attempts.push(Attempt::of(step, reason));
        if self.retries < EMPTY_RETRY_BUDGET && !self.deterministic() {
            self.retries += 1;
            return Recovery::Retry(backoff(self.retries));
        }
        Recovery::GiveUp
    }

    /// At least two attempts, all with zero output from one vendor, model and stop reason.
    fn deterministic(&self) -> bool {
        let Some(first) = self.attempts.first() else {
            return false;
        };
        self.enabled
            && self.attempts.len() >= 2
            && self.attempts.iter().all(|a| {
                a.zero_output
                    && (a.vendor.as_str(), a.model.as_str(), a.reason)
                        == (first.vendor.as_str(), first.model.as_str(), first.reason)
            })
    }
}

impl Attempt {
    fn of(step: &Step, reason: StopReason) -> Self {
        let (vendor, model) = step
            .route
            .as_ref()
            .map(|r| (r.vendor.clone(), r.model.clone()))
            .unwrap_or_default();
        let zero_output = step
            .usage
            .as_ref()
            .is_some_and(|u| u.input + u.cache_read + u.cache_write > 0 && u.output == 0);
        Self {
            vendor,
            model,
            reason,
            zero_output,
        }
    }
}

/// Whether `text` has anything but whitespace once reasoning and tool-call markup are removed.
pub(super) fn visible(text: &str) -> bool {
    !py_strip(&think::strip_blocks(text)).is_empty()
}

/// The wait before retry `attempt` (from 1): 5 s doubled per attempt up to 60 s, plus up to
/// half of that again at random.
fn backoff(attempt: u32) -> Duration {
    let delay = (BACKOFF_BASE_SECS * 2f64.powi(attempt.saturating_sub(1).min(62).cast_signed()))
        .min(BACKOFF_MAX_SECS);
    let bits = RandomState::new().hash_one(attempt) >> 11;
    #[allow(clippy::cast_precision_loss, reason = "53 bits fit the mantissa")]
    let unit = bits as f64 / (1u64 << 53) as f64;
    Duration::from_secs_f64(delay + unit * JITTER_RATIO * delay)
}
