//! The tool loop: one model turn on the qb gateway, the tool calls it makes run through the
//! policy chain, their results sent back, until the model stops. Every request is built only
//! from the loop's inputs, so a replayed conversation sends the same bytes.

use std::path::Path;
use std::time::Duration;

use ulei_contracts::generated::c4g::{
    Block, BlockType, Effort, Error as GatewayError, ErrorClass, Message, Role, RouteInfo,
    StopReason, StreamEvent, ToolCall, TurnRequest, Usage,
};

use crate::gateway::{Client, Error, Turn};
use crate::policy::{Chain, StopDecision};
use crate::tools::{Context, Interrupt, Registry};

/// Most retries of one step that the gateway answered with `overloaded`.
pub const MAX_OVERLOADED_RETRIES: u32 = 3;

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

/// How a loop ended.
#[derive(Debug)]
pub enum Ended {
    /// The model stopped for this reason without tool calls and no policy continued.
    Stopped(StopReason),
    /// The max-steps bound was reached; no further request was sent.
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
}

/// The assistant message of one step, its tool calls and its stop reason.
struct Reply {
    message: Message,
    calls: Vec<ToolCall>,
    reason: StopReason,
}

impl Agent<'_> {
    /// Runs the loop after `history`. Step `n` sends `request_id(n)`; at most `max_steps` steps
    /// send a request. A step with tool calls runs them in order and continues; a step without
    /// ends the loop with its stop reason unless `end` and a policy continues.
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
        let mut request = TurnRequest {
            route: template.route.clone(),
            system: template.system.clone(),
            messages: history.to_vec(),
            tools: (!tools.is_empty()).then_some(tools),
            max_tokens: template.max_tokens,
            effort: template.effort,
            session_key: template.session_key.clone(),
            cache: None,
            request_id: String::new(),
        };
        let mut steps = Vec::new();
        let ended = loop {
            if steps.len() == max_steps {
                break Ended::MaxSteps;
            }
            request.request_id = request_id(steps.len());
            let mut step = Step {
                request_id: request.request_id.clone(),
                route: None,
                usage: None,
            };
            let reply = self.answer(&request, interrupt, sleeper, &mut step);
            steps.push(step);
            let Reply {
                message,
                calls,
                reason,
            } = match reply {
                Ok(reply) => reply,
                Err(ended) => break ended,
            };
            if !message.content.is_empty() {
                request.messages.push(message.clone());
            }
            if !calls.is_empty() {
                let results = calls
                    .iter()
                    .map(|call| {
                        let ctx = Context {
                            cwd: self.cwd,
                            interrupt,
                        };
                        let output = self.chain.run_tool(self.tools, call, ctx);
                        Block {
                            id: Some(call.id.clone()),
                            content: Some(output.content),
                            is_error: Some(output.is_error),
                            ..block(BlockType::ToolResult)
                        }
                    })
                    .collect();
                request.messages.push(Message {
                    role: Role::User,
                    content: results,
                });
                if interrupt.is_interrupted() {
                    break Ended::Interrupted;
                }
                continue;
            }
            if reason == StopReason::End
                && let StopDecision::Continue(text) = self.chain.on_stop(&message)
            {
                request.messages.push(Message {
                    role: Role::User,
                    content: vec![Block {
                        text: Some(text),
                        ..block(BlockType::Text)
                    }],
                });
                continue;
            }
            break Ended::Stopped(reason);
        };
        Outcome {
            messages: request.messages.split_off(history.len()),
            steps,
            ended,
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
fn block(kind: BlockType) -> Block {
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
