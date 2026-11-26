// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The tool loop: one model turn on the qb gateway, the tool calls it makes run through the
//! policy chain, their results sent back, until the model stops, a policy halts the turn after
//! a tool batch or the step limit asks for a summary. Every request is built only from the
//! loop's inputs, so a replayed conversation sends the same bytes.

mod dispatch;
pub(crate) mod empty;
pub(crate) mod summary;
pub(crate) mod think;

use std::borrow::Cow;
use std::cell::Cell;
use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use ulei_contracts::generated::c4g::{
    Block, BlockType, CacheMode, Effort, Error as GatewayError, ErrorClass, Message, Role,
    RouteInfo, StopReason, StreamEvent, Tool, ToolCall, TurnRequest, Usage,
};

use crate::compress::{self, Compaction, Compression, MAX_COMPRESSION_ATTEMPTS};
use crate::gateway::{Client, Error, Turn};
use crate::policy::{Chain, Origin, StopDecision};
use crate::tools::{Context, Interrupt, Registry, py_strip, squeeze, thousands};
use empty::{EMPTY_REPLY, EMPTY_TOOL_RESPONSE_NUDGE, Guard, Recovery};

pub use dispatch::DEFAULT_CONCURRENT_TOOL_TIMEOUT;

/// Most retries of one step that the gateway answered with `overloaded`.
pub const MAX_OVERLOADED_RETRIES: u32 = 3;
/// Most nudges in a row after replies that stopped for tool use without a call.
const MAX_DROPPED_TOOL_CALL_RETRIES: u32 = 3;
/// The user message sent after a reply that stopped for tool use without a call.
pub(crate) const DROPPED_TOOL_CALL_NUDGE: &str = "Your previous turn indicated a tool call but none was included. Do not narrate a plan or restate intent — issue the actual tool call now to continue the task.";
/// Default step limit of one turn.
pub const DEFAULT_MAX_STEPS: usize = 200;
/// Wait before the step a compaction after `context_too_long` retries.
pub const COMPRESSION_RETRY_WAIT: Duration = Duration::from_secs(2);

/// The fields every turn request of a loop shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// Route alias configured in qb.
    pub route: String,
    /// System prompt.
    pub system: Option<String>,
    /// Upper bound of output tokens per model turn.
    pub max_tokens: u64,
    /// Reasoning effort.
    pub effort: Option<Effort>,
    /// Opaque session id passed to qb.
    pub session_key: Option<String>,
    /// Prompt cache mode of every request of the turn; `None` sends none.
    pub cache: Option<CacheMode>,
}

/// Waits between retries; tests inject one that records instead of sleeping.
pub trait Sleeper {
    /// Blocks the loop for `duration`, or until `interrupt` fires.
    fn sleep(&mut self, duration: Duration, interrupt: &Interrupt);
}

/// A [`Sleeper`] that blocks the calling thread until the wait ends or the interrupt fires.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&mut self, duration: Duration, interrupt: &Interrupt) {
        interrupt.sleep(duration);
    }
}

/// What the gateway reported for one step, from its last attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// `request_id` of the step's request.
    pub request_id: String,
    /// The last `route` event.
    pub route: Option<RouteInfo>,
    /// The last `usage` event.
    pub usage: Option<Usage>,
}

impl Step {
    pub(crate) fn new(request_id: String) -> Self {
        Self {
            request_id,
            route: None,
            usage: None,
        }
    }
}

/// How a loop ended.
#[derive(Debug)]
pub enum Ended {
    /// The model stopped for this reason without tool calls and no policy continued; an
    /// empty reply the retries did not mend leaves an `(empty)` answer.
    Stopped(StopReason),
    /// The step limit was reached; the last message is the summary or the text standing for a
    /// summary that failed.
    MaxSteps,
    /// A policy ended the turn after a tool batch; the last message is its answer.
    Halted,
    /// The interrupt handle fired.
    Interrupted,
    /// The gateway answered `context_too_long` and compaction could not make the step fit.
    ContextExhausted {
        /// One line saying why.
        line: String,
        /// The gateway's error.
        error: GatewayError,
    },
    /// Every login of the route is out of quota.
    QuotaExhausted {
        /// One line naming the route and the nearest reset in UTC.
        line: String,
        /// The gateway's error.
        error: GatewayError,
    },
    /// Another gateway error class, or `overloaded` past [`MAX_OVERLOADED_RETRIES`].
    Gateway(GatewayError),
    /// The connection failed, timed out, broke the wire format or was cut.
    Transport(Error),
}

impl fmt::Display for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopped(reason) => write!(f, "the model stopped: {}", reason.as_str()),
            Self::MaxSteps => write!(f, "the turn reached its step limit"),
            Self::Halted => write!(f, "a policy halted the turn"),
            Self::Interrupted => write!(f, "the turn was interrupted"),
            Self::ContextExhausted { line, .. } | Self::QuotaExhausted { line, .. } => {
                write!(f, "{line}")
            }
            Self::Gateway(error) => write!(
                f,
                "gateway error {}: {}",
                error.class.as_str(),
                error.message.as_deref().unwrap_or("no message")
            ),
            Self::Transport(error) => write!(f, "{error}"),
        }
    }
}

/// The result of [`Agent::run`].
#[derive(Debug)]
pub struct Outcome {
    /// Messages appended to the conversation, in order.
    pub messages: Vec<Message>,
    /// One entry per step that sent a request.
    pub steps: Vec<Step>,
    /// How the loop ended.
    pub ended: Ended,
    /// The live set of the turn's last compaction, if one ran.
    pub compacted: Option<Compacted>,
}

/// The live set a compaction inside a turn left.
#[derive(Debug, Clone, PartialEq)]
pub struct Compacted {
    /// The messages that replaced the history, the input and the turn's first
    /// [`Compacted::after`] messages.
    pub live: Vec<Message>,
    /// How many of [`Outcome::messages`] came before the compaction; the rest follow
    /// [`Compacted::live`].
    pub after: usize,
}

/// The pieces a loop runs with.
pub struct Agent<'a> {
    /// The qb gateway.
    pub client: &'a Client,
    /// Tools the model may call.
    pub tools: &'a Registry,
    /// Policies around tool calls and turn ends.
    pub chain: &'a mut Chain,
    /// Working directory of the tool calls.
    pub cwd: &'a Path,
    /// Fields every request shares.
    pub template: &'a Template,
    /// Deadline of one parallel segment of tool calls; `None` waits for every call.
    pub batch_timeout: Option<Duration>,
    /// Whether two empty replies with zero output tokens from one route end the retries early.
    pub empty_guard: bool,
    /// Who sent the turn's input and where, as the policies scope on it.
    pub origin: Origin,
    /// Compaction before a step and after `context_too_long`; `None` never compacts.
    pub compaction: Option<Compaction<'a>>,
    /// The context the caller sent with the turn's input, before the chain's turn context.
    pub context: Option<&'a str>,
    /// Blocks attached after the turn's input in every request and never kept in the outcome:
    /// the owner's images ([`crate::images::blocks`]).
    pub images: &'a [Block],
    /// Takes the interim text of each assistant message with tool calls before its calls run.
    pub interim: Option<&'a mut (dyn FnMut(&str) + 'a)>,
}

/// The assistant message of one step, its tool calls and its stop reason.
pub(crate) struct Reply {
    pub(crate) message: Message,
    calls: Vec<ToolCall>,
    reason: StopReason,
}

/// Where a message of the turn stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// It stays in the conversation.
    Kept,
    /// A thinking-only reply that counts toward the prefill limit; no request sends it.
    Prefill,
    /// The `(empty)` stand-in of a reply to tool results, a reply that stopped for tool use
    /// without a call, or the nudge after either.
    Nudge,
}

/// The messages of the turn so far; of those after the history, the turn keeps the
/// [`Kind::Kept`] ones and the requests send all but the [`Kind::Prefill`] ones. A compaction
/// makes its live set the history.
struct Live {
    request: TurnRequest,
    history: usize,
    kinds: Vec<Kind>,
    input: Option<Input>,
    /// The kept messages of the turn before its last compaction.
    earlier: Vec<Message>,
    compacted: bool,
    /// Set once the vendor refused a thinking block's signature: the requests of the rest of
    /// the turn leave every thinking block out.
    unsigned: Cell<bool>,
}

/// The turn's input where the request carries it with the turn context.
struct Input {
    at: usize,
    stored: Message,
    sent: Message,
}

impl Live {
    /// The turn before its first step: `messages` are the history, `input` the turn's.
    fn new(
        template: &Template,
        tools: Vec<Tool>,
        messages: Vec<Message>,
        input: Option<Input>,
    ) -> Self {
        Self {
            history: messages.len(),
            request: TurnRequest {
                route: template.route.clone(),
                system: template.system.clone(),
                messages,
                tools: (!tools.is_empty()).then_some(tools),
                max_tokens: template.max_tokens,
                effort: template.effort,
                session_key: template.session_key.clone(),
                cache: template.cache,
                request_id: String::new(),
            },
            kinds: Vec::new(),
            input,
            earlier: Vec::new(),
            compacted: false,
            unsigned: Cell::new(false),
        }
    }

    fn push(&mut self, message: Message, kind: Kind) {
        self.request.messages.push(message);
        self.kinds.push(kind);
    }

    /// Drops the trailing messages of a kind in `kinds`; whether one was dropped.
    fn drop_trailing(&mut self, kinds: &[Kind]) -> bool {
        let mut dropped = false;
        while self.kinds.last().is_some_and(|k| kinds.contains(k)) {
            self.kinds.pop();
            self.request.messages.pop();
            dropped = true;
        }
        dropped
    }

    /// The request to send: without prefills, and once one is left out, with every run of
    /// adjacent user messages without tool results merged into one; without thinking blocks
    /// once [`Live::unsigned`] is set.
    fn sent(&self) -> Cow<'_, TurnRequest> {
        let sent = self.without_prefills();
        if self.unsigned.get() {
            return Cow::Owned(without_thinking(&sent));
        }
        sent
    }

    fn without_prefills(&self) -> Cow<'_, TurnRequest> {
        if !self.kinds.contains(&Kind::Prefill) {
            return Cow::Borrowed(&self.request);
        }
        let kinds = std::iter::repeat_n(Kind::Kept, self.history).chain(self.kinds.iter().copied());
        let mut messages: Vec<Message> = Vec::with_capacity(self.request.messages.len());
        for (message, kind) in self.request.messages.iter().zip(kinds) {
            if kind == Kind::Prefill {
                continue;
            }
            match messages.last_mut() {
                Some(last) if plain_user(last) && plain_user(message) => {
                    last.content.extend(message.content.iter().cloned());
                }
                _ => messages.push(message.clone()),
            }
        }
        let request = &self.request;
        Cow::Owned(TurnRequest {
            route: request.route.clone(),
            system: request.system.clone(),
            messages,
            tools: request.tools.clone(),
            max_tokens: request.max_tokens,
            effort: request.effort,
            session_key: request.session_key.clone(),
            cache: request.cache,
            request_id: request.request_id.clone(),
        })
    }

    /// The kept messages after the history.
    fn turn(&self) -> impl Iterator<Item = &Message> {
        self.request.messages[self.history..]
            .iter()
            .zip(&self.kinds)
            .filter_map(|(message, kind)| (*kind == Kind::Kept).then_some(message))
    }

    /// The conversation as stored: the history with the input as the turn got it, and the
    /// kept messages after it.
    fn conversation(&self) -> Vec<Message> {
        let mut messages = self.request.messages[..self.history].to_vec();
        if let Some(input) = &self.input {
            messages[input.at] = input.stored.clone();
        }
        messages.extend(self.turn().cloned());
        messages
    }

    /// Makes `compacted` the history; the input takes back its turn context where the
    /// compacted set keeps it as it was.
    fn replace(&mut self, compacted: Vec<Message>) {
        let turn: Vec<Message> = self.turn().cloned().collect();
        self.earlier.extend(turn);
        self.history = compacted.len();
        self.request.messages = compacted;
        self.kinds.clear();
        self.compacted = true;
        if let Some(input) = &mut self.input {
            match self
                .request
                .messages
                .iter()
                .rposition(|m| *m == input.stored)
            {
                Some(at) => {
                    input.at = at;
                    self.request.messages[at] = input.sent.clone();
                }
                None => self.input = None,
            }
        }
    }

    fn outcome(mut self, steps: Vec<Step>, ended: Ended) -> Outcome {
        if let Some(input) = self.input {
            self.request.messages[input.at] = input.stored;
        }
        let turn = self.request.messages.split_off(self.history);
        let mut messages = self.earlier;
        let after = messages.len();
        messages.extend(
            turn.into_iter()
                .zip(self.kinds)
                .filter_map(|(message, kind)| (kind == Kind::Kept).then_some(message)),
        );
        let live = self.request.messages;
        Outcome {
            messages,
            steps,
            ended,
            compacted: self.compacted.then_some(Compacted { live, after }),
        }
    }
}

impl Agent<'_> {
    /// Runs the loop after `history`, whose last message is the turn's input; the chain sees
    /// that input once at the start, and every request sends it with the caller's and the
    /// chain's turn context appended after a blank line when it is one text block, then the
    /// turn's images, and the outcome never carries them. Step `n` sends `request_id(n)`. A step with tool calls hands
    /// its interim text to the listener, then runs them as
    /// [`crate::tools::plan`] splits them and keeps their results in call order; a policy that
    /// halts then ends the turn with its answer, else the loop continues; a reply to `end` or
    /// `tool_use` without visible text takes the text of the last step whose calls were all
    /// housekeeping as its reply, which the stop policies then see, else is nudged, kept as a
    /// prefill or retried; a `tool_use` reply without a call is nudged up to 3 times in a row;
    /// any other step ends the loop with its stop reason unless `end` or `tool_use` and a policy
    /// continues. After `max_steps` steps one more request without tools asks for a summary.
    /// With a compaction, a step whose prompt reached the threshold is preceded by one, and a
    /// step answered `context_too_long` is compacted and retried; [`MAX_COMPRESSION_ATTEMPTS`]
    /// bounds both until a step's usage proves a compaction brought the prompt under the
    /// threshold.
    pub fn run(
        &mut self,
        history: &[Message],
        request_id: &dyn Fn(usize) -> String,
        interrupt: &Interrupt,
        max_steps: usize,
        sleeper: &mut dyn Sleeper,
    ) -> Outcome {
        let (messages, input) = self.open(history);
        let mut live = Live::new(self.template, self.tools.definitions(), messages, input);
        let mut guard = Guard::new(self.empty_guard);
        let mut dropped_calls = 0;
        let mut compactions = 0;
        let mut delivered = Vec::new();
        let mut steps: Vec<Step> = Vec::new();
        let ended = loop {
            if steps.len() == max_steps {
                break self.summarize(
                    &mut live, &mut steps, request_id, interrupt, sleeper, max_steps,
                );
            }
            match self.before_step(&mut live, &mut compactions, interrupt) {
                Ok(true) => (guard, dropped_calls) = (Guard::new(self.empty_guard), 0),
                Ok(false) => {}
                Err(ended) => break ended,
            }
            live.request.request_id = request_id(steps.len());
            let mut step = Step::new(live.request.request_id.clone());
            let reply = self.answer(&live.sent(), &live.unsigned, interrupt, sleeper, &mut step);
            let Reply {
                mut message,
                calls,
                reason,
            } = match reply {
                Ok(reply) => reply,
                Err(ended) => {
                    steps.push(step);
                    match self.retry(ended, &mut live, &mut compactions, interrupt, sleeper) {
                        Ok(()) => (guard, dropped_calls) = (Guard::new(self.empty_guard), 0),
                        Err(ended) => break ended,
                    }
                    continue;
                }
            };
            if let Some(usage) = &step.usage {
                self.observe(usage, &mut compactions);
            }
            if !calls.is_empty() {
                steps.push(step);
                let prefilled = live.drop_trailing(&[Kind::Prefill]);
                let message = empty::without_marker(message);
                self.interim(&message, &mut delivered);
                guard.tools_ran(prefilled, &message, &calls);
                live.push(message, Kind::Kept);
                live.push(self.call_tools(&calls, interrupt), Kind::Kept);
                dropped_calls = 0;
                if let Some(answer) = self.chain.halt() {
                    live.push(text_message(Role::Assistant, &answer), Kind::Kept);
                    break Ended::Halted;
                }
                if interrupt.is_interrupted() {
                    break Ended::Interrupted;
                }
                continue;
            }
            let final_reply = matches!(reason, StopReason::End | StopReason::ToolUse);
            let mut dropped_call =
                reason == StopReason::ToolUse && dropped_calls < MAX_DROPPED_TOOL_CALL_RETRIES;
            if final_reply {
                let content = text(&message);
                if empty::visible(&content) {
                    guard.visible();
                } else if let Some(answer) = guard.housekeeping_answer() {
                    message = text_message(Role::Assistant, &answer);
                    dropped_call = false;
                } else {
                    let recovery =
                        guard.recover(&message, &content, &live.request.messages, &step, reason);
                    steps.push(step);
                    if recover(&mut live, recovery, message, (sleeper, interrupt)) {
                        continue;
                    }
                    break Ended::Stopped(reason);
                }
            }
            steps.push(step);
            if dropped_call {
                dropped_calls += 1;
                nudge_dropped_call(&mut live, message);
                continue;
            }
            dropped_calls = 0;
            live.drop_trailing(&[Kind::Prefill, Kind::Nudge]);
            let decision = if final_reply {
                self.chain.on_stop(&message, interrupt)
            } else {
                StopDecision::Stop
            };
            if !message.content.is_empty() {
                live.push(message, Kind::Kept);
            }
            if let StopDecision::Continue(text) = decision {
                live.push(text_message(Role::User, &text), Kind::Kept);
                continue;
            }
            break Ended::Stopped(reason);
        };
        live.outcome(steps, ended)
    }

    /// `history` after the chain saw its last message as the turn's input, with the chain's
    /// turn context appended to that message when it is a user message of one text block and
    /// the turn's images after it when it is a user message; the input when either changed it.
    fn open(&mut self, history: &[Message]) -> (Vec<Message>, Option<Input>) {
        let mut messages = history.to_vec();
        let Some(at) = messages.len().checked_sub(1) else {
            return (messages, None);
        };
        let stored = messages[at].clone();
        self.chain.on_turn_start(self.origin, &stored);
        if let Some(input) = user_text(&mut messages[at]) {
            self.chain.turn_context(self.origin, self.context, input);
        }
        if messages[at].role == Role::User {
            messages[at].content.extend_from_slice(self.images);
        }
        let input = (messages[at] != stored).then(|| Input {
            at,
            stored,
            sent: messages[at].clone(),
        });
        (messages, input)
    }

    /// Hands the listener the visible text of `message`: its text without reasoning and
    /// tool-call markup, stripped. Not when it is empty, `(empty)` or, with every whitespace run
    /// made one space, in `delivered`, which keeps the texts handed over.
    fn interim(&mut self, message: &Message, delivered: &mut Vec<String>) {
        let Some(listener) = self.interim.as_deref_mut() else {
            return;
        };
        let content = think::strip_blocks(&text(message));
        let visible = py_strip(&content);
        let key = squeeze(visible);
        if key.is_empty() || visible == EMPTY_REPLY || delivered.contains(&key) {
            return;
        }
        listener(visible);
        delivered.push(key);
    }

    /// Takes a step's usage; the first prompt size after a compaction that is under the
    /// threshold of the window in use proves the compaction worked and gives the turn its
    /// compactions back. A rough estimate never does.
    fn observe(&mut self, usage: &Usage, compactions: &mut usize) {
        let max_tokens = self.template.max_tokens;
        let Some(compaction) = &mut self.compaction else {
            return;
        };
        let threshold = compaction.overflow_window(max_tokens).threshold_tokens;
        let after = compaction.compressor.observe(usage, compaction.window);
        if after.is_some_and(|tokens| tokens > 0 && tokens < threshold) {
            *compactions = 0;
        }
    }

    /// Compacts the turn when the next step's prompt reached the threshold of a known window
    /// and the turn has compactions left; whether it did, or how the turn ends.
    fn before_step(
        &mut self,
        live: &mut Live,
        compactions: &mut usize,
        interrupt: &Interrupt,
    ) -> Result<bool, Ended> {
        let max_tokens = self.template.max_tokens;
        let Some(compaction) = &mut self.compaction else {
            return Ok(false);
        };
        let Some(window) = compaction.window else {
            return Ok(false);
        };
        let now = (compaction.clock)();
        if *compactions >= MAX_COMPRESSION_ATTEMPTS
            || !compaction
                .compressor
                .should_compress(&live.sent(), window, now)
        {
            return Ok(false);
        }
        *compactions += 1;
        compact(compaction, max_tokens, live, interrupt).ok_or(Ended::Interrupted)?;
        Ok(true)
    }

    /// Readies a step that `ended` to be sent again: after `context_too_long` the turn is
    /// compacted and the retry waits [`COMPRESSION_RETRY_WAIT`]. Otherwise the turn ends: with
    /// `ended` without a compaction or for another end, past [`MAX_COMPRESSION_ATTEMPTS`] or
    /// when the compaction made no progress with one line.
    fn retry(
        &mut self,
        ended: Ended,
        live: &mut Live,
        compactions: &mut usize,
        interrupt: &Interrupt,
        sleeper: &mut dyn Sleeper,
    ) -> Result<(), Ended> {
        let max_tokens = self.template.max_tokens;
        let (error, compaction) = match (ended, &mut self.compaction) {
            (Ended::Gateway(error), Some(compaction))
                if error.class == ErrorClass::ContextTooLong =>
            {
                (error, compaction)
            }
            (ended, _) => return Err(ended),
        };
        *compactions += 1;
        if *compactions > MAX_COMPRESSION_ATTEMPTS {
            return Err(Ended::ContextExhausted {
                line: format!(
                    "Context length exceeded: max compression attempts ({MAX_COMPRESSION_ATTEMPTS}) reached."
                ),
                error,
            });
        }
        let before = live.conversation();
        let compression =
            compact(compaction, max_tokens, live, interrupt).ok_or(Ended::Interrupted)?;
        if let Some(tokens) = no_progress(&before, &compression) {
            return Err(Ended::ContextExhausted {
                line: format!(
                    "Context length exceeded ({} tokens). Cannot compress further.",
                    thousands(tokens)
                ),
                error,
            });
        }
        sleeper.sleep(COMPRESSION_RETRY_WAIT, interrupt);
        Ok(())
    }

    /// Runs `calls` and answers them with one message of their results in call order.
    fn call_tools(&mut self, calls: &[ToolCall], interrupt: &Interrupt) -> Message {
        let ctx = Context {
            cwd: self.cwd,
            interrupt,
        };
        let outputs = dispatch::run_calls(self.chain, self.tools, calls, ctx, self.batch_timeout);
        let results = calls
            .iter()
            .zip(outputs)
            .map(|(call, output)| Block {
                id: Some(call.id.clone()),
                content: Some(output.content),
                is_error: Some(output.is_error),
                ..block(BlockType::ToolResult)
            })
            .collect();
        Message {
            role: Role::User,
            content: results,
        }
    }

    /// One step: the turn, retried while the gateway is overloaded, and sent once more at once
    /// without its thinking blocks when the vendor refused one of their signatures; `unsigned`
    /// then stays set so the turn's later requests leave them out as well.
    fn answer(
        &self,
        request: &TurnRequest,
        unsigned: &Cell<bool>,
        interrupt: &Interrupt,
        sleeper: &mut dyn Sleeper,
        step: &mut Step,
    ) -> Result<Reply, Ended> {
        let mut request = Cow::Borrowed(request);
        let mut retries = 0;
        loop {
            if interrupt.is_interrupted() {
                return Err(Ended::Interrupted);
            }
            let error = match self.attempt(&request, interrupt, step) {
                Err(Ended::Gateway(error)) => error,
                other => return other,
            };
            if !unsigned.get() && thinking_signature(&error) && carries_thinking(&request) {
                unsigned.set(true);
                request = Cow::Owned(without_thinking(&request));
                continue;
            }
            match (error.class, error.retry_after) {
                (ErrorClass::QuotaExhausted, _) => {
                    return Err(Ended::QuotaExhausted {
                        line: quota_line(&request.route, error.resets_at),
                        error,
                    });
                }
                (ErrorClass::Overloaded, Some(seconds)) if retries < MAX_OVERLOADED_RETRIES => {
                    retries += 1;
                    sleeper.sleep(Duration::from_secs(seconds), interrupt);
                }
                _ => return Err(Ended::Gateway(error)),
            }
        }
    }

    /// Sends `request` once and reads its stream while `interrupt` can cancel it.
    fn attempt(
        &self,
        request: &TurnRequest,
        interrupt: &Interrupt,
        step: &mut Step,
    ) -> Result<Reply, Ended> {
        step.route = None;
        step.usage = None;
        let turn = self.client.turn(request).map_err(ended)?;
        read_unless_interrupted(turn, interrupt, step)
    }
}

/// The text of the answer to the side `request`; past `timeout` from now the turn is
/// cancelled at qb, and `interrupt` cancels it as it does a step.
pub(crate) fn side_turn(
    client: &Client,
    request: &TurnRequest,
    timeout: Option<Duration>,
    interrupt: &Interrupt,
) -> Result<String, Ended> {
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut turn = client.turn(request).map_err(Ended::Transport)?;
    if let Some(deadline) = deadline {
        turn.set_deadline(deadline);
    }
    let cancel = turn.cancel_handle();
    let step = &mut Step::new(request.request_id.clone());
    read_unless_interrupted(turn, interrupt, step)
        .map(|reply| text(&reply.message))
        .inspect_err(|ended| {
            if matches!(ended, Ended::Transport(Error::Timeout)) {
                let _ = cancel.cancel();
            }
        })
}

/// Reads `turn` while `interrupt` can end it; a turn the interrupt ended is cancelled at qb
/// before this returns.
pub(crate) fn read_unless_interrupted(
    turn: Turn,
    interrupt: &Interrupt,
    step: &mut Step,
) -> Result<Reply, Ended> {
    let cancel = turn.cancel_handle();
    if !interrupt.attach_stream(cancel.clone()) {
        let _ = cancel.cancel();
        return Err(Ended::Interrupted);
    }
    let reply = read(turn, step);
    interrupt.detach_stream();
    if matches!(reply, Err(Ended::Interrupted)) {
        // The answer of qb changes nothing: the local stream has ended either way.
        let _ = cancel.cancel();
    }
    reply
}

/// Compacts the turn's conversation, sized for requests reserving `max_tokens`, and makes a
/// compacted result its history; `None` when the interrupt fired.
fn compact(
    compaction: &mut Compaction<'_>,
    max_tokens: u64,
    live: &mut Live,
    interrupt: &Interrupt,
) -> Option<Compression> {
    let window = compaction.overflow_window(max_tokens);
    let now = (compaction.clock)();
    let compression =
        compaction
            .compressor
            .compress(&live.conversation(), window, now, interrupt)?;
    if let Compression::Compacted { messages, .. } = &compression {
        live.replace(messages.clone());
    }
    Some(compression)
}

/// The rough tokens of `before` when `compression` neither dropped a message from it nor cut
/// its rough tokens by 5 %: the overflow cannot be retried.
fn no_progress(before: &[Message], compression: &Compression) -> Option<usize> {
    let tokens = compress::messages_tokens_rough(before);
    let Compression::Compacted { messages, .. } = compression else {
        return Some(tokens);
    };
    let after = compress::messages_tokens_rough(messages);
    let progress = messages.len() < before.len()
        || (after > 0 && after.saturating_mul(100) < tokens.saturating_mul(95));
    (!progress).then_some(after)
}

/// Whether `error` is the vendor refusing the signature of a thinking block the request
/// replays, as when a changed system prompt or tool set no longer matches the prefix the block
/// was bound to: a `bad_request` whose message, lowercased, names `thinking` together with
/// `signature`, `cannot be modified`, `must remain as they were`, or `redacted_thinking` and
/// `invalid`.
fn thinking_signature(error: &GatewayError) -> bool {
    if error.class != ErrorClass::BadRequest {
        return false;
    }
    let Some(message) = error.message.as_deref() else {
        return false;
    };
    let message = message.to_lowercase();
    message.contains("thinking")
        && (message.contains("signature")
            || message.contains("cannot be modified")
            || message.contains("must remain as they were")
            || (message.contains("redacted_thinking") && message.contains("invalid")))
}

/// Whether a message of `request` carries a thinking block.
fn carries_thinking(request: &TurnRequest) -> bool {
    request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .any(|b| b.r#type == BlockType::Thinking)
}

/// A copy of `request` without thinking blocks; a message they were all of is left out and the
/// user messages without tool results it leaves adjacent are merged, as for prefills. The
/// conversation the turn keeps and stores is not touched.
fn without_thinking(request: &TurnRequest) -> TurnRequest {
    let mut messages: Vec<Message> = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        let content = message
            .content
            .iter()
            .filter(|b| b.r#type != BlockType::Thinking)
            .cloned();
        match messages.last_mut() {
            _ if message
                .content
                .iter()
                .all(|b| b.r#type == BlockType::Thinking) => {}
            Some(last) if plain_user(last) && plain_user(message) => last.content.extend(content),
            _ => messages.push(Message {
                role: message.role,
                content: content.collect(),
            }),
        }
    }
    TurnRequest {
        route: request.route.clone(),
        system: request.system.clone(),
        messages,
        tools: request.tools.clone(),
        max_tokens: request.max_tokens,
        effort: request.effort,
        session_key: request.session_key.clone(),
        cache: request.cache,
        request_id: request.request_id.clone(),
    }
}

/// Sends `reply`, which stopped for tool use without a call, and the nudge after it; both are
/// kept out of the history.
fn nudge_dropped_call(live: &mut Live, reply: Message) {
    live.push(reply, Kind::Nudge);
    live.push(
        text_message(Role::User, DROPPED_TOOL_CALL_NUDGE),
        Kind::Nudge,
    );
}

/// Applies `recovery` from the empty `reply`; false when the turn ends with an `(empty)` answer.
fn recover(
    live: &mut Live,
    recovery: Recovery,
    reply: Message,
    (sleeper, interrupt): (&mut dyn Sleeper, &Interrupt),
) -> bool {
    match recovery {
        Recovery::Nudge => {
            let mut stand_in = text_message(Role::Assistant, EMPTY_REPLY);
            let thinking = reply
                .content
                .into_iter()
                .filter(|b| b.r#type == BlockType::Thinking);
            stand_in.content.splice(0..0, thinking);
            live.push(stand_in, Kind::Nudge);
            live.push(
                text_message(Role::User, EMPTY_TOOL_RESPONSE_NUDGE),
                Kind::Nudge,
            );
        }
        Recovery::Prefill => live.push(reply, Kind::Prefill),
        Recovery::Retry(wait) => sleeper.sleep(wait, interrupt),
        Recovery::GiveUp => {
            live.push(text_message(Role::Assistant, EMPTY_REPLY), Kind::Kept);
            return false;
        }
    }
    true
}

/// Collects the blocks of one stream in order; consecutive text deltas form one block.
fn read(turn: Turn, step: &mut Step) -> Result<Reply, Ended> {
    let mut content: Vec<Block> = Vec::new();
    let mut calls = Vec::new();
    for item in turn {
        let StreamEvent {
            route,
            text,
            thinking,
            tool_call,
            usage,
            reason,
            error,
            ..
        } = item.map_err(ended)?;
        if let Some(route) = route {
            step.route = Some(route);
        }
        if let Some(usage) = usage {
            step.usage = Some(usage);
        }
        if let Some(delta) = text {
            match content.last_mut() {
                Some(Block {
                    r#type: BlockType::Text,
                    text: Some(text),
                    ..
                }) => text.push_str(&delta),
                _ => content.push(Block {
                    text: Some(delta),
                    ..block(BlockType::Text)
                }),
            }
        }
        if let Some(thinking) = thinking {
            content.push(Block {
                vendor: Some(thinking.vendor),
                opaque: Some(thinking.opaque),
                ..block(BlockType::Thinking)
            });
        }
        if let Some(call) = tool_call {
            content.push(Block {
                id: Some(call.id.clone()),
                name: Some(call.name.clone()),
                input: Some(call.input.clone()),
                ..block(BlockType::ToolUse)
            });
            calls.push(call);
        }
        if let Some(error) = error {
            return Err(Ended::Gateway(error));
        }
        if let Some(reason) = reason {
            return Ok(Reply {
                message: Message {
                    role: Role::Assistant,
                    content,
                },
                calls,
                reason,
            });
        }
    }
    Err(Ended::Transport(Error::Cut))
}

fn ended(error: Error) -> Ended {
    match error {
        Error::Rejected { error, .. } => Ended::Gateway(error),
        Error::Cancelled => Ended::Interrupted,
        other => Ended::Transport(other),
    }
}

/// A block of `kind` with no fields set.
pub fn block(kind: BlockType) -> Block {
    Block {
        r#type: kind,
        text: None,
        media_type: None,
        path: None,
        base64: None,
        id: None,
        name: None,
        input: None,
        content: None,
        is_error: None,
        vendor: None,
        opaque: None,
    }
}

/// A message of one text block.
pub fn text_message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![Block {
            text: Some(text.to_owned()),
            ..block(BlockType::Text)
        }],
    }
}

/// The text of a user message of one text block, the only shape that takes a turn context.
fn user_text(message: &mut Message) -> Option<&mut String> {
    match (message.content.as_mut_slice(), message.role) {
        ([block], Role::User) if block.r#type == BlockType::Text => block.text.as_mut(),
        _ => None,
    }
}

/// A user message that the reference keeps in the user role: it carries no tool result.
fn plain_user(message: &Message) -> bool {
    message.role == Role::User
        && !message
            .content
            .iter()
            .any(|b| b.r#type == BlockType::ToolResult)
}

/// The text blocks of `message`, joined.
pub fn text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter(|b| b.r#type == BlockType::Text)
        .filter_map(|b| b.text.as_deref())
        .collect()
}

fn quota_line(route: &str, resets_at: Option<u64>) -> String {
    match resets_at {
        Some(unix) => format!(
            "quota exhausted on route {route}; nearest reset {}",
            utc(unix)
        ),
        None => format!("quota exhausted on route {route}; the gateway gave no reset time"),
    }
}

/// `YYYY-MM-DD HH:MM:SS UTC` of a unix time.
pub(crate) fn utc(unix: u64) -> String {
    let (days, seconds) = (unix / 86_400, unix % 86_400);
    let (year, month, day) = crate::zone::civil_from_days(days.cast_signed());
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}
