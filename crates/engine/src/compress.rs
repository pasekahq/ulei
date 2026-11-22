// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Context compaction of a session's live set: old tool output replaced by one-line
//! descriptions, the head and a token-budget tail kept verbatim, the middle summarized by one
//! tool-less turn on the qb route alias of [`Settings::route`] (an earlier summary updated, not
//! repeated), and the result stored with [`Journal::compact`]. A turn failed on access, quota or
//! the connection leaves the session as it was; any other failure stores a deterministic summary.

pub mod text;

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};
use ulei_contracts::generated::c4g::{
    Block, BlockType, CacheMode, ErrorClass, Message, Role, TurnRequest, Usage,
};

use crate::agent::{self, Ended, block, text_message};
use crate::config::{self, Config, Setting};
use crate::gateway::{self, Client};
use crate::journal::search::MessageRole;
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
/// Share of the window at which compaction runs when none is configured.
pub const DEFAULT_THRESHOLD: f64 = 0.3;
/// Prompt tokens at which compaction runs at the latest when none is configured.
pub const DEFAULT_THRESHOLD_TOKENS: usize = 300_000;
/// Context length that sizes an overflow compaction of a route with no configured window.
pub const DEFAULT_FALLBACK_CONTEXT: u64 = 256_000;
/// Compactions one turn may run before and after its steps until a step's usage proves one
/// worked.
pub const MAX_COMPRESSION_ATTEMPTS: usize = 1;
/// Seconds the anti-thrash guard holds compaction back before one probe.
pub const ANTI_THRASH_RECOVERY_SECONDS: i64 = 300;
/// Seconds without a summary turn after the first, second and later timeouts in a row.
pub const TIMEOUT_COOLDOWN_LADDER: [i64; 3] = [60, 300, 900];
/// Seconds without a summary turn after a malformed answer or a closed stream.
pub const SHORT_COOLDOWN_SECONDS: i64 = 30;
/// Seconds without a summary turn after any other failure.
pub const TRANSIENT_COOLDOWN_SECONDS: i64 = 60;

/// Lowest threshold in tokens, unless it would not fit the window.
const MINIMUM_CONTEXT_LENGTH: u64 = 64_000;
/// Share of the input budget at which a window too small for the floor compacts.
const MIN_CTX_TRIGGER_RATIO: f64 = 0.85;
/// Windows below this many tokens compact at no less than [`SMALL_CTX_THRESHOLD_PERCENT`].
const SMALL_CTX_WINDOW_LIMIT: u64 = 512_000;
const SMALL_CTX_THRESHOLD_PERCENT: f64 = 0.75;
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
/// Error texts the reference reads as a timeout.
const TIMEOUT_MARKERS: [&str; 2] = ["timeout", "timed out"];
/// Error texts the reference reads as a connection failure.
const CONNECTION_MARKERS: [&str; 12] = [
    "connection refused",
    "name or service not known",
    "no route to host",
    "network is unreachable",
    "timed out",
    "connection reset",
    "incomplete chunked read",
    "peer closed connection",
    "response ended prematurely",
    "unexpected eof",
    "remoteprotocolerror",
    "localprotocolerror",
];
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
    /// Prompt cache mode of the summary turn; `None` sends none.
    pub cache: Option<CacheMode>,
    /// Messages kept at the start of a first compaction.
    pub protect_first_n: usize,
    /// Recent messages the tail keeps at least, up to 8.
    pub protect_last_n: usize,
    /// Share of the threshold kept as the verbatim tail.
    pub target_ratio: f64,
    /// Share of the window at which compaction runs, raised to 0.75 below 512K tokens.
    pub threshold: f64,
    /// Prompt tokens at which compaction runs at the latest; `None` sets no cap.
    pub threshold_tokens: Option<u64>,
}

impl Settings {
    /// The settings of `config`, each overridden by its environment variable as `env` reads
    /// it; `max_tokens` is the engine's output bound per model turn and `cache` its prompt
    /// cache mode.
    ///
    /// # Errors
    /// [`config::Error`] for a value that is not of its kind.
    pub fn from_settings(
        config: &Config,
        env: &dyn Fn(&str) -> Option<OsString>,
        max_tokens: u64,
        cache: Option<CacheMode>,
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
            cache,
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
            threshold: config.number(
                config::COMPRESSION_THRESHOLD,
                value(config::COMPRESSION_THRESHOLD),
                DEFAULT_THRESHOLD,
            )?,
            threshold_tokens: u64::try_from(config.count(
                config::COMPRESSION_THRESHOLD_TOKENS,
                value(config::COMPRESSION_THRESHOLD_TOKENS),
                DEFAULT_THRESHOLD_TOKENS,
            )?)
            .ok()
            .filter(|cap| *cap > 0),
        })
    }

    /// The window of a route of `context_length` tokens whose requests reserve `max_tokens`.
    pub fn window(&self, context_length: u64, max_tokens: u64) -> Window {
        Window::new(
            context_length,
            max_tokens,
            self.threshold,
            self.threshold_tokens,
        )
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

impl Window {
    /// The window of a route of `context_length` tokens whose requests reserve `max_tokens`
    /// for output: compaction runs at `threshold` of the input budget, raised to 0.75 below
    /// 512K tokens, floored at 64K unless that leaves no room, and at `cap` tokens at the latest.
    pub fn new(context_length: u64, max_tokens: u64, threshold: f64, cap: Option<u64>) -> Self {
        let percent = if context_length > 0 && context_length < SMALL_CTX_WINDOW_LIMIT {
            threshold.max(SMALL_CTX_THRESHOLD_PERCENT)
        } else {
            threshold
        };
        let mut threshold_tokens = compute_threshold_tokens(context_length, percent, max_tokens);
        if let Some(cap) = cap.filter(|c| *c > 0) {
            threshold_tokens = threshold_tokens.min(cap.min(context_length));
        }
        Self {
            context_length,
            threshold_tokens,
        }
    }
}

/// What a turn compacts with.
pub struct Compaction<'a> {
    /// The compressor bound to the turn's session.
    pub compressor: &'a mut Compressor,
    /// The window of the turn's route; `None` when unknown, which runs no compaction before a
    /// step and sizes an overflow compaction by [`DEFAULT_FALLBACK_CONTEXT`].
    pub window: Option<Window>,
    /// The unix time now.
    pub clock: &'a dyn Fn() -> i64,
}

impl Compaction<'_> {
    /// The window an overflow compaction sizes its budgets by, for requests reserving
    /// `max_tokens`.
    pub(crate) fn overflow_window(&self, max_tokens: u64) -> Window {
        self.window.unwrap_or_else(|| {
            self.compressor
                .settings
                .window(DEFAULT_FALLBACK_CONTEXT, max_tokens)
        })
    }
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
    /// The summary turn failed on access, quota or the connection, now or before the
    /// cooldown it started; the messages stay as they were.
    Aborted {
        /// The failure, `None` during the cooldown.
        reason: Option<String>,
    },
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

/// The compactions of one engine: the settings, and the state one session carries between
/// compactions.
pub struct Compressor {
    client: Client,
    settings: Settings,
    redactor: Redactor,
    zone: Zone,
    session: Option<u64>,
    state: State,
}

/// What one session carries between compactions.
#[derive(Default)]
struct State {
    previous_summary: Option<String>,
    summary_has_user_turn: Option<bool>,
    compressions: u64,
    cooldown_until: i64,
    timeouts: usize,
    access_failure: bool,
    network_failure: bool,
    reading: Reading,
    verify: bool,
    ineffective: u32,
    fallback_streak: u32,
    recovery_deadline: i64,
}

/// The prompt size the gateway last reported.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// No step reported one.
    #[default]
    Unknown,
    /// A compaction ran since; the stale reading must not trigger another.
    AfterCompaction,
    /// Prompt tokens, cached ones included.
    Tokens(u64),
}

/// The rows a summary replaces and where the tail starts.
struct SummaryWindow {
    turns: Vec<Row>,
    indices: HashSet<usize>,
    tail_start: usize,
}

/// What the reference's `_generate_summary` reads a failed summary turn as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cause {
    /// Access refused.
    Access,
    /// The quota is exhausted: an access failure, and a timeout by its 429 status.
    Quota,
    /// The vendor asked to back off, a timeout by status.
    BackOff,
    /// The deadline passed, a timeout and a connection failure.
    TimedOut,
    /// The connection failed or the stream closed early.
    Connection,
    /// The answer is not valid JSON.
    Malformed,
    /// Anything else.
    Other,
}

/// A failed summary turn and why.
struct Failure {
    text: String,
    cause: Cause,
}

impl Failure {
    /// The class of the turn's end mapped onto the reference's checks.
    fn of(ended: &Ended) -> Self {
        let cause = match ended {
            Ended::Gateway(error) => match error.class {
                ErrorClass::Auth => Cause::Access,
                ErrorClass::QuotaExhausted => Cause::Quota,
                ErrorClass::Overloaded => Cause::BackOff,
                ErrorClass::Unavailable
                | ErrorClass::BadRequest
                | ErrorClass::ContextTooLong
                | ErrorClass::Cancelled
                | ErrorClass::NoRoute => Cause::Other,
            },
            Ended::Transport(gateway::Error::Timeout) => Cause::TimedOut,
            Ended::Transport(gateway::Error::Io(_) | gateway::Error::Cut) => Cause::Connection,
            Ended::Transport(gateway::Error::Malformed(_)) => Cause::Malformed,
            _ => Cause::Other,
        };
        Self {
            text: ended.to_string(),
            cause,
        }
    }

    fn other(text: String) -> Self {
        Self {
            text,
            cause: Cause::Other,
        }
    }

    fn marked(&self, markers: &[&str]) -> bool {
        let lower = self.text.to_lowercase();
        markers.iter().any(|m| lower.contains(m))
    }

    fn timeout(&self) -> bool {
        matches!(self.cause, Cause::Quota | Cause::BackOff | Cause::TimedOut)
            || self.marked(&TIMEOUT_MARKERS)
    }

    fn short(&self) -> bool {
        matches!(self.cause, Cause::Connection | Cause::Malformed)
    }

    fn network(&self) -> bool {
        matches!(self.cause, Cause::TimedOut | Cause::Connection)
            || self.marked(&CONNECTION_MARKERS)
    }
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
            session: None,
            state: State::default(),
        }
    }

    /// Makes `session` the one the state belongs to; a switch drops the state of the previous
    /// session.
    pub fn bind(&mut self, session: u64) {
        if self.session != Some(session) {
            self.session = Some(session);
            self.state = State::default();
        }
    }

    /// Takes the usage of a step that answered: its prompt size, and whether the compaction
    /// before it brought the prompt under `window`'s threshold. Returns the prompt size when
    /// it is the first reading after a completed compaction.
    pub fn observe(&mut self, usage: &Usage, window: Option<Window>) -> Option<u64> {
        let tokens = usage.input + usage.cache_read + usage.cache_write;
        let state = &mut self.state;
        let after_compaction = state.verify.then_some(tokens);
        state.reading = Reading::Tokens(tokens);
        if let Some(window) = window.filter(|_| tokens > 0) {
            let fits = tokens < window.threshold_tokens;
            if fits || state.verify {
                state.ineffective = if fits { 0 } else { state.ineffective + 1 };
            }
        }
        state.verify = false;
        after_compaction
    }

    /// Whether the prompt of `request` reached `window`'s threshold at unix time `now` and no
    /// cooldown or anti-thrash guard holds compaction back. The prompt size is the last
    /// reading; without one a rough count of `request`, and none right after a compaction.
    pub fn should_compress(&mut self, request: &TurnRequest, window: Window, now: i64) -> bool {
        let tokens = match self.state.reading {
            Reading::Tokens(tokens) if tokens > 0 => tokens,
            Reading::AfterCompaction => 0,
            _ => request_tokens_rough(request),
        };
        tokens >= window.threshold_tokens && !self.blocked(now)
    }

    /// The cooldown after a failed summary turn, or two ineffective or fallback compactions in
    /// a row until [`ANTI_THRASH_RECOVERY_SECONDS`] pass and one probe is let through.
    fn blocked(&mut self, now: i64) -> bool {
        let state = &mut self.state;
        if now < state.cooldown_until {
            return true;
        }
        if state.ineffective < 2 && state.fallback_streak < 2 {
            state.recovery_deadline = 0;
            return false;
        }
        if state.recovery_deadline == 0 {
            state.recovery_deadline = now + ANTI_THRASH_RECOVERY_SECONDS;
            return true;
        }
        if now < state.recovery_deadline {
            return true;
        }
        state.recovery_deadline = 0;
        state.ineffective = state.ineffective.min(1);
        state.fallback_streak = state.fallback_streak.min(1);
        false
    }

    /// Compacts the live set of `session` at unix time `now` with that session's state and
    /// stores a compacted result as the new live set.
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
        self.bind(session);
        let compression = self
            .compress(&live, window, now, interrupt)
            .ok_or(Error::Interrupted)?;
        if let Compression::Compacted { messages, .. } = &compression {
            journal.compact(session, messages, now)?;
        }
        Ok(compression)
    }

    /// The compacted form of `messages` at unix time `now`; `None` when the interrupt fired
    /// during the summary turn.
    pub fn compress(
        &mut self,
        messages: &[Message],
        window: Window,
        now: i64,
        interrupt: &Interrupt,
    ) -> Option<Compression> {
        let mut rows = flatten(messages);
        if rows.len() <= self.protect_head_size(&rows) + 3 + 1 {
            return Some(Compression::Skipped(Skip::TooFewMessages));
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
            return Some(Compression::Skipped(Skip::NoWindow));
        }

        let before = (
            self.state.previous_summary.clone(),
            self.state.summary_has_user_turn,
        );
        let window_rows = self.summary_window(&rows, start, end);
        if window_rows.turns.is_empty() {
            return Some(Compression::Skipped(Skip::NothingNew));
        }
        let turns = &window_rows.turns;

        let focus = text::auto_focus_topic(self.redactor, &rows);
        let Some(generated) = self.generate_summary(turns, focus, window, now, interrupt) else {
            (
                self.state.previous_summary,
                self.state.summary_has_user_turn,
            ) = before;
            return None;
        };
        if let Generated::Failed(reason) = &generated
            && (self.state.access_failure || self.state.network_failure)
        {
            self.state.previous_summary = before.0;
            return Some(Compression::Aborted {
                reason: reason.clone(),
            });
        }

        let (summary_text, summary) = match generated {
            Generated::Summary(text) => (text, Summary::Model),
            Generated::Failed(reason) => (
                text::static_fallback_summary(
                    self.redactor,
                    turns,
                    self.state.previous_summary.as_deref(),
                    reason.as_deref(),
                ),
                Summary::Fallback { reason },
            ),
        };
        let mut compressed = assemble(&rows, start, end, &window_rows, summary_text);
        self.state.compressions += 1;
        self.state.reading = Reading::AfterCompaction;
        self.state.verify = true;
        self.state.fallback_streak = match summary {
            Summary::Model => 0,
            Summary::Fallback { .. } => self.state.fallback_streak + 1,
        };
        sanitize_tool_pairs(&mut compressed);
        Some(Compression::Compacted {
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
            if self.state.previous_summary.is_none() {
                let bodies: Vec<&str> = hits
                    .iter()
                    .map(|(_, b)| b.as_str())
                    .filter(|b| !b.is_empty())
                    .collect();
                if !bodies.is_empty() {
                    self.state.previous_summary = Some(bodies.join("\n\n"));
                }
            }
            if real_user {
                self.state.summary_has_user_turn = Some(true);
            } else if self.state.summary_has_user_turn.is_none() {
                self.state.summary_has_user_turn = Some(!body.contains(NO_USER_TASK_SENTINEL));
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
            self.state.previous_summary = None;
            self.state.summary_has_user_turn = Some(real_user);
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
        if self.state.compressions >= 1 || self.state.previous_summary.is_some() || first_n == 0 {
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
        let calls: Calls = rows
            .iter()
            .flat_map(|row| {
                row.tool_uses()
                    .map(|(id, name, input)| (id.to_owned(), (name.to_owned(), input.clone())))
            })
            .collect();

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
            demote_tool_result(row, &calls);
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
            demote_tool_result(&mut rows[i], &calls);
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
                demote_tool_result(row, &calls);
                truncate_tool_arguments(row);
            }
            if let Some(last) = last_tool.filter(|l| *l >= boundary)
                && region(rows) > soft
            {
                demote_tool_result(&mut rows[last], &calls);
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
        if now < self.state.cooldown_until {
            return Some(Generated::Failed(None));
        }
        let focus = focus.map(|f| text::redact_compaction(self.redactor, &f));
        if let Some(previous) = &self.state.previous_summary {
            self.state.previous_summary = Some(text::redact_compaction(self.redactor, previous));
        }
        let content = text::serialize_for_summary(self.redactor, turns);
        let content = text::bound_summary_input(&content);
        let has_user_turn = self
            .state
            .summary_has_user_turn
            .unwrap_or_else(|| text::has_real_user_turn(turns));
        let prompt = text::summary_prompt(&PromptInput {
            has_user_turn,
            today: &self.today(now),
            budget: summary_budget(turns, window),
            previous: self.state.previous_summary.as_deref(),
            content: &content,
            focus: focus.as_deref(),
        });
        let answer = match self.ask(&prompt, interrupt) {
            Ok(answer) => self
                .accept(&answer, turns, has_user_turn)
                .map_err(Failure::other),
            Err(None) => return None,
            Err(Some(failure)) => Err(failure),
        };
        Some(match answer {
            Ok(summary) => {
                self.state.previous_summary = Some(summary.clone());
                self.state.cooldown_until = 0;
                self.state.timeouts = 0;
                self.state.access_failure = false;
                self.state.network_failure = false;
                Generated::Summary(text::with_summary_prefix(&summary))
            }
            Err(failure) => Generated::Failed(Some(self.record_failure(&failure, now))),
        })
    }

    /// Starts the cooldown of `failure` and keeps its access and network flags; the text the
    /// fallback summary names.
    fn record_failure(&mut self, failure: &Failure, now: i64) -> String {
        if matches!(failure.cause, Cause::Access | Cause::Quota) {
            self.state.access_failure = true;
        }
        let seconds = if failure.timeout() {
            self.state.timeouts += 1;
            TIMEOUT_COOLDOWN_LADDER[self.state.timeouts.min(TIMEOUT_COOLDOWN_LADDER.len()) - 1]
        } else if failure.short() {
            SHORT_COOLDOWN_SECONDS
        } else {
            TRANSIENT_COOLDOWN_SECONDS
        };
        self.state.cooldown_until = now + seconds;
        if failure.network() {
            self.state.network_failure = true;
        }
        error_text(&failure.text)
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
    fn ask(&self, prompt: &str, interrupt: &Interrupt) -> Result<String, Option<Failure>> {
        let request = TurnRequest {
            route: self.settings.route.clone(),
            system: None,
            messages: vec![text_message(Role::User, prompt)],
            tools: None,
            max_tokens: self.settings.max_tokens,
            effort: None,
            session_key: None,
            cache: self.settings.cache,
            request_id: format!(
                "compact-{}",
                random_hex(8).map_err(|e| Some(Failure::other(e.to_string())))?
            ),
        };
        match agent::side_turn(&self.client, &request, self.settings.timeout, interrupt) {
            Ok(text) => Ok(text),
            Err(Ended::Interrupted) => Err(None),
            Err(ended) => Err(Some(Failure::of(&ended))),
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

/// Prompt tokens at which compaction runs: `percent` of the input budget left after
/// `max_tokens`, at least [`MINIMUM_CONTEXT_LENGTH`] unless that reaches the budget, which then
/// compacts at [`MIN_CTX_TRIGGER_RATIO`] of it.
fn compute_threshold_tokens(context_length: u64, percent: f64, max_tokens: u64) -> u64 {
    let effective = match context_length.checked_sub(max_tokens) {
        Some(left) if left > 0 => left,
        _ => context_length,
    };
    let floored = to_u64(as_f64(effective) * percent).max(MINIMUM_CONTEXT_LENGTH);
    if effective > 0 && floored >= effective {
        return to_u64(as_f64(effective) * MIN_CTX_TRIGGER_RATIO)
            .min(effective - 1)
            .max(1);
    }
    floored
}

/// Rough tokens of a whole request: the system prompt, the messages and the tool schemas.
pub fn request_tokens_rough(request: &TurnRequest) -> u64 {
    let system = request.system.as_deref().map_or(0, estimate_tokens_rough);
    let tools = request.tools.as_ref().map_or(0, |tools| {
        estimate_tokens_rough(&serde_json::to_string(tools).unwrap_or_default())
    });
    u64::try_from(system + messages_tokens_rough(&request.messages) + tools).unwrap_or(u64::MAX)
}

/// Rough tokens of `messages` as a request carries them.
pub fn messages_tokens_rough(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| estimate_tokens_rough(&serde_json::to_string(m).unwrap_or_default()))
        .sum()
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the reference's int() of a non-negative product"
)]
fn to_u64(x: f64) -> u64 {
    x as u64
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

/// Whether the summary recognizer accepts `message`: one of its rows is a standalone or merged
/// handoff summary.
pub(crate) fn is_summary(message: &Message) -> bool {
    flatten(std::slice::from_ref(message))
        .iter()
        .any(|row| text::is_summary_content(&row.text()))
}

/// The text the history index holds for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexText {
    /// The reference's role: `tool` for a user message that carries a tool result.
    pub role: MessageRole,
    /// The content of its flat transcript rows, tool outputs first, joined by newlines.
    pub content: String,
    /// The names of the calls its tool results answer, space separated.
    pub tool_name: Option<String>,
    /// Its tool calls as the JSON array of their C4G `tool_use` blocks.
    pub tool_calls: Option<String>,
}

/// The tool name of each tool call id of the assistant messages among `messages`.
pub(crate) fn tool_names<'a>(
    messages: impl IntoIterator<Item = &'a Message>,
) -> HashMap<&'a str, &'a str> {
    messages
        .into_iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| &m.content)
        .filter(|b| b.r#type == BlockType::ToolUse)
        .filter_map(|b| Some((b.id.as_deref()?, b.name.as_deref()?)))
        .collect()
}

/// The history index text of `message`, naming its tool results by the calls in `names`.
///
/// # Errors
/// A tool call that does not serialize.
pub(crate) fn index_text(
    message: &Message,
    names: &HashMap<&str, &str>,
) -> Result<IndexText, serde_json::Error> {
    let rows = flatten(std::slice::from_ref(message));
    let role = match message.role {
        Role::Assistant => MessageRole::Assistant,
        Role::User if rows.iter().any(|r| r.kind == Kind::Tool) => MessageRole::Tool,
        Role::User => MessageRole::User,
        Role::System => MessageRole::System,
    };
    let content = rows
        .iter()
        .map(Row::text)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let called: Vec<&str> = rows
        .iter()
        .filter(|r| r.kind == Kind::Tool)
        .filter_map(|r| names.get(r.call_id()).copied())
        .collect();
    let calls: Vec<&Block> = message
        .content
        .iter()
        .filter(|b| message.role == Role::Assistant && b.r#type == BlockType::ToolUse)
        .collect();
    Ok(IndexText {
        role,
        content,
        tool_name: (!called.is_empty()).then(|| called.join(" ")),
        tool_calls: if calls.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&calls)?)
        },
    })
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
pub fn estimate_tokens_rough(text: &str) -> usize {
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

/// Name and arguments of each tool call, by call id.
type Calls = HashMap<String, (String, Map<String, Value>)>;

/// Replaces a long tool result with the one-line description of its call.
fn demote_tool_result(row: &mut Row, calls: &Calls) {
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
    if skip {
        return;
    }
    let empty = Map::new();
    let (name, args) = calls
        .get(row.call_id())
        .map_or(("unknown", &empty), |(name, args)| (name.as_str(), args));
    row.set_text(text::summarize_tool_result(name, args, &content));
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
