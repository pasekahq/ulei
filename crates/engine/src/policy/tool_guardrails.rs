// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `tool-guardrails`: per-turn detection of repeated failed and non-progressing tool calls.
//! A warning is appended to the result of the call that crossed its threshold; with hard stop
//! on, a call past its block threshold does not run. A `web_search` past the turn's cap never
//! runs, hard stop or not. A block or the same-tool halt ends the turn after its batch with the
//! controlled halt answer. With stall guards on, a run of identical calls with identical results
//! gets a notice from its third call and a reference stub for a long result from its second.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Write;
use std::path::Path;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use ulei_contracts::generated::c4g::{Message, ToolCall};

use super::{Origin, Policy, Verdict};
use crate::config::{self, Config, Setting};
use crate::tools::{Output, char_byte, hex, json_object, json_str};

/// Read-only tools whose unchanged result on an identical call is no progress.
pub const IDEMPOTENT_TOOLS: [&str; 4] = ["read_file", "search_files", "web_search", "web_extract"];
/// Tools that are never idempotent, whatever the idempotent list holds.
pub const MUTATING_TOOLS: [&str; 3] = ["terminal", "write_file", "patch"];

/// The consecutive identical call with an identical result from which the notice is appended.
const STALL_GUARD_IDENTICAL_CALL_THRESHOLD: usize = 3;
/// Characters below which a repeated result is kept whole.
const IDENTICAL_RESULT_STUB_MIN_CHARS: usize = 512;
/// Characters of the canonical arguments a reference stub carries.
const RESULT_STUB_ARGS_PREVIEW_CHARS: usize = 120;

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
    /// Identical-call notices and result-reference stubs are added.
    pub stall_guards: bool,
    /// `web_search` calls one turn may make; 0 is no limit.
    pub max_web_searches: usize,
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
            stall_guards: true,
            max_web_searches: 50,
        }
    }
}

impl GuardrailConfig {
    /// The `engine.tool_loop_guardrails` and `engine.stall_guards` settings of `config`, each
    /// overridden by its environment variable as `env` reads it; an unset one keeps its default.
    ///
    /// # Errors
    /// [`config::Error::NotPositive`] for a threshold that is not a positive integer,
    /// [`config::Error::NotCount`] for a cap that is not a non-negative integer.
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
            stall_guards: switch(config::STALL_GUARDS, d.stall_guards),
            max_web_searches: config.count(
                config::GUARDRAIL_MAX_WEB_SEARCHES,
                env(config::GUARDRAIL_MAX_WEB_SEARCHES.env),
                d.max_web_searches,
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

/// The current run of consecutive identical calls with identical results.
#[derive(Debug)]
struct Streak {
    signature: Signature,
    result_hash: [u8; 32],
    count: usize,
    /// Call id of the first call of the run, whose result the stub refers to.
    first_id: String,
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
    streak: Option<Streak>,
    web_searches: usize,
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

    /// The block of a `web_search` once the turn made its cap of them; counts an allowed one.
    fn loop_cap(&mut self, call: &ToolCall) -> Option<Decision> {
        if call.name != "web_search" {
            return None;
        }
        let cap = self.config.max_web_searches;
        if cap == 0 || self.web_searches < cap {
            self.web_searches += 1;
            return None;
        }
        Some(Decision {
            action: Action::Block,
            code: "loop_web_search_cap",
            message: format!(
                "Blocked web_search: this turn has already made {cap} web searches, the per-turn \
                 limit. This looks like a runaway search loop. Work with the results you already \
                 have and give the user your answer."
            ),
            tool: call.name.clone(),
            count: self.web_searches,
            signature: Signature::of(call),
        })
    }

    /// Extends or restarts the run of identical calls with `call` and its `result`; returns the
    /// notice to append and the stub that replaces `result`.
    fn observe(
        &mut self,
        call: &ToolCall,
        result: &str,
        failed: bool,
    ) -> (Option<String>, Option<String>) {
        let signature = Signature::of(call);
        let result_hash = result_hash(result);
        let streak = match &mut self.streak {
            Some(streak) if streak.signature == signature && streak.result_hash == result_hash => {
                streak.count += 1;
                streak
            }
            other => other.insert(Streak {
                signature,
                result_hash,
                count: 1,
                first_id: call.id.clone(),
            }),
        };
        let tool = &call.name;
        let count = streak.count;
        let notice = (count >= STALL_GUARD_IDENTICAL_CALL_THRESHOLD).then(|| {
            format!(
                "[note: this is the {} consecutive identical call to {tool} with identical \
                 arguments returning the same result. Do not repeat it — change arguments, use a \
                 different tool, or proceed with what you have.]",
                ordinal(count)
            )
        });
        let stub =
            (count >= 2 && !failed && result.chars().count() >= IDENTICAL_RESULT_STUB_MIN_CHARS)
                .then(|| reference_stub(call, &streak.first_id));
        (notice, stub)
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
        let decision = self.loop_cap(call).or_else(|| {
            self.config
                .hard_stop_enabled
                .then(|| self.block(call))
                .flatten()
        });
        match decision {
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
        let decision = self.after_call(call, &output.content, failed);
        let (notice, stub) = if self.config.stall_guards {
            self.observe(call, &output.content, failed)
        } else {
            (None, None)
        };
        if let Some(stub) = stub {
            output.content = stub;
        }
        if let Some(decision) = decision {
            decision.append_guidance(&mut output.content);
            if decision.action == Action::Halt {
                self.record_halt(decision);
            }
        }
        if let Some(notice) = notice {
            output.content.push_str("\n\n");
            output.content.push_str(&notice);
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

/// The text that replaces a result of `call` identical to the result of call `first_id`.
fn reference_stub(call: &ToolCall, first_id: &str) -> String {
    let mut args = String::new();
    canonical_object(&call.input, &mut args);
    let cut = char_byte(&args, RESULT_STUB_ARGS_PREVIEW_CHARS);
    if cut < args.len() {
        args.truncate(cut);
        args.push('…');
    }
    let reference = if first_id.is_empty() {
        String::new()
    } else {
        format!(" (tool_call_id {first_id})")
    };
    format!(
        "[note: this result is byte-identical to the {} result earlier this turn{reference}. \
         Refer to that result; it has not changed. Args: {args}]",
        call.name
    )
}

/// `n` with its English ordinal suffix: `1st`, `2nd`, `3rd`, `4th`, `11th`, `21st`.
fn ordinal(n: usize) -> String {
    let suffix = match (n % 100, n % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
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
    crate::sort::sort_by(&mut entries, |a, b| a.0.cmp(b.0));
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
