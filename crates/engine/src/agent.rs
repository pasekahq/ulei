// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The tool loop: one model turn on the qb gateway, the tool calls it makes run through the
//! policy chain, their results sent back, until the model stops or the step limit asks for a
//! summary. Every request is built only from the loop's inputs, so a replayed conversation
//! sends the same bytes.

mod dispatch;
mod empty;
mod summary;
mod think;

use std::fmt;
use std::path::Path;
use std::time::Duration;

use ulei_contracts::generated::c4g::{
    Block, BlockType, Effort, Error as GatewayError, ErrorClass, Message, Role, RouteInfo,
    StopReason, StreamEvent, ToolCall, TurnRequest, Usage,
};

use crate::gateway::{Client, Error, Turn};
use crate::policy::{Chain, StopDecision};
use crate::tools::{Context, Interrupt, Registry};
use empty::{EMPTY_REPLY, EMPTY_TOOL_RESPONSE_NUDGE, Guard, Recovery};

pub use dispatch::DEFAULT_CONCURRENT_TOOL_TIMEOUT;

/// Most retries of one step that the gateway answered with `overloaded`.
pub const MAX_OVERLOADED_RETRIES: u32 = 3;
/// Default step limit of one turn.
pub const DEFAULT_MAX_STEPS: usize = 200;

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
}

/// Waits between retries; tests inject one that records instead of sleeping.
pub trait Sleeper {
    /// Blocks the loop for `duration`.
    fn sleep(&mut self, duration: Duration);
}

/// A [`Sleeper`] that blocks the calling thread.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
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
    fn new(request_id: String) -> Self {
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
    /// The interrupt handle fired.
    Interrupted,
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
            Self::Interrupted => write!(f, "the turn was interrupted"),
            Self::QuotaExhausted { line, .. } => write!(f, "{line}"),
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
}

/// The assistant message of one step, its tool calls and its stop reason.
struct Reply {
    message: Message,
    calls: Vec<ToolCall>,
    reason: StopReason,
}

/// Where a message of the turn stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// It stays in the conversation.
    Kept,
    /// A thinking-only reply sent back for the model to go on from.
    Prefill,
    /// The `(empty)` stand-in of a reply to tool results, or the nudge after it.
    Nudge,
}

/// The messages the next request sends; of those after the history, the turn keeps the
/// [`Kind::Kept`] ones.
struct Live {
    request: TurnRequest,
    history: usize,
    kinds: Vec<Kind>,
}

impl Live {
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

    fn kept(mut self) -> Vec<Message> {
        self.request
            .messages
            .split_off(self.history)
            .into_iter()
            .zip(self.kinds)
            .filter_map(|(message, kind)| (kind == Kind::Kept).then_some(message))
            .collect()
    }
}

impl Agent<'_> {
    /// Runs the loop after `history`. Step `n` sends `request_id(n)`. A step with tool calls
    /// runs them as [`crate::tools::plan`] splits them, sends their results in call order and
    /// continues; a reply to `end` or `tool_use` without visible text is nudged, sent back or
    /// retried; any other step ends the loop with its stop reason unless `end` and a policy
    /// continues. After `max_steps` steps one more request without tools asks for a summary.
    pub fn run(
        &mut self,
        history: &[Message],
        request_id: &dyn Fn(usize) -> String,
        interrupt: &Interrupt,
        max_steps: usize,
        sleeper: &mut dyn Sleeper,
    ) -> Outcome {
        let template = self.template;
        let tools = self.tools.definitions();
        let mut live = Live {
            request: TurnRequest {
                route: template.route.clone(),
                system: template.system.clone(),
                messages: history.to_vec(),
                tools: (!tools.is_empty()).then_some(tools),
                max_tokens: template.max_tokens,
                effort: template.effort,
                session_key: template.session_key.clone(),
                cache: None,
                request_id: String::new(),
            },
            history: history.len(),
            kinds: Vec::new(),
        };
        let mut guard = Guard::new(self.empty_guard);
        let mut steps: Vec<Step> = Vec::new();
        let ended = loop {
            if steps.len() == max_steps {
                break self.summarize(
                    &mut live, &mut steps, request_id, interrupt, sleeper, max_steps,
                );
            }
            live.request.request_id = request_id(steps.len());
            let mut step = Step::new(live.request.request_id.clone());
            let reply = self.answer(&live.request, interrupt, sleeper, &mut step);
            let Reply {
                message,
                calls,
                reason,
            } = match reply {
                Ok(reply) => reply,
                Err(ended) => {
                    steps.push(step);
                    break ended;
                }
            };
            if !calls.is_empty() {
                steps.push(step);
                let prefilled = live.drop_trailing(&[Kind::Prefill]);
                live.push(message, Kind::Kept);
                live.push(self.call_tools(&calls, interrupt), Kind::Kept);
                guard.tools_ran(prefilled);
                if interrupt.is_interrupted() {
                    break Ended::Interrupted;
                }
                continue;
            }
            if matches!(reason, StopReason::End | StopReason::ToolUse) {
                let content = text(&message);
                if empty::visible(&content) {
                    guard.visible();
                } else {
                    let recovery =
                        guard.recover(&message, &content, &live.request.messages, &step, reason);
                    steps.push(step);
                    if recover(&mut live, recovery, message, sleeper) {
                        continue;
                    }
                    break Ended::Stopped(reason);
                }
            }
            steps.push(step);
            live.drop_trailing(&[Kind::Prefill, Kind::Nudge]);
            let decision = if reason == StopReason::End {
                self.chain.on_stop(&message)
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
        Outcome {
            messages: live.kept(),
            steps,
            ended,
        }
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

    /// One step: the turn, retried while the gateway is overloaded.
    fn answer(
        &self,
        request: &TurnRequest,
        interrupt: &Interrupt,
        sleeper: &mut dyn Sleeper,
        step: &mut Step,
    ) -> Result<Reply, Ended> {
        let mut retries = 0;
        loop {
            if interrupt.is_interrupted() {
                return Err(Ended::Interrupted);
            }
            let error = match self.attempt(request, interrupt, step) {
                Err(Ended::Gateway(error)) => error,
                other => return other,
            };
            match (error.class, error.retry_after) {
                (ErrorClass::QuotaExhausted, _) => {
                    return Err(Ended::QuotaExhausted {
                        line: quota_line(&request.route, error.resets_at),
                        error,
                    });
                }
                (ErrorClass::Overloaded, Some(seconds)) if retries < MAX_OVERLOADED_RETRIES => {
                    retries += 1;
                    sleeper.sleep(Duration::from_secs(seconds));
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
        if !interrupt.attach_stream(turn.cancel_handle()) {
            // The interrupt fired after the check but before the request went out.
            let _ = turn.cancel_handle().cancel();
            return Err(Ended::Interrupted);
        }
        let reply = read(turn, step);
        interrupt.detach_stream();
        reply
    }
}

/// Applies `recovery` from the empty `reply`; false when the turn ends with an `(empty)` answer.
fn recover(live: &mut Live, recovery: Recovery, reply: Message, sleeper: &mut dyn Sleeper) -> bool {
    match recovery {
        Recovery::Nudge => {
            let mut stand_in: Vec<Block> = reply
                .content
                .into_iter()
                .filter(|b| b.r#type == BlockType::Thinking)
                .collect();
            stand_in.push(text_block(EMPTY_REPLY));
            let stand_in = Message {
                role: Role::Assistant,
                content: stand_in,
            };
            live.push(stand_in, Kind::Nudge);
            live.push(
                text_message(Role::User, EMPTY_TOOL_RESPONSE_NUDGE),
                Kind::Nudge,
            );
        }
        Recovery::Prefill => live.push(reply, Kind::Prefill),
        Recovery::Retry(wait) => sleeper.sleep(wait),
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

/// A block of text.
fn text_block(text: &str) -> Block {
    Block {
        text: Some(text.to_owned()),
        ..block(BlockType::Text)
    }
}

/// A message of one text block.
pub fn text_message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![text_block(text)],
    }
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

/// `YYYY-MM-DD HH:MM:SS UTC` of a unix time, by H. Hinnant's `civil_from_days`, so no date
/// crate is needed.
fn utc(unix: u64) -> String {
    let (days, seconds) = (unix / 86_400, unix % 86_400);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}
