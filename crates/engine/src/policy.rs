// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! In-loop policies: hooks at the start of a turn, before and after each tool call and at the
//! end of a turn, run as an ordered [`Chain`].

mod hardline;
mod no_ask_guard;
mod owner_urgent;
mod state_db_guard;
mod tool_guardrails;
mod turn_reminder;

use std::path::Path;

use ulei_contracts::generated::c4g::{Message, ToolCall};

use crate::tools::{Context, Interrupt, Output, Registry};

pub use hardline::{Hardline, MalformedGrep};
pub use no_ask_guard::{
    Classifier, DEFAULT_TIMEOUT, INSTRUCTION_FIELDS, NOTE_FIELDS, NoAskGuard, TextTemplate,
    ends_with_question,
};
pub use owner_urgent::OwnerUrgent;
pub use state_db_guard::{InvalidDbPath, StateDbGuard};
pub use tool_guardrails::{GuardrailConfig, IDEMPOTENT_TOOLS, MUTATING_TOOLS, ToolGuardrails};
pub use turn_reminder::TurnReminder;

/// Who wrote the input of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sender {
    /// The owner.
    Owner,
    /// An event: a schedule, a system notice, another agent.
    Event,
}

/// Whether a turn answers the owner on the main channel in a top-level session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The owner's input on the main channel, in a session without a parent.
    TopLevelOwner,
    /// Another channel, no channel, an event or a sub-session.
    Other,
}

impl Origin {
    /// [`Origin::TopLevelOwner`] when the owner sent the input on `channel` and `main` names
    /// that channel. Sessions have no parent yet, so every session is top-level; the parent
    /// check of session lineage belongs here.
    pub fn of(sender: Sender, channel: Option<&str>, main: Option<&str>) -> Self {
        if sender == Sender::Owner && channel.is_some() && channel == main {
            Self::TopLevelOwner
        } else {
            Self::Other
        }
    }
}

/// Whether a tool call may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Run the call.
    Allow,
    /// Do not run it; the reason is the call's `is_error` result.
    Deny(String),
    /// Do not run it; this text, as it stands, is the call's `is_error` result.
    Synthetic(String),
}

/// What happens after the model ended its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopDecision {
    /// The turn ends.
    Stop,
    /// The loop sends this message to the model and continues.
    Continue(String),
}

/// A hook of the tool loop. Every method defaults to doing nothing.
pub trait Policy {
    /// Name shown in denials.
    fn name(&self) -> &'static str;

    /// Called once before the first step of a turn of `origin` with its `input`.
    fn on_turn_start(&mut self, _origin: Origin, _input: &Message) {}

    /// Called once before the first step of a turn of `origin` whose input is a user message of
    /// one text block; text appended in place to the request copy of that block, none when
    /// empty.
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "implementations return text they own"
    )]
    fn turn_context(&self, _origin: Origin) -> &str {
        ""
    }

    /// Called before a tool call runs; `cwd` is the call's working directory.
    fn pre_tool(&mut self, _call: &ToolCall, _cwd: &Path) -> Verdict {
        Verdict::Allow
    }

    /// Called with the output of a call that ran; may amend it.
    fn post_tool(&mut self, _call: &ToolCall, _output: &mut Output) {}

    /// Called after the results of a batch of tool calls joined the turn; an answer ends the
    /// turn as its final assistant message.
    fn halt(&mut self) -> Option<String> {
        None
    }

    /// Called with the final assistant message of a turn; `interrupt` is the turn's.
    fn on_stop(&mut self, _turn: &Message, _interrupt: &Interrupt) -> StopDecision {
        StopDecision::Stop
    }
}

/// Policies run in order: the first denial or continue wins and later policies are not asked;
/// every policy amends the output of a call that ran.
#[derive(Default)]
pub struct Chain {
    policies: Vec<Box<dyn Policy>>,
}

impl Chain {
    /// A chain of `policies` in this order.
    pub fn new(policies: Vec<Box<dyn Policy>>) -> Self {
        Self { policies }
    }

    /// Shows every policy the `input` of a turn of `origin`.
    pub fn on_turn_start(&mut self, origin: Origin, input: &Message) {
        for policy in &mut self.policies {
            policy.on_turn_start(origin, input);
        }
    }

    /// Appends `first`, then the context of every policy in order, to `input`, the text of a
    /// user message of one text block, each non-empty one after a blank line.
    pub fn turn_context(&self, origin: Origin, first: Option<&str>, input: &mut String) {
        let policies = self
            .policies
            .iter()
            .map(|policy| policy.turn_context(origin));
        for part in first.into_iter().chain(policies) {
            if !part.is_empty() {
                input.push_str("\n\n");
                input.push_str(part);
            }
        }
    }

    /// The first refusal: a denial with its reason prefixed with the policy name, or a
    /// synthetic result as it stands.
    pub fn pre_tool(&mut self, call: &ToolCall, cwd: &Path) -> Verdict {
        for policy in &mut self.policies {
            match policy.pre_tool(call, cwd) {
                Verdict::Allow => {}
                Verdict::Deny(reason) => {
                    return Verdict::Deny(format!("denied by policy {}: {reason}", policy.name()));
                }
                synthetic @ Verdict::Synthetic(_) => return synthetic,
            }
        }
        Verdict::Allow
    }

    /// Lets every policy amend `output` in order.
    pub fn post_tool(&mut self, call: &ToolCall, output: &mut Output) {
        for policy in &mut self.policies {
            policy.post_tool(call, output);
        }
    }

    /// The first answer that ends the turn after a tool batch.
    pub fn halt(&mut self) -> Option<String> {
        self.policies.iter_mut().find_map(|policy| policy.halt())
    }

    /// The first request to continue.
    pub fn on_stop(&mut self, turn: &Message, interrupt: &Interrupt) -> StopDecision {
        for policy in &mut self.policies {
            if let StopDecision::Continue(message) = policy.on_stop(turn, interrupt) {
                return StopDecision::Continue(message);
            }
        }
        StopDecision::Stop
    }

    /// Runs `call` from `tools` between the pre and post hooks; a denied call does not run.
    pub fn run_tool(&mut self, tools: &Registry, call: &ToolCall, ctx: Context<'_>) -> Output {
        if let Some(denied) = self.admit(tools, call, ctx.cwd) {
            return denied;
        }
        let mut output = tools.execute(&call.name, &call.input, ctx);
        self.post_tool(call, &mut output);
        output
    }

    /// The pre hooks of `call`: `None` when it may run, else its result. A refused call counts
    /// as dispatched for the read tracker of `tools`.
    pub fn admit(&mut self, tools: &Registry, call: &ToolCall, cwd: &Path) -> Option<Output> {
        let refused = match self.pre_tool(call, cwd) {
            Verdict::Allow => return None,
            Verdict::Deny(reason) => Output::error(reason),
            Verdict::Synthetic(content) => Output {
                content,
                is_error: true,
            },
        };
        tools.notify_other_tool_call(&call.name);
        Some(refused)
    }
}
