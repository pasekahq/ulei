//! In-loop policies: hooks before and after each tool call and at the end of a turn, run as an
//! ordered [`Chain`].

mod state_db_guard;

use std::path::Path;

use ulei_contracts::generated::c4g::{Message, ToolCall};

use crate::tools::{Context, Output, Registry};

pub use state_db_guard::{InvalidDbPath, StateDbGuard};

/// Whether a tool call may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Run the call.
    Allow,
    /// Do not run it; the reason is the call's `is_error` result.
    Deny(String),
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

    /// Called before a tool call runs; `cwd` is the call's working directory.
    fn pre_tool(&mut self, _call: &ToolCall, _cwd: &Path) -> Verdict {
        Verdict::Allow
    }

    /// Called with the output of a call that ran; may amend it.
    fn post_tool(&mut self, _call: &ToolCall, _output: &mut Output) {}

    /// Called with the final assistant message of a turn.
    fn on_stop(&mut self, _turn: &Message) -> StopDecision {
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

    /// The first denial, its reason prefixed with the policy name.
    pub fn pre_tool(&mut self, call: &ToolCall, cwd: &Path) -> Verdict {
        for policy in &mut self.policies {
            if let Verdict::Deny(reason) = policy.pre_tool(call, cwd) {
                return Verdict::Deny(format!("denied by policy {}: {reason}", policy.name()));
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

    /// The first request to continue.
    pub fn on_stop(&mut self, turn: &Message) -> StopDecision {
        for policy in &mut self.policies {
            if let StopDecision::Continue(message) = policy.on_stop(turn) {
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

    /// The pre hooks of `call`: `None` when it may run, else its result. A denied call counts
    /// as dispatched for the read tracker of `tools`.
    pub fn admit(&mut self, tools: &Registry, call: &ToolCall, cwd: &Path) -> Option<Output> {
        match self.pre_tool(call, cwd) {
            Verdict::Allow => None,
            Verdict::Deny(reason) => {
                tools.notify_other_tool_call(&call.name);
                Some(Output::error(reason))
            }
        }
    }
}
