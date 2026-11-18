// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Context compaction of a session's live set: old tool output pruned, the head and a
//! token-budget tail kept verbatim, the middle summarized by one tool-less turn on the qb route
//! alias of [`Settings::route`] (an earlier summary updated, not repeated), a deterministic
//! summary when that turn fails, and the result stored with [`Journal::compact`].

pub mod text;

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};
use ulei_contracts::generated::c4g::{Block, BlockType, Message, Role, TurnRequest};

use crate::agent::{self, Ended, block, text_message};
use crate::config::{self, Config, Setting};
use crate::gateway::Client;
use crate::journal::{self, Journal};
use crate::tools::redact::Redactor;
use crate::tools::{Interrupt, py_strip, random_hex};
use crate::zone::{Zone, civil_from_days};
use text::{
    DUPLICATE_TOOL_OUTPUT, MERGED_PRIOR_CONTEXT_HEADER, MERGED_SUMMARY_DELIMITER,
    NO_USER_TASK_SENTINEL, PRUNED_TOOL_PLACEHOLDER, PromptInput, SUMMARY_END_MARKER,
};

/// Route alias of the summary turn when none is configured.
pub const DEFAULT_ROUTE: &str = "compact";
/// Deadline of the summary turn when none is configured.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(450);
/// Messages kept verbatim at the start of a first compaction.
pub const PROTECT_FIRST_N: usize = 3;
/// Recent messages the tail keeps at least, up to [`MAX_TAIL_MESSAGE_FLOOR`].
pub const DEFAULT_PROTECT_LAST_N: usize = 20;
/// Share of the threshold kept as the verbatim tail, clamped to 0.10..=0.80.
pub const DEFAULT_TARGET_RATIO: f64 = 0.20;
/// Seconds without a summary turn after a failed one.
pub const SUMMARY_FAILURE_COOLDOWN_SECONDS: i64 = 600;

const MIN_SUMMARY_TOKENS: usize = 2000;
const SUMMARY_RATIO: f64 = 0.20;
const SUMMARY_TOKENS_CEILING: usize = 10_000;
const PRUNE_MIN_CHARS: usize = 200;
const CHARS_PER_TOKEN: usize = 4;
const MAX_TAIL_MESSAGE_FLOOR: usize = 8;
const PRESSURE_KEEP_RECENT_MESSAGES: usize = 3;
const RESTART_HANDOFF_PROBE_EXTRA_MESSAGES: usize = 4;
const TOOL_ARGS_TRUNCATE_OVER: usize = 500;
const TOOL_ARGS_STRING_HEAD: usize = 200;
const ERROR_TEXT_MAX_CHARS: usize = 220;
const TOOL_CALL_REMOVED: &str = "(tool call removed)";
/// `origin` of a row the compaction wrote.
const NEW_ROW: usize = usize::MAX;

/// Where the summary turn goes and what the compaction keeps.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Route alias of the summary turn.
    pub route: String,
    /// Deadline of the summary turn; `None` waits without one.
    pub timeout: Option<Duration>,
    /// Upper bound of output tokens of the summary turn.
    pub max_tokens: u64,
    /// Messages kept at the start of a first compaction.
    pub protect_first_n: usize,
    /// Recent messages the tail keeps at least, up to 8.
    pub protect_last_n: usize,
    /// Share of the threshold kept as the verbatim tail.
    pub target_ratio: f64,
}

impl Settings {
    /// The settings of `config`, each overridden by its environment variable as `env` reads
    /// it; `max_tokens` is the engine's output bound per model turn.
    ///
    /// # Errors
    /// [`config::Error`] for a value that is not of its kind.
    pub fn from_settings(
        config: &Config,
        env: &dyn Fn(&str) -> Option<OsString>,
        max_tokens: u64,
    ) -> Result<Self, config::Error> {
        let value = |s: Setting| env(s.env);
        Ok(Self {
            route: config
                .text(config::COMPRESSION_ROUTE, value(config::COMPRESSION_ROUTE))?
                .unwrap_or_else(|| DEFAULT_ROUTE.to_owned()),
            timeout: config.timeout(
                config::COMPRESSION_TIMEOUT,
                value(config::COMPRESSION_TIMEOUT),
                DEFAULT_TIMEOUT,
            )?,
            max_tokens,
            protect_first_n: PROTECT_FIRST_N,
            protect_last_n: config.count(
                config::COMPRESSION_PROTECT_LAST_N,
                value(config::COMPRESSION_PROTECT_LAST_N),
                DEFAULT_PROTECT_LAST_N,
            )?,
            target_ratio: config.number(
                config::COMPRESSION_TARGET_RATIO,
                value(config::COMPRESSION_TARGET_RATIO),
                DEFAULT_TARGET_RATIO,
            )?,
        })
    }
}

/// The context window of the route the session talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Context length in tokens.
    pub context_length: u64,
    /// Prompt tokens at which compaction runs; the tail budget is a share of it.
    pub threshold_tokens: u64,
}

/// Why a compaction left the messages as they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// The transcript is no longer than the head and the shortest tail.
    TooFewMessages,
    /// The head and the tail cover the transcript.
    NoWindow,
    /// The window holds only an earlier summary.
    NothingNew,
}

/// The summary a compaction wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Summary {
    /// The summary turn answered.
    Model,
    /// The deterministic summary: the turn failed for `reason`, or was not sent during the
    /// cooldown after a failure (`None`).
    Fallback {
        /// The failure as the summary names it.
        reason: Option<String>,
    },
}

/// The result of a compaction.
#[derive(Debug, Clone, PartialEq)]
pub enum Compression {
    /// The new live set: head, summary, tail.
    Compacted {
        /// The messages in order.
        messages: Vec<Message>,
        /// How the summary was made.
        summary: Summary,
    },
    /// Nothing to compact.
    Skipped(Skip),
}

/// Failure of [`Compressor::compact`].
#[derive(Debug)]
pub enum Error {
    /// The interrupt handle fired during the summary turn; nothing changed.
    Interrupted,
    /// Reading or storing the live set failed.
    Journal(journal::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interrupted => write!(f, "the compaction was interrupted"),
            Self::Journal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Journal(e) => Some(e),
            Self::Interrupted => None,
        }
    }
}

impl From<journal::Error> for Error {
    fn from(e: journal::Error) -> Self {
        Self::Journal(e)
    }
}

/// The compactions of one engine: the settings and the state carried between compactions.
pub struct Compressor {
    client: Client,
    settings: Settings,
    redactor: Redactor,
    zone: Zone,
    previous_summary: Option<String>,
    summary_has_user_turn: Option<bool>,
    compressions: u64,
    cooldown_until: i64,
}

/// The rows a summary replaces and where the tail starts.
struct SummaryWindow {
    turns: Vec<Row>,
    indices: HashSet<usize>,
    tail_start: usize,
}

/// What the summary turn gave.
enum Generated {
    Summary(String),
    Failed(Option<String>),
}

impl Compressor {
    /// A compressor with no earlier summary; `zone` dates the summary prompt.
    pub fn new(client: Client, settings: Settings, redactor: Redactor, zone: Zone) -> Self {
        Self {
            client,
            settings,
            redactor,
            zone,
            previous_summary: None,
            summary_has_user_turn: None,
            compressions: 0,
            cooldown_until: 0,
        }
    }

    /// Compacts the live set of `session` at unix time `now` and stores a compacted result
    /// as the new live set.
    ///
    /// # Errors
    /// [`Error::Interrupted`] with the journal unchanged, or [`Error::Journal`].
    pub fn compact(
        &mut self,
        journal: &mut Journal,
        session: u64,
        window: Window,
        now: i64,
        interrupt: &Interrupt,
    ) -> Result<Compression, Error> {
        let live = journal.messages(session)?;
        let compression = self.compress(&live, window, now, interrupt)?;
        if let Compression::Compacted { messages, .. } = &compression {
            journal.compact(session, messages)?;
        }
        Ok(compression)
    }

    /// The compacted form of `messages` at unix time `now`.
    ///
    /// # Errors
    /// [`Error::Interrupted`] when the interrupt fired during the summary turn.
    pub fn compress(
        &mut self,
        messages: &[Message],
        window: Window,
        now: i64,
        interrupt: &Interrupt,
    ) -> Result<Compression, Error> {
        let mut rows = flatten(messages);
        if rows.len() <= self.protect_head_size(&rows) + 3 + 1 {
            return Ok(Compression::Skipped(Skip::TooFewMessages));
        }
        let tail_budget = self.tail_token_budget(window);
        self.prune_old_tool_results(&mut rows, tail_budget);
        let echoes = blank_echoes_after(&rows, last_user_index(&rows, 0));
        if !echoes.is_empty() {
            rows = rows
                .into_iter()
                .enumerate()
                .filter_map(|(i, row)| (!echoes.contains(&i)).then_some(row))
                .collect();
        }
        let latest_user = last_user_index(&rows, 0);
        let start = align_forward(&rows, self.protect_head_size(&rows));
        let mut end = self.tail_cut(&rows, start, tail_budget);
        if Some(end) == latest_user {
            let bridge = match end.checked_sub(1).map(|i| rows[i].kind) {
                Some(Kind::Tool) => Some(align_backward(&rows, end)),
                Some(Kind::Assistant) => Some(end - 1),
                _ => None,
            };
            if let Some(bridge) = bridge.filter(|b| *b > start) {
                end = bridge;
            }
        }
        if start >= end {
            return Ok(Compression::Skipped(Skip::NoWindow));
        }

        let before = (self.previous_summary.clone(), self.summary_has_user_turn);
        let window_rows = self.summary_window(&rows, start, end);
        if window_rows.turns.is_empty() {
            return Ok(Compression::Skipped(Skip::NothingNew));
        }
        let turns = &window_rows.turns;

        let focus = text::auto_focus_topic(self.redactor, &rows);
        let Some(generated) = self.generate_summary(turns, focus, window, now, interrupt) else {
            (self.previous_summary, self.summary_has_user_turn) = before;
            return Err(Error::Interrupted);
        };

        let (summary_text, summary) = match generated {
            Generated::Summary(text) => (text, Summary::Model),
            Generated::Failed(reason) => (
                text::static_fallback_summary(
                    self.redactor,
                    turns,
                    self.previous_summary.as_deref(),
                    reason.as_deref(),
                ),
                Summary::Fallback { reason },
            ),
        };
        let mut compressed = assemble(&rows, start, end, &window_rows, summary_text);
        self.compressions += 1;
        sanitize_tool_pairs(&mut compressed);
        Ok(Compression::Compacted {
            messages: unflatten(compressed),
            summary,
        })
    }

    /// The rows to summarize between `start` and `end`, earlier summaries left out and their
    /// bodies taken as the previous summary.
    fn summary_window(&mut self, rows: &[Row], start: usize, end: usize) -> SummaryWindow {
        let hits: Vec<(usize, String)> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| text::is_summary_content(&row.text()))
            .map(|(i, row)| (i, text::strip_summary_prefix(&row.text())))
            .collect();
        let real_user = text::has_real_user_turn(rows);
        let indices: HashSet<usize> = hits.iter().map(|(i, _)| *i).collect();
        let mut turns: Vec<Row> = rows[start..end].to_vec();
        let mut tail_start = end;
        if let Some((newest, body)) = hits.last() {
            let newest = *newest;
            if self.previous_summary.is_none() {
                let bodies: Vec<&str> = hits
                    .iter()
                    .map(|(_, b)| b.as_str())
                    .filter(|b| !b.is_empty())
                    .collect();
                if !bodies.is_empty() {
                    self.previous_summary = Some(bodies.join("\n\n"));
                }
            }
            if real_user {
                self.summary_has_user_turn = Some(true);
            } else if self.summary_has_user_turn.is_none() {
                self.summary_has_user_turn = Some(!body.contains(NO_USER_TASK_SENTINEL));
            }
            let window_row = |i: usize| {
                if indices.contains(&i) {
                    strip_handoff(&rows[i])
                } else {
                    Some(rows[i].clone())
                }
            };
            turns = (start..newest.max(start)).filter_map(window_row).collect();
            turns.extend(strip_handoff(&rows[newest]));
            turns.extend(rows[(newest + 1).min(end)..end].iter().cloned());
            if newest >= end {
                tail_start = newest + 1;
            }
        } else {
            self.previous_summary = None;
            self.summary_has_user_turn = Some(real_user);
        }
        SummaryWindow {
            turns,
            indices,
            tail_start,
        }
    }

    fn tail_token_budget(&self, window: Window) -> usize {
        let ratio = self.settings.target_ratio.clamp(0.10, 0.80);
        to_usize(as_f64(window.threshold_tokens) * ratio)
    }

    /// The head kept verbatim: [`Settings::protect_first_n`] messages until the session has
    /// a summary, none after.
    fn protect_head_size(&self, rows: &[Row]) -> usize {
        let first_n = self.settings.protect_first_n;
        if self.compressions >= 1 || self.previous_summary.is_some() || first_n == 0 {
            return 0;
        }
        let probe = rows
            .len()
            .min(first_n + RESTART_HANDOFF_PROBE_EXTRA_MESSAGES);
        if rows[..probe]
            .iter()
            .any(|row| text::is_summary_content(&row.text()))
        {
            return 0;
        }
        first_n
    }

    /// Replaces old tool results and shrinks old tool arguments in place, protecting a tail
    /// of `budget` tokens and at least a short run of recent messages.
    fn prune_old_tool_results(&self, rows: &mut [Row], budget: usize) {
        let n = rows.len();
        let min_protect = self
            .settings
            .protect_last_n
            .min(n)
            .min(MAX_TAIL_MESSAGE_FLOOR);
        let mut boundary = n;
        if budget > 0 {
            let mut accumulated = 0;
            for i in (0..n).rev() {
                let tokens = budget_tokens(&rows[i]);
                if accumulated + tokens > budget && n - i >= min_protect {
                    boundary = i;
                    break;
                }
                accumulated += tokens;
                boundary = i;
            }
        }
        let protected = (n - boundary).max(min_protect);
        let boundary = n - protected;

        let mut seen: HashSet<String> = HashSet::new();
        for row in rows.iter_mut().rev() {
            if row.kind != Kind::Tool {
                continue;
            }
            let content = row.text();
            if content.chars().count() < PRUNE_MIN_CHARS {
                continue;
            }
            if seen.contains(&content) {
                row.set_text(DUPLICATE_TOOL_OUTPUT.to_owned());
            } else {
                seen.insert(content);
            }
        }
        for row in &mut rows[..boundary] {
            demote_tool_result(row);
        }
        for row in &mut rows[..boundary] {
            truncate_tool_arguments(row);
        }

        if budget == 0 || rows.is_empty() {
            return;
        }
        let soft = budget * 3 / 2;
        let demote_end = n - PRESSURE_KEEP_RECENT_MESSAGES.min(n);
        let region = |rows: &[Row]| rows[boundary..].iter().map(budget_tokens).sum::<usize>();
        if demote_end <= boundary || region(rows) <= soft {
            return;
        }
        for i in boundary..demote_end {
            demote_tool_result(&mut rows[i]);
            truncate_tool_arguments(&mut rows[i]);
            if region(rows) <= soft {
                break;
            }
        }
        if region(rows) > soft {
            let last_tool = rows.iter().rposition(|row| row.kind == Kind::Tool);
            for (i, row) in rows.iter_mut().enumerate().skip(boundary) {
                if Some(i) == last_tool {
                    continue;
                }
                demote_tool_result(row);
                truncate_tool_arguments(row);
            }
            if let Some(last) = last_tool.filter(|l| *l >= boundary)
                && region(rows) > soft
            {
                demote_tool_result(&mut rows[last]);
            }
        }
    }

    /// Where the tail starts: a token budget walked back from the end with a short message
    /// floor, never inside a tool group, always holding the last user and assistant messages.
    fn tail_cut(&self, rows: &[Row], head_end: usize, budget: usize) -> usize {
        let n = rows.len();
        let available = n.saturating_sub(head_end + 1);
        let floor = self
            .settings
            .protect_last_n
            .clamp(3, MAX_TAIL_MESSAGE_FLOOR);
        let cap = available.saturating_sub(2).max(3);
        let min_tail = if available > 1 {
            floor.min(cap).min(available)
        } else {
            0
        };
        let soft = budget * 3 / 2;
        let mut accumulated = 0;
        let mut cut = n;
        for i in (head_end..n).rev() {
            let tokens = budget_tokens(&rows[i]);
            if accumulated + tokens > soft && n - i >= min_tail {
                break;
            }
            accumulated += tokens;
            cut = i;
        }
        if cut <= head_end && accumulated <= soft && accumulated > 0 {
            let mut raw = 0;
            for j in (head_end..n).rev() {
                let tokens = budget_tokens(&rows[j]);
                if raw + tokens > budget && n - j >= min_tail {
                    cut = j;
                    break;
                }
                raw += tokens;
                cut = j;
            }
        }
        let fallback = n - min_tail;
        cut = cut.min(fallback);
        if cut <= head_end {
            cut = fallback.max(head_end + 1);
        }
        cut = align_backward(rows, cut);
        cut = ensure_last_user_in_tail(rows, cut, head_end);
        cut = ensure_last_assistant_in_tail(rows, cut, head_end);
        n.min(align_forward(rows, cut.max(head_end + 1)))
    }

    /// The summary of `turns` in its handoff form, or why there is none; `None` when the
    /// interrupt fired.
    fn generate_summary(
        &mut self,
        turns: &[Row],
        focus: Option<String>,
        window: Window,
        now: i64,
        interrupt: &Interrupt,
    ) -> Option<Generated> {
        if now < self.cooldown_until {
            return Some(Generated::Failed(None));
        }
        let focus = focus.map(|f| text::redact_compaction(self.redactor, &f));
        if let Some(previous) = &self.previous_summary {
            self.previous_summary = Some(text::redact_compaction(self.redactor, previous));
        }
        let content = text::serialize_for_summary(self.redactor, turns);
        let content = text::bound_summary_input(&content);
        let has_user_turn = self
            .summary_has_user_turn
            .unwrap_or_else(|| text::has_real_user_turn(turns));
        let prompt = text::summary_prompt(&PromptInput {
            has_user_turn,
            today: &self.today(now),
            budget: summary_budget(turns, window),
            previous: self.previous_summary.as_deref(),
            content: &content,
            focus: focus.as_deref(),
        });
        let answer = match self.ask(&prompt, interrupt) {
            Ok(answer) => self.accept(&answer, turns, has_user_turn),
            Err(None) => return None,
            Err(Some(error)) => Err(error),
        };
        Some(match answer {
            Ok(summary) => {
                self.previous_summary = Some(summary.clone());
                self.cooldown_until = 0;
                Generated::Summary(text::with_summary_prefix(&summary))
            }
            Err(error) => {
                self.cooldown_until = now + SUMMARY_FAILURE_COOLDOWN_SECONDS;
                Generated::Failed(Some(error_text(&error)))
            }
        })
    }

    /// The summary body of the turn's `answer`, or why it is unusable.
    fn accept(&self, answer: &str, turns: &[Row], has_user_turn: bool) -> Result<String, String> {
        if py_strip(answer).is_empty() {
            return Err(format!(
                "Context compression LLM returned empty content (provider=qb model={})",
                self.settings.route
            ));
        }
        let stripped = agent::think::strip_blocks(answer);
        let content = match py_strip(&stripped) {
            "" => answer,
            stripped => stripped,
        };
        let summary = text::redact_compaction(self.redactor, py_strip(content));
        let summary = text::ground_task_snapshot(self.redactor, &summary, turns);
        text::check_user_provenance(&summary, has_user_turn)?;
        Ok(summary)
    }

    /// The text of the summary turn's answer to `prompt`; `Err(None)` when the interrupt fired.
    fn ask(&self, prompt: &str, interrupt: &Interrupt) -> Result<String, Option<String>> {
        let request = TurnRequest {
            route: self.settings.route.clone(),
            system: None,
            messages: vec![text_message(Role::User, prompt)],
            tools: None,
            max_tokens: self.settings.max_tokens,
            effort: None,
            session_key: None,
            cache: None,
            request_id: format!(
                "compact-{}",
                random_hex(8).map_err(|e| Some(e.to_string()))?
            ),
        };
        match agent::side_turn(&self.client, &request, self.settings.timeout, interrupt) {
            Ok(text) => Ok(text),
            Err(Ended::Interrupted) => Err(None),
            Err(ended) => Err(Some(ended.to_string())),
        }
    }

    /// `YYYY-MM-DD` of unix time `now` in the compressor's zone.
    fn today(&self, now: i64) -> String {
        let local = now + self.zone.at(now).offset;
        let (year, month, day) = civil_from_days(local.div_euclid(86_400));
        format!("{year:04}-{month:02}-{day:02}")
    }
}

/// The new live set: the head, the summary as its own message or merged into the first tail
/// message so the roles alternate, and the tail.
fn assemble(
    rows: &[Row],
    start: usize,
    end: usize,
    window: &SummaryWindow,
    mut summary_text: String,
) -> Vec<Row> {
    let mut compressed: Vec<Row> = rows[..start].iter().filter_map(strip_handoff).collect();
    let mut tail: Vec<Row> = Vec::new();
    for (i, row) in rows.iter().enumerate().skip(end.max(window.tail_start)) {
        if window.indices.contains(&i) && i >= window.tail_start {
            continue;
        }
        tail.extend(strip_handoff(row));
    }

    let last_head = if compressed.is_empty() {
        Some(Kind::User)
    } else {
        compressed.iter().rev().find_map(visible_role)
    };
    let first_tail = tail
        .iter()
        .enumerate()
        .find_map(|(i, row)| visible_role(row).map(|kind| (i, kind)));
    let mut force_user = start == 0;
    if !force_user {
        let nonempty_user = |row: &Row| row.kind == Kind::User && !py_strip(&row.text()).is_empty();
        force_user = !compressed.iter().chain(&tail).any(nonempty_user);
    }
    let mut role = match last_head {
        None | Some(Kind::Assistant) => Kind::User,
        _ if force_user => Kind::User,
        _ => Kind::Assistant,
    };
    let mut merge = false;
    if let Some((_, first)) = first_tail.filter(|(_, kind)| *kind == role) {
        let flipped = if first == Kind::User {
            Kind::Assistant
        } else {
            Kind::User
        };
        if last_head.is_some_and(|head| head != flipped) && !force_user {
            role = flipped;
        } else {
            merge = !tail.is_empty();
        }
    }
    if !merge {
        summary_text = format!("{summary_text}\n\n{SUMMARY_END_MARKER}");
        compressed.push(Row::text_row(role, summary_text.clone()));
    }
    let target = match first_tail {
        Some((i, _)) if force_user => i,
        _ => 0,
    };
    for (i, mut row) in tail.into_iter().enumerate() {
        if merge && i == target {
            if force_user && role == Kind::User {
                row.add_text(&format!("{summary_text}\n\n{SUMMARY_END_MARKER}\n\n"), true);
            } else {
                row.add_text(
                    &format!(
                        "\n\n{MERGED_SUMMARY_DELIMITER}\n\n{summary_text}\n\n{SUMMARY_END_MARKER}"
                    ),
                    false,
                );
                row.add_text(&format!("{MERGED_PRIOR_CONTEXT_HEADER}\n"), true);
            }
            merge = false;
        }
        compressed.push(row);
    }
    compressed
}

/// `error` stripped and cut to [`ERROR_TEXT_MAX_CHARS`].
fn error_text(error: &str) -> String {
    let error = py_strip(error);
    if error.chars().count() > ERROR_TEXT_MAX_CHARS {
        format!(
            "{}...",
            text::head(error, ERROR_TEXT_MAX_CHARS - 3).trim_end()
        )
    } else {
        error.to_owned()
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "token counts stay far below 2^52"
)]
fn as_f64(n: u64) -> f64 {
    n as f64
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the reference's int() of a non-negative product"
)]
fn to_usize(x: f64) -> usize {
    x as usize
}

/// Summary tokens to ask for: a share of the summarized turns between 2,000 and 5 % of the
/// context, at most 10,000.
fn summary_budget(turns: &[Row], window: Window) -> usize {
    let content: usize = turns.iter().map(rough_tokens).sum();
    let budget = to_usize(as_f64(content as u64) * SUMMARY_RATIO);
    let ceiling = to_usize(as_f64(window.context_length) * 0.05).min(SUMMARY_TOKENS_CEILING);
    budget.min(ceiling).max(MIN_SUMMARY_TOKENS)
}

/// Role of a message as the reference's flat transcript has it: tool results are their own
/// rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    User,
    Assistant,
    Tool,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::User => "USER",
            Self::Assistant => "ASSISTANT",
            Self::Tool => "TOOL",
        }
    }
}

/// One message of the flat transcript: a user or assistant message, or one tool result,
/// with the index of the C4G message it came from.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    kind: Kind,
    blocks: Vec<Block>,
    origin: usize,
}

impl Row {
    fn text_row(kind: Kind, text: String) -> Self {
        Self {
            kind,
            blocks: vec![Block {
                text: Some(text),
                ..block(BlockType::Text)
            }],
            origin: NEW_ROW,
        }
    }

    /// The content: a tool result's output, else the text blocks joined by newlines.
    fn text(&self) -> String {
        if self.kind == Kind::Tool {
            return self
                .blocks
                .first()
                .and_then(|b| b.content.clone())
                .unwrap_or_default();
        }
        let texts: Vec<&str> = self
            .blocks
            .iter()
            .filter(|b| b.r#type == BlockType::Text)
            .filter_map(|b| b.text.as_deref())
            .collect();
        match texts.as_slice() {
            [one] => (*one).to_owned(),
            many => many
                .iter()
                .filter(|t| !t.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// Replaces the content; the text blocks become one at the place of the first.
    fn set_text(&mut self, text: String) {
        if self.kind == Kind::Tool {
            if let Some(result) = self.blocks.first_mut() {
                result.content = Some(text);
            }
            return;
        }
        let at = self
            .blocks
            .iter()
            .position(|b| b.r#type == BlockType::Text)
            .unwrap_or(0);
        self.blocks.retain(|b| b.r#type != BlockType::Text);
        self.blocks.insert(
            at.min(self.blocks.len()),
            Block {
                text: Some(text),
                ..block(BlockType::Text)
            },
        );
    }

    /// Adds `text` before or after the content: inside a tool result or the one text block,
    /// else as a text block of its own.
    fn add_text(&mut self, text: &str, before: bool) {
        let texts = self
            .blocks
            .iter()
            .filter(|b| b.r#type == BlockType::Text)
            .count();
        if self.kind == Kind::Tool || texts <= 1 {
            let old = self.text();
            self.set_text(if before {
                format!("{text}{old}")
            } else {
                format!("{old}{text}")
            });
            return;
        }
        let new = Block {
            text: Some(text.to_owned()),
            ..block(BlockType::Text)
        };
        if before {
            self.blocks.insert(0, new);
        } else {
            self.blocks.push(new);
        }
    }

    fn call_id(&self) -> &str {
        match self.kind {
            Kind::Tool => self
                .blocks
                .first()
                .and_then(|b| b.id.as_deref())
                .unwrap_or(""),
            _ => "",
        }
    }

    /// `(id, name, input)` of each tool call of an assistant row.
    fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Map<String, Value>)> {
        static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
        let assistant = self.kind == Kind::Assistant;
        self.blocks
            .iter()
            .filter(move |b| assistant && b.r#type == BlockType::ToolUse)
            .map(|b| {
                (
                    b.id.as_deref().unwrap_or(""),
                    b.name.as_deref().unwrap_or("unknown"),
                    b.input
                        .as_ref()
                        .unwrap_or_else(|| EMPTY.get_or_init(Map::new)),
                )
            })
    }

    fn message(&self) -> Message {
        Message {
            role: if self.kind == Kind::Assistant {
                Role::Assistant
            } else {
                Role::User
            },
            content: self.blocks.clone(),
        }
    }
}

/// The flat transcript of `messages`: each tool result of a user message is a row before the
/// rest of that message.
fn flatten(messages: &[Message]) -> Vec<Row> {
    let mut rows = Vec::with_capacity(messages.len());
    for (origin, message) in messages.iter().enumerate() {
        if message.role == Role::Assistant {
            rows.push(Row {
                kind: Kind::Assistant,
                blocks: message.content.clone(),
                origin,
            });
            continue;
        }
        let (results, rest): (Vec<Block>, Vec<Block>) = message
            .content
            .iter()
            .cloned()
            .partition(|b| b.r#type == BlockType::ToolResult);
        let had_results = !results.is_empty();
        rows.extend(results.into_iter().map(|result| Row {
            kind: Kind::Tool,
            blocks: vec![result],
            origin,
        }));
        if !rest.is_empty() || !had_results {
            rows.push(Row {
                kind: Kind::User,
                blocks: rest,
                origin,
            });
        }
    }
    rows
}

/// The C4G messages of `rows`: adjacent tool results form one user message, which takes back
/// the rest of the message they came from.
fn unflatten(rows: Vec<Row>) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::with_capacity(rows.len());
    let mut results_of: Option<usize> = None;
    for row in rows {
        match (row.kind, results_of, messages.last_mut()) {
            (Kind::Tool, Some(_), Some(last)) => last.content.extend(row.blocks),
            (Kind::User, Some(origin), Some(last)) if origin == row.origin && origin != NEW_ROW => {
                last.content.extend(row.blocks);
                results_of = None;
            }
            (kind, _, _) => {
                results_of = (kind == Kind::Tool).then_some(row.origin);
                messages.push(row.message());
            }
        }
    }
    messages
}

/// Rough tokens of `text`: one per CJK character, one per four other characters, rounded up.
fn estimate_tokens_rough(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    if text.is_ascii() {
        return text.len().div_ceil(CHARS_PER_TOKEN);
    }
    let (mut dense, mut sparse) = (0, 0);
    for c in text.chars() {
        if matches!(c, '\u{1100}'..='\u{11ff}' | '\u{2e80}'..='\u{9fff}' | '\u{a960}'..='\u{a97f}'
            | '\u{ac00}'..='\u{d7af}' | '\u{f900}'..='\u{faff}' | '\u{ff00}'..='\u{ffef}')
        {
            dense += 1;
        } else {
            sparse += 1;
        }
    }
    dense + usize::div_ceil(sparse, CHARS_PER_TOKEN)
}

/// Tokens of a row in the tail walks: its content plus each tool call as sent.
fn budget_tokens(row: &Row) -> usize {
    let calls: usize = row
        .blocks
        .iter()
        .filter(|b| row.kind == Kind::Assistant && b.r#type == BlockType::ToolUse)
        .map(|b| estimate_tokens_rough(&serde_json::to_string(b).unwrap_or_default()))
        .sum();
    estimate_tokens_rough(&row.text()) + 10 + calls
}

/// Tokens of a row as the request carries it.
fn rough_tokens(row: &Row) -> usize {
    estimate_tokens_rough(&serde_json::to_string(&row.message()).unwrap_or_default())
}

/// Replaces a long tool result with the placeholder.
fn demote_tool_result(row: &mut Row) {
    if row.kind != Kind::Tool {
        return;
    }
    let content = row.text();
    let len = content.chars().count();
    let skip = content.is_empty()
        || content == PRUNED_TOOL_PLACEHOLDER
        || content.starts_with("[Duplicate tool output")
        || (content.starts_with('[') && content.contains(" chars)") && len < 400)
        || content.starts_with("[screenshot removed")
        || len <= PRUNE_MIN_CHARS;
    if !skip {
        row.set_text(PRUNED_TOOL_PLACEHOLDER.to_owned());
    }
}

/// Cuts every string longer than 200 characters in the arguments of a long tool call.
fn truncate_tool_arguments(row: &mut Row) {
    if row.kind != Kind::Assistant {
        return;
    }
    for b in &mut row.blocks {
        if b.r#type != BlockType::ToolUse {
            continue;
        }
        if let Some(input) = &mut b.input
            && text::arguments(input).chars().count() > TOOL_ARGS_TRUNCATE_OVER
        {
            input.values_mut().for_each(shrink);
        }
    }
}

fn shrink(value: &mut Value) {
    match value {
        Value::String(s) if s.chars().count() > TOOL_ARGS_STRING_HEAD => {
            *s = format!("{}...[truncated]", text::head(s, TOOL_ARGS_STRING_HEAD));
        }
        Value::Object(map) => map.values_mut().for_each(shrink),
        Value::Array(items) => items.iter_mut().for_each(shrink),
        _ => {}
    }
}

/// A user row with no content but blank text.
fn is_blank_user(row: &Row) -> bool {
    row.kind == Kind::User
        && !text::is_summary_content(&row.text())
        && row.blocks.iter().all(|b| {
            b.r#type == BlockType::Text && b.text.as_deref().is_none_or(|t| py_strip(t).is_empty())
        })
}

/// The newest user row at or after `head_end` that a human wrote with content.
fn last_user_index(rows: &[Row], head_end: usize) -> Option<usize> {
    (head_end..rows.len()).rev().find(|&i| {
        let row = &rows[i];
        row.kind == Kind::User
            && !text::is_summary_content(&row.text())
            && !is_blank_user(row)
            && !text::is_synthetic_user(row)
    })
}

/// The blank user rows right after `user`, when an assistant row follows them.
fn blank_echoes_after(rows: &[Row], user: Option<usize>) -> HashSet<usize> {
    let Some(user) = user else {
        return HashSet::new();
    };
    let mut i = user + 1;
    let mut echoes = HashSet::new();
    while i < rows.len() && is_blank_user(&rows[i]) {
        echoes.insert(i);
        i += 1;
    }
    if echoes.is_empty() || rows.get(i).is_none_or(|row| row.kind != Kind::Assistant) {
        return HashSet::new();
    }
    echoes
}

fn align_forward(rows: &[Row], mut i: usize) -> usize {
    while i < rows.len() && rows[i].kind == Kind::Tool {
        i += 1;
    }
    i
}

/// `i` moved before the assistant row whose tool results it would split.
fn align_backward(rows: &[Row], i: usize) -> usize {
    if i == 0 || i >= rows.len() {
        return i;
    }
    let mut check = i - 1;
    while rows[check].kind == Kind::Tool {
        if check == 0 {
            return i;
        }
        check -= 1;
    }
    if rows[check].kind == Kind::Assistant && rows[check].tool_uses().next().is_some() {
        check
    } else {
        i
    }
}

/// The index after the user row `user`, its assistant reply and that reply's tool results.
fn turn_pair_end(rows: &[Row], user: usize) -> usize {
    let mut i = user + 1;
    if rows.get(i).is_none_or(|row| row.kind != Kind::Assistant) {
        return i;
    }
    i += 1;
    while rows.get(i).is_some_and(|row| row.kind == Kind::Tool) {
        i += 1;
    }
    i
}

fn ensure_last_user_in_tail(rows: &[Row], cut: usize, head_end: usize) -> usize {
    let Some(user) = last_user_index(rows, head_end) else {
        return cut;
    };
    if user >= cut {
        return cut;
    }
    if head_end + 1 > user {
        return turn_pair_end(rows, user).max(head_end + 1);
    }
    user
}

fn ensure_last_assistant_in_tail(rows: &[Row], cut: usize, head_end: usize) -> usize {
    let mut last_any = None;
    let mut found = None;
    for i in (head_end..rows.len()).rev() {
        let row = &rows[i];
        if row.kind != Kind::Assistant || text::is_summary_content(&row.text()) {
            continue;
        }
        last_any.get_or_insert(i);
        if !py_strip(&row.text()).is_empty() {
            found = Some(i);
            break;
        }
    }
    match found.or(last_any) {
        Some(last) if last < cut => align_backward(rows, last).max(head_end + 1),
        _ => cut,
    }
}

/// The role strict templates count: tool results and assistant rows with tool calls are
/// skipped.
fn visible_role(row: &Row) -> Option<Kind> {
    match row.kind {
        Kind::Tool => None,
        Kind::Assistant if row.tool_uses().next().is_some() => None,
        kind => Some(kind),
    }
}

/// `row` without an earlier handoff summary: a merged one keeps its prior content, a
/// standalone one keeps what follows its end marker, else it is dropped.
fn strip_handoff(row: &Row) -> Option<Row> {
    let content = row.text();
    if !text::is_summary_content(&content) {
        return Some(row.clone());
    }
    let kept = if let Some((prior, _)) = content.split_once(MERGED_SUMMARY_DELIMITER) {
        let prior = py_strip(prior);
        prior
            .strip_prefix(MERGED_PRIOR_CONTEXT_HEADER)
            .map_or(prior, |rest| {
                rest.trim_start_matches(crate::tools::is_py_space)
            })
            .to_owned()
    } else {
        content
            .find(SUMMARY_END_MARKER)
            .map(|at| {
                content[at + SUMMARY_END_MARKER.len()..]
                    .trim_start_matches(crate::tools::is_py_space)
                    .to_owned()
            })
            .unwrap_or_default()
    };
    (!kept.is_empty()).then(|| {
        let mut row = row.clone();
        row.set_text(kept);
        row
    })
}

/// Drops tool results without their call and tool calls without their result, except the
/// calls of the last assistant row, which may still be running.
fn sanitize_tool_pairs(rows: &mut Vec<Row>) {
    let calls: HashSet<String> = rows
        .iter()
        .flat_map(|row| row.tool_uses().map(|(id, _, _)| id.to_owned()))
        .filter(|id| !id.is_empty())
        .collect();
    let results: HashSet<String> = rows
        .iter()
        .filter(|row| row.kind == Kind::Tool && !row.call_id().is_empty())
        .map(|row| row.call_id().to_owned())
        .collect();
    rows.retain(|row| {
        row.kind != Kind::Tool || calls.contains(row.call_id()) || row.call_id().is_empty()
    });
    let missing: HashSet<&String> = calls.difference(&results).collect();
    if missing.is_empty() {
        return;
    }
    let inflight = rows
        .iter()
        .rposition(|row| row.kind != Kind::Tool)
        .filter(|&i| rows[i].kind == Kind::Assistant);
    for (i, row) in rows.iter_mut().enumerate() {
        if Some(i) == inflight || row.tool_uses().next().is_none() {
            continue;
        }
        let before = row.blocks.len();
        row.blocks.retain(|b| {
            b.r#type != BlockType::ToolUse || !b.id.as_ref().is_some_and(|id| missing.contains(id))
        });
        if row.blocks.len() != before
            && row.tool_uses().next().is_none()
            && py_strip(&row.text()).is_empty()
        {
            row.blocks.retain(|b| b.r#type != BlockType::Text);
            row.blocks.push(Block {
                text: Some(TOOL_CALL_REMOVED.to_owned()),
                ..block(BlockType::Text)
            });
        }
    }
}
