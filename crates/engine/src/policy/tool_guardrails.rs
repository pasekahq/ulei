// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `tool-guardrails`: per-turn detection of repeated failed and non-progressing tool calls.
//! A warning is appended to the result of the call that crossed its threshold; with hard stop
//! on, a call past its block threshold does not run, and a block or the same-tool halt ends the
//! turn after its batch with the controlled halt answer.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Write;
use std::path::Path;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use ulei_contracts::generated::c4g::{Message, ToolCall};

use super::{Origin, Policy, Verdict};
use crate::config::{self, Config, Setting};
use crate::tools::{Output, hex, json_object, json_str};

/// Read-only tools whose unchanged result on an identical call is no progress.
pub const IDEMPOTENT_TOOLS: [&str; 4] = ["read_file", "search_files", "web_search", "web_extract"];
/// Tools that are never idempotent, whatever the idempotent list holds.
pub const MUTATING_TOOLS: [&str; 3] = ["terminal", "write_file", "patch"];

/// Thresholds and switches of [`ToolGuardrails`]; the default is the reference's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardrailConfig {
    /// Warnings are appended to results.
    pub warnings_enabled: bool,
    /// Calls are blocked and turns halted.
    pub hard_stop_enabled: bool,
    /// Failures of one identical call that warn.
    pub exact_failure_warn_after: usize,
    /// Failures of one identical call after which it is blocked.
    pub exact_failure_block_after: usize,
    /// Failures of one tool that warn.
    pub same_tool_failure_warn_after: usize,
    /// Failures of one tool that halt the turn.
    pub same_tool_failure_halt_after: usize,
    /// Identical results of one idempotent call that warn.
    pub no_progress_warn_after: usize,
    /// Identical results of one idempotent call after which it is blocked.
    pub no_progress_block_after: usize,
}

impl Default for GuardrailConfig {
    fn default() -> Self {
        Self {
            warnings_enabled: true,
            hard_stop_enabled: false,
            exact_failure_warn_after: 2,
            exact_failure_block_after: 5,
            same_tool_failure_warn_after: 3,
            same_tool_failure_halt_after: 8,
            no_progress_warn_after: 2,
            no_progress_block_after: 5,
        }
    }
}

impl GuardrailConfig {
    /// The `engine.tool_loop_guardrails` settings of `config`, each overridden by its
    /// environment variable as `env` reads it; an unset one keeps its default.
    ///
    /// # Errors
    /// [`config::Error::NotPositive`] for a count that is not a positive integer.
    pub fn from_settings(
        config: &Config,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Self, config::Error> {
        let d = Self::default();
        let switch = |s: Setting, default| config.switch(s, env(s.env), default);
        let count = |s: Setting, default| config.positive(s, env(s.env), default);
        Ok(Self {
            warnings_enabled: switch(config::GUARDRAIL_WARNINGS, d.warnings_enabled),
            hard_stop_enabled: switch(config::GUARDRAIL_HARD_STOP, d.hard_stop_enabled),
            exact_failure_warn_after: count(
                config::GUARDRAIL_WARN_EXACT_FAILURE,
                d.exact_failure_warn_after,
            )?,
            same_tool_failure_warn_after: count(
                config::GUARDRAIL_WARN_SAME_TOOL_FAILURE,
                d.same_tool_failure_warn_after,
            )?,
            no_progress_warn_after: count(
                config::GUARDRAIL_WARN_NO_PROGRESS,
                d.no_progress_warn_after,
            )?,
            exact_failure_block_after: count(
                config::GUARDRAIL_STOP_EXACT_FAILURE,
                d.exact_failure_block_after,
            )?,
            same_tool_failure_halt_after: count(
                config::GUARDRAIL_STOP_SAME_TOOL_FAILURE,
                d.same_tool_failure_halt_after,
            )?,
            no_progress_block_after: count(
                config::GUARDRAIL_STOP_NO_PROGRESS,
                d.no_progress_block_after,
            )?,
        })
    }
}

/// A tool name with the SHA-256 of its canonical arguments.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Signature {
    tool: String,
    args_hash: String,
}

impl Signature {
    fn of(call: &ToolCall) -> Self {
        let mut args = String::new();
        canonical_object(&call.input, &mut args);
        Self {
            tool: call.name.clone(),
            args_hash: hex(&Sha256::digest(args.as_bytes())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Warn,
    Block,
    Halt,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Block => "block",
            Self::Halt => "halt",
        }
    }
}

#[derive(Debug)]
struct Decision {
    action: Action,
    code: &'static str,
    message: String,
    tool: String,
    count: usize,
    signature: Signature,
}

impl Decision {
    /// The `{"error": ..., "guardrail": ...}` result of a blocked call.
    fn synthetic_result(&self) -> String {
        let signature = json_object([
            ("tool_name", json_str(&self.signature.tool)),
            ("args_hash", json_str(&self.signature.args_hash)),
        ]);
        let guardrail = json_object([
            ("action", json_str(self.action.as_str())),
            ("code", json_str(self.code)),
            ("message", json_str(&self.message)),
            ("tool_name", json_str(&self.tool)),
            ("count", self.count.to_string()),
            ("signature", signature),
        ]);
        json_object([("error", json_str(&self.message)), ("guardrail", guardrail)])
    }

    /// Appends the bracketed guidance of a warning or halt to `result`.
    fn append_guidance(&self, result: &mut String) {
        let label = if self.action == Action::Halt {
            "Tool loop hard stop"
        } else {
            "Tool loop warning"
        };
        let _ = write!(
            result,
            "\n\n[{label}: {}; count={}; {}]",
            self.code, self.count, self.message
        );
    }

    /// The final answer of a turn this decision halted.
    fn halt_answer(&self) -> String {
        let tool = if self.tool.is_empty() {
            "a tool"
        } else {
            &self.tool
        };
        format!(
            "I stopped retrying {tool} because it hit the tool-call guardrail ({}) after {} \
             repeated non-progressing attempts. The last tool result explains the blocker; the \
             next step is to change strategy instead of repeating the same call.",
            self.code, self.count
        )
    }
}

/// The per-turn counters of repeated failed and non-progressing calls.
#[derive(Debug, Default)]
pub struct ToolGuardrails {
    config: GuardrailConfig,
    exact_failures: HashMap<Signature, usize>,
    same_tool_failures: HashMap<String, usize>,
    /// Hash of the last result of each idempotent call and how many of its results in a row
    /// had that hash.
    no_progress: HashMap<Signature, ([u8; 32], usize)>,
    halt: Option<Decision>,
}

impl ToolGuardrails {
    /// The policy under `config`.
    pub fn new(config: GuardrailConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    fn is_idempotent(tool: &str) -> bool {
        !MUTATING_TOOLS.contains(&tool) && IDEMPOTENT_TOOLS.contains(&tool)
    }

    fn block(&self, call: &ToolCall) -> Option<Decision> {
        let signature = Signature::of(call);
        let tool = &call.name;
        let exact = self.exact_failures.get(&signature).copied().unwrap_or(0);
        let (code, count, message) = if exact >= self.config.exact_failure_block_after {
            (
                "repeated_exact_failure_block",
                exact,
                format!(
                    "Blocked {tool}: the same tool call failed {exact} times with identical \
                     arguments. Stop retrying it unchanged; change strategy or explain the \
                     blocker."
                ),
            )
        } else {
            let repeats = self
                .no_progress
                .get(&signature)
                .filter(|_| Self::is_idempotent(tool))
                .map_or(0, |&(_, n)| n);
            if repeats < self.config.no_progress_block_after {
                return None;
            }
            (
                "idempotent_no_progress_block",
                repeats,
                format!(
                    "Blocked {tool}: this read-only call returned the same result {repeats} \
                     times. Stop repeating it unchanged; use the result already provided or try \
                     a different query."
                ),
            )
        };
        Some(Decision {
            action: Action::Block,
            code,
            message,
            tool: tool.clone(),
            count,
            signature,
        })
    }

    fn after_call(&mut self, call: &ToolCall, result: &str, failed: bool) -> Option<Decision> {
        let signature = Signature::of(call);
        let tool = &call.name;
        let config = self.config;
        let decision = |action, code, message, count| {
            Some(Decision {
                action,
                code,
                message,
                tool: tool.clone(),
                count,
                signature: signature.clone(),
            })
        };
        if failed {
            let exact = self.exact_failures.entry(signature.clone()).or_insert(0);
            *exact += 1;
            let exact = *exact;
            self.no_progress.remove(&signature);
            let same = self.same_tool_failures.entry(tool.clone()).or_insert(0);
            *same += 1;
            let same = *same;
            if config.hard_stop_enabled && same >= config.same_tool_failure_halt_after {
                let message = format!(
                    "Stopped {tool}: it failed {same} times this turn. Stop retrying the same \
                     failing tool path and choose a different approach."
                );
                return decision(Action::Halt, "same_tool_failure_halt", message, same);
            }
            if config.warnings_enabled && exact >= config.exact_failure_warn_after {
                let message = format!(
                    "{tool} has failed {exact} times with identical arguments. This looks like a \
                     loop; inspect the error and change strategy instead of retrying it \
                     unchanged."
                );
                return decision(
                    Action::Warn,
                    "repeated_exact_failure_warning",
                    message,
                    exact,
                );
            }
            if config.warnings_enabled && same >= config.same_tool_failure_warn_after {
                let message = recovery_hint(tool, same);
                return decision(Action::Warn, "same_tool_failure_warning", message, same);
            }
            return None;
        }
        self.exact_failures.remove(&signature);
        self.same_tool_failures.remove(tool);
        if !Self::is_idempotent(tool) {
            self.no_progress.remove(&signature);
            return None;
        }
        let hash = result_hash(result);
        let repeats = match self.no_progress.get(&signature) {
            Some(&(previous, n)) if previous == hash => n + 1,
            _ => 1,
        };
        self.no_progress.insert(signature.clone(), (hash, repeats));
        if config.warnings_enabled && repeats >= config.no_progress_warn_after {
            let message = format!(
                "{tool} returned the same result {repeats} times. Use the result already \
                 provided or change the query instead of repeating it unchanged."
            );
            return decision(
                Action::Warn,
                "idempotent_no_progress_warning",
                message,
                repeats,
            );
        }
        None
    }

    /// Keeps the first decision that stops the turn.
    fn record_halt(&mut self, decision: Decision) {
        self.halt.get_or_insert(decision);
    }
}

impl Policy for ToolGuardrails {
    fn name(&self) -> &'static str {
        "tool-guardrails"
    }

    fn on_turn_start(&mut self, _origin: Origin, _input: &Message) {
        *self = Self::new(self.config);
    }

    fn pre_tool(&mut self, call: &ToolCall, _cwd: &Path) -> Verdict {
        if !self.config.hard_stop_enabled {
            return Verdict::Allow;
        }
        match self.block(call) {
            Some(decision) => {
                let result = decision.synthetic_result();
                self.record_halt(decision);
                Verdict::Synthetic(result)
            }
            None => Verdict::Allow,
        }
    }

    fn post_tool(&mut self, call: &ToolCall, output: &mut Output) {
        let failed = output.is_error || nonzero_exit(call, &output.content);
        if let Some(decision) = self.after_call(call, &output.content, failed) {
            decision.append_guidance(&mut output.content);
            if decision.action == Action::Halt {
                self.record_halt(decision);
            }
        }
    }

    fn halt(&mut self) -> Option<String> {
        self.halt.take().map(|decision| decision.halt_answer())
    }
}

/// A `terminal` result whose `exit_code` is set and not 0, which the reference counts as a
/// failure whether or not the call is `is_error`.
fn nonzero_exit(call: &ToolCall, content: &str) -> bool {
    if call.name != "terminal" {
        return false;
    }
    match serde_json::from_str::<Map<String, Value>>(content) {
        Ok(result) => match result.get("exit_code") {
            None | Some(Value::Null) => false,
            Some(code) => code.as_f64() != Some(0.0),
        },
        Err(_) => false,
    }
}

/// The reference's guidance after `count` failures of `tool` with different arguments.
fn recovery_hint(tool: &str, count: usize) -> String {
    let common = format!(
        "{tool} has failed {count} times this turn. This looks like a loop. Do not switch to \
         text-only replies; keep using tools, but diagnose before retrying. First inspect the \
         latest error/output and verify your assumptions. "
    );
    let specific = if tool == "terminal" {
        "For terminal failures, run a small diagnostic such as `pwd && ls -la` in the same tool, \
         then try an absolute path, a simpler command, a different working directory, or a \
         different tool such as read_file/write_file/patch."
    } else {
        "Try different arguments, a narrower query/path, an absolute path when relevant, or a \
         different tool that can make progress. If the blocker is external, report the blocker \
         after one diagnostic attempt instead of repeating the same failing path."
    };
    common + specific
}

/// SHA-256 of a result: of its canonical JSON when it parses as JSON, else of its text.
fn result_hash(result: &str) -> [u8; 32] {
    let digest = match serde_json::from_str::<Value>(result) {
        Ok(value) => {
            let mut text = String::new();
            canonical(&value, &mut text);
            Sha256::digest(text.as_bytes())
        }
        Err(_) => Sha256::digest(result.as_bytes()),
    };
    digest.into()
}

/// Writes `value` as compact JSON with the keys of every object sorted.
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => canonical_object(map, out),
        Value::Array(items) => {
            out.push('[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// Writes `map` as [`canonical`] writes an object.
fn canonical_object(map: &Map<String, Value>, out: &mut String) {
    let mut entries: Vec<(&String, &Value)> = map.iter().collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    out.push('{');
    for (n, (key, value)) in entries.into_iter().enumerate() {
        if n > 0 {
            out.push(',');
        }
        out.push_str(&json_str(key));
        out.push(':');
        canonical(value, out);
    }
    out.push('}');
}
