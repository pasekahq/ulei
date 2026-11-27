//! One night run (ADR-0078 sections 2 to 6): stage b clears the forget queue, stage a sorts the
//! seedbed, stage c sorts the history of the configured archive sources into the garden and its
//! session summaries into the forest (ADR-0079), all within the spend limit, and stage d judges
//! the new facts the garden hinted within a spend limit of its own (ADR-0096); or, with
//! `--dry-run`, the same walk read only with estimates.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use serde_json::{Value, json};

use crate::config::{self, Setting};
use crate::night::client::{self, Archive, Garden};
use crate::night::extract::{
    self, CallError, Candidate, HISTORY_SYSTEM, History, MAX_FACT_CHARS, Model, Reply, SEED_SYSTEM,
};
use crate::night::judge::{self, JUDGE_SYSTEM, PROMPT_VERSION};
use crate::night::render::{self, Part};
use crate::night::spend::{Budget, estimate_output, estimate_tokens};
use crate::night::state::{self, Cursor, Report, Source};
use crate::night::{MAX_JUDGE_BATCH, MAX_SEED_BATCH, STATE, Settings};
use crate::wire::c4g::StopReason;
use crate::wire::c7::{
    ForgetDoneRequest, ForgetTakeRequest, JudgeStats, ProposalStatus, ProposeRequest,
    ProposeResponse, ReconcileApplied, ReconcileDoneRequest, ReconcileItem, ReconcileJudged,
    ReconcileTakeRequest, SeedRecord, SeedbedDoneItem, SeedbedDoneRequest, SeedbedTakeRequest,
    SourceKind,
};
use crate::wire::c10::{ForestLink, ForestPutRequest, ListRequest, Listed, Producer};
use crate::zone;

/// Most entries of one window of a history walk.
pub const WINDOW: usize = 1_000;
/// Entries per `list` page.
const LIST_PAGE: u64 = 500;
/// The `why` of an answer that hit `max_tokens`.
const MAX_TOKENS: &str = "the answer hit max_tokens";
/// Rows per forget page.
const FORGET_PAGE: u64 = 100;
/// Runs a session may fail in before the cursor passes it.
pub const GIVE_UP_AFTER: u32 = 3;

/// A stage of the night.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// `a`: the seedbed.
    Seedbed,
    /// `b`: the forget queue.
    Forget,
    /// `c`: the archive history.
    History,
    /// `d`: the judge of the new facts the garden hinted (ADR-0096).
    Judge,
}

impl Stage {
    /// The stage of its letter.
    #[must_use]
    pub fn parse(letter: &str) -> Option<Self> {
        match letter {
            "a" => Some(Self::Seedbed),
            "b" => Some(Self::Forget),
            "c" => Some(Self::History),
            "d" => Some(Self::Judge),
            _ => None,
        }
    }

    /// Its letter.
    #[must_use]
    pub fn letter(self) -> &'static str {
        match self {
            Self::Seedbed => "a",
            Self::Forget => "b",
            Self::History => "c",
            Self::Judge => "d",
        }
    }
}

/// What a run does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Call no model and write nothing; print the estimates.
    pub dry_run: bool,
    /// The stages to run; they run in the order b, a, c, d whatever the order given.
    pub stages: Vec<Stage>,
    /// Most sessions or parts stage c sends.
    pub limit_sessions: Option<u64>,
}

/// Why a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Every selected stage ran to its end.
    Done,
    /// Every selected stage ran to its end, but sessions or parts of the history failed and wait
    /// for a later run (`waiting` in the totals).
    DoneWithFailures,
    /// The spend reached the limit, or the next call would pass it.
    SpendLimit,
    /// Stage c sent `--limit-sessions` sessions.
    SessionLimit,
    /// A daemon, the gateway or the state directory failed.
    Failed(String),
}

impl Stop {
    /// The word of the summary line and the report.
    #[must_use]
    pub fn word(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::DoneWithFailures => "done_with_failures",
            Self::SpendLimit => "spend_limit",
            Self::SessionLimit => "session_limit",
            Self::Failed(_) => "failed",
        }
    }
}

/// The counts of a run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    /// Seedbed records taken into a batch or passed straight to the edge.
    pub taken: u64,
    /// Candidates handed to the garden.
    pub proposed: u64,
    /// Candidates the edge accepted.
    pub accepted: u64,
    /// Candidates the edge queued into the digest.
    pub digest: u64,
    /// Candidates the garden refused.
    pub refused: u64,
    /// Candidates the garden already held.
    pub duplicate: u64,
    /// Calls whose answer was malformed twice, or did not fit the model.
    pub malformed: u64,
    /// Seedbed records left unprocessed for the next night.
    pub left: u64,
    /// Seedbed batches split in two after their answer hit `max_tokens`.
    pub split: u64,
    /// Forget queue rows taken.
    pub forget_rows: u64,
    /// Facts the forget queue closed.
    pub cleared: u64,
    /// Sessions or parts sent.
    pub sessions: u64,
    /// Sessions or parts given up.
    pub given_up: u64,
    /// Sessions or parts of the history that failed and wait for a later run, as the run ends.
    pub waiting: u64,
    /// History messages sent.
    pub messages: u64,
    /// Model calls.
    pub calls: u64,
    /// Input tokens, cached ones included.
    pub tokens_in: u64,
    /// Output tokens.
    pub tokens_out: u64,
    /// USD spent.
    pub usd: f64,
    /// New facts the judge's verdicts were applied to.
    pub judged: u64,
    /// Of them, merged into an earlier fact.
    pub merged: u64,
    /// Earlier facts a new one superseded.
    pub superseded: u64,
    /// Contradictions recorded for the digest.
    pub contradictions: u64,
    /// Verdicts on facts no longer current.
    pub stale: u64,
    /// Judge calls whose answer was malformed twice, refused or too long; their facts wait.
    pub judge_failures: u64,
    /// Judge calls that timed out.
    pub judge_timeouts: u64,
    /// Of `usd`, what the judge spent.
    pub judge_usd: f64,
}

impl Totals {
    fn count(&mut self, outcome: &ProposeResponse) {
        match outcome.status {
            ProposalStatus::Accepted => self.accepted += 1,
            ProposalStatus::Queued => self.digest += 1,
            ProposalStatus::Rejected => self.refused += 1,
            ProposalStatus::Duplicate => self.duplicate += 1,
            ProposalStatus::Seeded => {}
        }
    }

    /// The one-line summary on stderr.
    #[must_use]
    pub fn line(&self, stop: &Stop, seconds: f64) -> String {
        let mut line = format!(
            "ulei-engine night: {} taken {} proposed {} accepted {} digest {} refused {} malformed {} cleared {} sessions {} waiting {} judged {} tokens {}/{} usd {:.4} in {seconds:.0} s",
            stop.word(),
            self.taken,
            self.proposed,
            self.accepted,
            self.digest,
            self.refused,
            self.malformed,
            self.cleared,
            self.sessions,
            self.waiting,
            self.judged,
            self.tokens_in,
            self.tokens_out,
            self.usd,
        );
        if let Stop::Failed(why) = stop {
            line.push_str(": ");
            line.push_str(why);
        }
        line
    }

    fn json(&self, stop: &Stop, seconds: f64) -> Value {
        json!({
            "type": "total",
            "stop": stop.word(),
            "error": match stop { Stop::Failed(why) => Some(why.as_str()), _ => None },
            "taken": self.taken,
            "proposed": self.proposed,
            "accepted": self.accepted,
            "digest": self.digest,
            "refused": self.refused,
            "duplicate": self.duplicate,
            "malformed": self.malformed,
            "left": self.left,
            "split": self.split,
            "forget_rows": self.forget_rows,
            "cleared": self.cleared,
            "sessions": self.sessions,
            "given_up": self.given_up,
            "waiting": self.waiting,
            "messages": self.messages,
            "calls": self.calls,
            "tokens_in": self.tokens_in,
            "tokens_out": self.tokens_out,
            "usd": self.usd,
            "judged": self.judged,
            "merged": self.merged,
            "superseded": self.superseded,
            "contradictions": self.contradictions,
            "stale": self.stale,
            "judge_failures": self.judge_failures,
            "judge_timeouts": self.judge_timeouts,
            "judge_usd": self.judge_usd,
            "seconds": seconds,
        })
    }
}

/// The estimates of one stage of a dry run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Estimate {
    /// Seedbed records, forget rows or history entries walked.
    pub items: u64,
    /// Sessions or parts.
    pub sessions: u64,
    /// Of `sessions`, those that failed in an earlier run and are tried again (stage c).
    pub waiting: u64,
    /// History messages.
    pub messages: u64,
    /// Model calls.
    pub calls: u64,
    /// Estimated input tokens.
    pub tokens_in: u64,
    /// Estimated output tokens.
    pub tokens_out: u64,
    /// Estimated USD.
    pub usd: f64,
}

/// A failure that ends the run.
type Fail = String;

/// The judge's own counts since the last `reconcile_done`, for the garden's local counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct JudgeCounts {
    calls: u64,
    failures: u64,
    timeouts: u64,
    tokens_in: u64,
    tokens_out: u64,
}

impl JudgeCounts {
    /// The counts as C7 sends them; `None` when there is nothing to send.
    fn wire(self) -> Option<JudgeStats> {
        let some = |n: u64| (n > 0).then_some(n);
        (self != Self::default()).then(|| JudgeStats {
            calls: some(self.calls),
            failures: some(self.failures),
            timeouts: some(self.timeouts),
            tokens_in: some(self.tokens_in),
            tokens_out: some(self.tokens_out),
        })
    }
}

/// Tokens in, tokens out and USD of the attempts of one extraction.
type Usage = (u64, u64, f64);

/// The outcome of one extraction with its retry.
enum Extracted<T> {
    Answer(T),
    /// Malformed twice, or too long for the model.
    Failed(&'static str),
    /// The budget does not allow the call.
    OverBudget,
}

/// One run's state.
struct Night<'a> {
    settings: &'a Settings,
    options: &'a Options,
    model: Option<&'a mut dyn Model>,
    budget: Budget,
    /// The judge's own spend limit (ADR-0096 section 3).
    judge_budget: Budget,
    /// Stage d runs: calls spend from `judge_budget` and count into `judge_stats`.
    judging: bool,
    /// The judge's counts not yet handed to the garden.
    judge_stats: JudgeCounts,
    /// The vendor model that last answered the judge, from the gateway's route event.
    judge_model: Option<String>,
    totals: Totals,
    report: Option<Report>,
    state: Option<PathBuf>,
    started: i64,
    /// Sessions and parts stage c has tried in this run, for `--limit-sessions`.
    tried: u64,
    out: &'a mut dyn Write,
}

fn need(path: Option<&PathBuf>, setting: Setting) -> Result<PathBuf, Fail> {
    path.cloned()
        .ok_or_else(|| config::Error::Missing(setting).to_string())
}

fn daemon(e: &client::Error) -> Fail {
    e.to_string()
}

/// Runs the night under `settings`: `model` answers the extraction calls (unused and may be
/// `None` in a dry run); a dry run prints its estimates to `out`. Returns why it stopped, the
/// counts and the report's file.
pub fn run<'a>(
    settings: &'a Settings,
    options: &'a Options,
    model: Option<&'a mut dyn Model>,
    out: &'a mut dyn Write,
) -> (Stop, Totals, Option<PathBuf>) {
    let clock = Instant::now();
    let mut night = Night {
        settings,
        options,
        model,
        budget: Budget::new(settings.max_usd),
        judge_budget: Budget::new(settings.judge_max_usd),
        judging: false,
        judge_stats: JudgeCounts::default(),
        judge_model: None,
        totals: Totals::default(),
        report: None,
        state: settings.state.clone(),
        started: crate::journal::unix_now(),
        tried: 0,
        out,
    };
    let stop = night.stages();
    let seconds = clock.elapsed().as_secs_f64();
    if let Some(report) = night.report.as_mut() {
        let _ = report.line(&night.totals.json(&stop, seconds));
    }
    let path = night.report.as_ref().map(|r| r.path().to_owned());
    (stop, night.totals, path)
}

impl Night<'_> {
    fn stages(&mut self) -> Stop {
        if !self.options.dry_run {
            let dir = match need(self.state.as_ref(), STATE) {
                Ok(dir) => dir,
                Err(e) => return Stop::Failed(e),
            };
            let report = state::prepare(&dir)
                .and_then(|()| Report::create(&dir, self.started, std::process::id()));
            match report {
                Ok(report) => self.report = Some(report),
                Err(e) => return Stop::Failed(format!("{}: {e}", dir.display())),
            }
        }
        let mut failures = false;
        // A limit of stage a or c ends the extraction, not the judge, which spends from a
        // budget of its own: a history import cannot starve it (ADR-0096 section 3).
        let mut limited = None;
        for stage in [Stage::Forget, Stage::Seedbed, Stage::History, Stage::Judge] {
            if !self.options.stages.contains(&stage) || (limited.is_some() && stage != Stage::Judge)
            {
                continue;
            }
            let result = match (stage, self.options.dry_run) {
                (Stage::Forget, false) => self.forget(),
                (Stage::Forget, true) => self.dry_forget(),
                (Stage::Seedbed, false) => self.seedbed(),
                (Stage::Seedbed, true) => self.dry_seedbed(),
                (Stage::History, false) => self.history(),
                (Stage::History, true) => self.dry_history(),
                (Stage::Judge, false) => self.judge(),
                (Stage::Judge, true) => self.dry_judge(),
            };
            match result {
                Ok(Stop::Done) => {}
                Ok(Stop::DoneWithFailures) => failures = true,
                Ok(stop) => {
                    if limited.is_none() {
                        limited = Some(stop);
                    }
                }
                Err(e) => return Stop::Failed(e),
            }
        }
        if let Some(stop) = limited {
            stop
        } else if failures {
            Stop::DoneWithFailures
        } else {
            Stop::Done
        }
    }

    fn line(&mut self, value: &Value) -> Result<(), Fail> {
        match self.report.as_mut() {
            Some(report) => report
                .line(value)
                .map_err(|e| format!("{}: {e}", report.path().display())),
            None => Ok(()),
        }
    }

    fn garden(&self) -> Result<Garden, Fail> {
        need(self.settings.garden.as_ref(), config::GARDEN_SOCKET).map(Garden::new)
    }

    fn archive(&self) -> Result<Archive, Fail> {
        need(self.settings.archive.as_ref(), config::MEMORY_SOCKET).map(Archive::new)
    }

    fn print(&mut self, stage: Stage, estimate: &Estimate) -> Result<(), Fail> {
        let mut line = json!({
            "stage": stage.letter(),
            "items": estimate.items,
            "sessions": estimate.sessions,
            "messages": estimate.messages,
            "calls": estimate.calls,
            "tokens_in": estimate.tokens_in,
            "tokens_out": estimate.tokens_out,
            "usd": estimate.usd,
        });
        if stage == Stage::History {
            line["waiting"] = estimate.waiting.into();
        }
        writeln!(self.out, "{line}").map_err(|e| format!("stdout: {e}"))
    }

    /// Adds one call of `prompt` under `system` to `estimate`.
    fn estimate_call(&self, estimate: &mut Estimate, system: &str, prompt: &str) {
        let input = estimate_tokens(system) + estimate_tokens(prompt);
        let output = estimate_output(input, self.settings.max_tokens);
        estimate.calls += 1;
        estimate.tokens_in += input;
        estimate.tokens_out += output;
        let prices = self.settings.prices;
        estimate.usd += prices.input_cost(input)
            + prices.cost(&crate::wire::c4g::Usage {
                input: 0,
                output,
                cache_read: 0,
                cache_write: 0,
            });
    }

    // Stage b.

    fn forget(&mut self) -> Result<Stop, Fail> {
        let garden = self.garden()?;
        let mut after = None;
        loop {
            let rows = garden
                .forget_take(&ForgetTakeRequest {
                    limit: Some(FORGET_PAGE),
                    after,
                })
                .map_err(|e| daemon(&e))?
                .rows;
            let Some(last) = rows.last() else {
                return Ok(Stop::Done);
            };
            after = Some(last.id);
            let ids: Vec<u64> = rows.iter().map(|r| r.id).collect();
            let done = garden
                .forget_done(&ForgetDoneRequest { ids })
                .map_err(|e| daemon(&e))?;
            let closed = done.items.iter().filter(|i| i.closed).count() as u64;
            self.totals.forget_rows += rows.len() as u64;
            self.totals.cleared += closed;
            self.line(&json!({"type": "forget", "rows": rows.len(), "closed": closed}))?;
        }
    }

    fn dry_forget(&mut self) -> Result<Stop, Fail> {
        let garden = self.garden()?;
        let mut estimate = Estimate::default();
        let mut after = None;
        loop {
            let rows = garden
                .forget_take(&ForgetTakeRequest {
                    limit: Some(FORGET_PAGE),
                    after,
                })
                .map_err(|e| daemon(&e))?
                .rows;
            let Some(last) = rows.last() else { break };
            after = Some(last.id);
            estimate.items += rows.len() as u64;
        }
        self.print(Stage::Forget, &estimate)?;
        Ok(Stop::Done)
    }

    // Stage a.

    /// The batches of the records of `page` that go to the model, by `seed_batch` and the prompt
    /// budget, and the records that go to the edge as they are: a secret pointer or a pin.
    fn batches(&self, page: Vec<SeedRecord>) -> (Vec<Vec<SeedRecord>>, Vec<SeedRecord>) {
        let budget = self
            .settings
            .max_prompt_tokens
            .saturating_sub(estimate_tokens(SEED_SYSTEM));
        let (mut batches, mut straight) = (vec![Vec::new()], Vec::new());
        let mut used = 0;
        for record in page {
            if record.candidate.secret_ref.is_some() || record.candidate.pin.is_some() {
                straight.push(record);
                continue;
            }
            let cost = estimate_tokens(&extract::seed_line(&record));
            let current = batches.last().map_or(0, Vec::len);
            if current > 0 && (current >= self.settings.seed_batch || used + cost > budget) {
                batches.push(Vec::new());
                used = 0;
            }
            used += cost;
            if let Some(batch) = batches.last_mut() {
                batch.push(record);
            }
        }
        batches.retain(|b| !b.is_empty());
        (batches, straight)
    }

    fn take(&self, garden: &Garden, after: Option<String>) -> Result<Vec<SeedRecord>, Fail> {
        #[allow(clippy::cast_precision_loss)]
        let until = Some(self.started as f64);
        garden
            .seedbed_take(&SeedbedTakeRequest {
                limit: Some(MAX_SEED_BATCH as u64),
                until,
                after,
            })
            .map(|r| r.records)
            .map_err(|e| daemon(&e))
    }

    fn seedbed(&mut self) -> Result<Stop, Fail> {
        let garden = self.garden()?;
        let mut after = None;
        loop {
            if self.budget.exhausted() {
                return Ok(Stop::SpendLimit);
            }
            let page = self.take(&garden, after.clone())?;
            let Some(last) = page.last() else {
                return Ok(Stop::Done);
            };
            after = Some(last.r#ref.clone());
            let (batches, straight) = self.batches(page);
            if !straight.is_empty() {
                self.totals.taken += straight.len() as u64;
                let items: Vec<SeedbedDoneItem> = straight
                    .iter()
                    .map(|r| SeedbedDoneItem {
                        r#ref: r.r#ref.clone(),
                        candidates: None,
                    })
                    .collect();
                let stats = self.done(&garden, &items)?;
                self.line(&json!({"type": "seedbed", "records": straight.len(),
                    "outcome": "as_written", "outcomes": stats}))?;
            }
            for batch in batches {
                if let Some(stop) = self.seed_batch(&garden, &batch)? {
                    return Ok(stop);
                }
            }
        }
    }

    /// Sends one batch; `Some` stops the run. An answer that hits `max_tokens` is not asked
    /// again: the batch is split in two halves sent in turn, down to one record, which is left.
    fn seed_batch(&mut self, garden: &Garden, batch: &[SeedRecord]) -> Result<Option<Stop>, Fail> {
        let prompt: String = batch.iter().map(extract::seed_line).collect();
        let refs: Vec<String> = batch.iter().map(|r| r.r#ref.clone()).collect();
        let (answer, attempts, usage) = self.extract(SEED_SYSTEM, &prompt, true, |text| {
            extract::parse_seed(text, &refs)
        })?;
        let first = refs.first().cloned().unwrap_or_default();
        let last = refs.last().cloned().unwrap_or_default();
        let candidates = match answer {
            Extracted::Failed(MAX_TOKENS) if batch.len() > 1 => {
                self.totals.split += 1;
                self.line(
                    &json!({"type": "seedbed", "records": batch.len(), "first": first,
                    "last": last, "outcome": "split", "why": MAX_TOKENS, "attempts": attempts,
                    "tokens_in": usage.0, "tokens_out": usage.1, "usd": usage.2}),
                )?;
                let (head, tail) = batch.split_at(batch.len() / 2);
                for half in [head, tail] {
                    if let Some(stop) = self.seed_batch(garden, half)? {
                        return Ok(Some(stop));
                    }
                }
                return Ok(None);
            }
            Extracted::OverBudget => return Ok(Some(Stop::SpendLimit)),
            Extracted::Failed(why) => {
                self.totals.malformed += 1;
                self.totals.left += batch.len() as u64;
                self.line(
                    &json!({"type": "seedbed", "records": batch.len(), "first": first,
                    "last": last, "outcome": "left", "why": why, "attempts": attempts,
                    "tokens_in": usage.0, "tokens_out": usage.1, "usd": usage.2}),
                )?;
                return Ok(None);
            }
            Extracted::Answer(candidates) => candidates,
        };
        self.totals.taken += batch.len() as u64;
        let items: Vec<SeedbedDoneItem> = batch
            .iter()
            .zip(candidates)
            .map(|(record, candidates)| SeedbedDoneItem {
                r#ref: record.r#ref.clone(),
                candidates: Some(
                    candidates
                        .iter()
                        .map(|c| seed_proposal(record, c))
                        .collect(),
                ),
            })
            .collect();
        let count: usize = items
            .iter()
            .map(|i| i.candidates.as_ref().map_or(0, Vec::len))
            .sum();
        let stats = self.done(garden, &items)?;
        self.line(
            &json!({"type": "seedbed", "records": batch.len(), "first": first,
            "last": last, "outcome": "done", "candidates": count, "outcomes": stats,
            "attempts": attempts, "tokens_in": usage.0, "tokens_out": usage.1,
            "usd": usage.2}),
        )?;
        Ok(None)
    }

    /// `seedbed_done` of `items`, counted; the counts of their outcomes.
    fn done(&mut self, garden: &Garden, items: &[SeedbedDoneItem]) -> Result<Value, Fail> {
        let mut stats = Totals::default();
        for chunk in items.chunks(MAX_SEED_BATCH) {
            let answer = garden
                .seedbed_done(&SeedbedDoneRequest {
                    items: chunk.to_vec(),
                })
                .map_err(|e| daemon(&e))?;
            for outcome in answer.items.iter().flat_map(|i| &i.outcomes) {
                stats.proposed += 1;
                stats.count(outcome);
            }
        }
        self.totals.proposed += stats.proposed;
        self.totals.accepted += stats.accepted;
        self.totals.digest += stats.digest;
        self.totals.refused += stats.refused;
        self.totals.duplicate += stats.duplicate;
        Ok(
            json!({"proposed": stats.proposed, "accepted": stats.accepted,
            "digest": stats.digest, "refused": stats.refused, "duplicate": stats.duplicate}),
        )
    }

    fn dry_seedbed(&mut self) -> Result<Stop, Fail> {
        let garden = self.garden()?;
        let mut estimate = Estimate::default();
        let mut after = None;
        loop {
            let page = self.take(&garden, after.clone())?;
            let Some(last) = page.last() else { break };
            after = Some(last.r#ref.clone());
            estimate.items += page.len() as u64;
            let (batches, _) = self.batches(page);
            for batch in batches {
                let prompt: String = batch.iter().map(extract::seed_line).collect();
                self.estimate_call(&mut estimate, SEED_SYSTEM, &prompt);
            }
        }
        self.print(Stage::Seedbed, &estimate)?;
        Ok(Stop::Done)
    }

    /// One extraction of `prompt` under `system`, asked once more when the answer is malformed;
    /// with the attempts and the tokens in, out and USD of all of them. With `split`, an answer
    /// that hits `max_tokens` fails at once instead of being asked again.
    fn extract<T>(
        &mut self,
        system: &str,
        prompt: &str,
        split: bool,
        parse: impl Fn(&str) -> Result<T, extract::Malformed>,
    ) -> Result<(Extracted<T>, u32, Usage), Fail> {
        let estimate = estimate_tokens(system) + estimate_tokens(prompt);
        let mut usage = (0, 0, 0.0);
        let mut why = "";
        for attempt in 1..=2 {
            let cost = self.settings.prices.input_cost(estimate);
            let allowed = if self.judging {
                self.judge_budget.allows(cost)
            } else {
                self.budget.allows(cost)
            };
            if !allowed {
                if attempt == 1 {
                    return Ok((Extracted::OverBudget, 0, usage));
                }
                return Ok((Extracted::Failed(why), 1, usage));
            }
            let Some(model) = self.model.as_deref_mut() else {
                return Err("no model".to_owned());
            };
            let reply = match model.call(system, prompt) {
                Ok(reply) => reply,
                Err(CallError::TooLong(_)) => {
                    return Ok((Extracted::Failed("too long for the model"), attempt, usage));
                }
                Err(CallError::Timeout(e)) => {
                    if self.judging {
                        self.totals.judge_timeouts += 1;
                        self.judge_stats.timeouts += 1;
                    }
                    return Err(e);
                }
                Err(CallError::Gateway(e)) => return Err(e),
            };
            let (tokens_in, tokens_out, usd) = self.charge(&reply, estimate);
            if self.judging && reply.model.is_some() {
                self.judge_model.clone_from(&reply.model);
            }
            usage = (usage.0 + tokens_in, usage.1 + tokens_out, usage.2 + usd);
            if reply.stop == StopReason::MaxTokens {
                why = MAX_TOKENS;
                if split {
                    return Ok((Extracted::Failed(why), attempt, usage));
                }
                continue;
            }
            if reply.stop == StopReason::Refusal {
                why = "the model refused";
                continue;
            }
            match parse(&reply.text) {
                Ok(answer) => return Ok((Extracted::Answer(answer), attempt, usage)),
                Err(e) => why = e,
            }
        }
        Ok((Extracted::Failed(why), 2, usage))
    }

    /// Counts the tokens and cost of `reply`, priced from its usage, else from the estimates.
    fn charge(&mut self, reply: &Reply, estimate: u64) -> (u64, u64, f64) {
        let prices = self.settings.prices;
        let (tokens_in, tokens_out, usd) = if let Some(u) = &reply.usage {
            (
                u.input + u.cache_read + u.cache_write,
                u.output,
                prices.cost(u),
            )
        } else {
            let output = estimate_tokens(&reply.text);
            let usage = crate::wire::c4g::Usage {
                input: estimate,
                output,
                cache_read: 0,
                cache_write: 0,
            };
            (estimate, output, prices.cost(&usage))
        };
        if self.judging {
            self.judge_budget.charge(usd);
            self.totals.judge_usd += usd;
            let s = &mut self.judge_stats;
            s.calls += 1;
            s.tokens_in += tokens_in;
            s.tokens_out += tokens_out;
        } else {
            self.budget.charge(usd);
        }
        self.totals.calls += 1;
        self.totals.tokens_in += tokens_in;
        self.totals.tokens_out += tokens_out;
        self.totals.usd += usd;
        (tokens_in, tokens_out, usd)
    }

    // Stage d.

    /// The judge: takes the facts the garden hinted page by page, sends them in calls of whole
    /// entities, and hands the verdicts to `reconcile_done`, from a budget of its own. A batch
    /// whose answer is malformed twice is left for the next night; facts left when the budget
    /// is spent wait too. The judge's counts reach the garden with each `reconcile_done`, and a
    /// last one carries what is left when the stage ends.
    fn judge(&mut self) -> Result<Stop, Fail> {
        let garden = self.garden()?;
        self.judging = true;
        let stop = self.judge_walk(&garden);
        self.judging = false;
        let flushed = self.judge_done(&garden, Vec::new()).map(drop);
        let stop = stop?;
        flushed?;
        Ok(stop)
    }

    fn judge_walk(&mut self, garden: &Garden) -> Result<Stop, Fail> {
        let mut after = None;
        loop {
            if self.judge_budget.exhausted() {
                return Ok(Stop::SpendLimit);
            }
            let page = garden
                .reconcile_take(&ReconcileTakeRequest {
                    limit: Some(MAX_JUDGE_BATCH as u64),
                    after,
                    hint: None,
                })
                .map_err(|e| daemon(&e))?
                .items;
            let Some(last) = page.last() else {
                return Ok(Stop::Done);
            };
            after = Some(last.seq);
            let budget = self
                .settings
                .max_prompt_tokens
                .saturating_sub(estimate_tokens(JUDGE_SYSTEM));
            for batch in judge::batches(&page, self.settings.judge_batch, budget) {
                if let Some(stop) = self.judge_batch(garden, &batch)? {
                    return Ok(stop);
                }
            }
        }
    }

    /// Sends one judge call; `Some` stops the stage. An answer that hits `max_tokens` is split
    /// in halves down to one fact, as in stage a.
    fn judge_batch(
        &mut self,
        garden: &Garden,
        batch: &[&ReconcileItem],
    ) -> Result<Option<Stop>, Fail> {
        let prompt = judge::render(batch);
        let (answer, attempts, usage) = self.extract(JUDGE_SYSTEM, &prompt, true, |text| {
            judge::parse(text, batch)
        })?;
        let first = batch.first().map_or(0, |i| i.seq);
        let last = batch.last().map_or(0, |i| i.seq);
        let mut line = json!({"type": "judge", "facts": batch.len(), "first": first, "last": last,
            "attempts": attempts, "tokens_in": usage.0, "tokens_out": usage.1, "usd": usage.2});
        let judged = match answer {
            Extracted::Failed(MAX_TOKENS) if batch.len() > 1 => {
                line["outcome"] = "split".into();
                self.line(&line)?;
                let (head, tail) = batch.split_at(batch.len() / 2);
                for half in [head, tail] {
                    if let Some(stop) = self.judge_batch(garden, half)? {
                        return Ok(Some(stop));
                    }
                }
                return Ok(None);
            }
            Extracted::OverBudget => return Ok(Some(Stop::SpendLimit)),
            Extracted::Failed(why) => {
                self.totals.judge_failures += 1;
                self.judge_stats.failures += 1;
                line["outcome"] = "left".into();
                line["why"] = why.into();
                self.line(&line)?;
                return Ok(None);
            }
            Extracted::Answer(judged) => judged,
        };
        let done = self.judge_done(garden, judged)?;
        let count = |what: ReconcileApplied| {
            done.iter()
                .flat_map(|i| &i.outcomes)
                .filter(|o| o.applied == what)
                .count() as u64
        };
        let (merged, superseded, contradictions, stale) = (
            count(ReconcileApplied::Merged),
            count(ReconcileApplied::Superseded),
            count(ReconcileApplied::Contradiction),
            count(ReconcileApplied::Stale),
        );
        self.totals.judged += done.len() as u64;
        self.totals.merged += merged;
        self.totals.superseded += superseded;
        self.totals.contradictions += contradictions;
        self.totals.stale += stale;
        line["outcome"] = "done".into();
        line["merged"] = merged.into();
        line["superseded"] = superseded.into();
        line["contradictions"] = contradictions.into();
        line["stale"] = stale.into();
        self.line(&line)?;
        Ok(None)
    }

    /// `reconcile_done` of `items` with the judge's counts not yet sent; nothing when both are
    /// empty. The outcomes.
    fn judge_done(
        &mut self,
        garden: &Garden,
        items: Vec<ReconcileJudged>,
    ) -> Result<Vec<crate::wire::c7::ReconcileDoneItem>, Fail> {
        let stats = self.judge_stats.wire();
        if items.is_empty() && stats.is_none() {
            return Ok(Vec::new());
        }
        let label = format!(
            "{} {PROMPT_VERSION}",
            self.judge_model.as_deref().unwrap_or(&self.settings.route)
        );
        let answer = garden
            .reconcile_done(&ReconcileDoneRequest {
                judge: label,
                items,
                stats,
            })
            .map_err(|e| daemon(&e))?;
        self.judge_stats = JudgeCounts::default();
        Ok(answer.items)
    }

    fn dry_judge(&mut self) -> Result<Stop, Fail> {
        let garden = self.garden()?;
        let mut estimate = Estimate::default();
        let mut after = None;
        let budget = self
            .settings
            .max_prompt_tokens
            .saturating_sub(estimate_tokens(JUDGE_SYSTEM));
        loop {
            let page = garden
                .reconcile_take(&ReconcileTakeRequest {
                    limit: Some(MAX_JUDGE_BATCH as u64),
                    after,
                    hint: Some(false),
                })
                .map_err(|e| daemon(&e))?
                .items;
            let Some(last) = page.last() else { break };
            after = Some(last.seq);
            estimate.items += page.len() as u64;
            for batch in judge::batches(&page, self.settings.judge_batch, budget) {
                self.estimate_call(&mut estimate, JUDGE_SYSTEM, &judge::render(&batch));
            }
        }
        self.print(Stage::Judge, &estimate)?;
        Ok(Stop::Done)
    }

    // Stage c.

    fn transcript_budget(&self) -> u64 {
        self.settings
            .max_prompt_tokens
            .saturating_sub(estimate_tokens(HISTORY_SYSTEM))
    }

    /// The entries of `source` after `after`: up to `end` when given, else a new window of at
    /// most [`WINDOW`] entries.
    fn window(
        &self,
        archive: &Archive,
        source: &str,
        after: u64,
        end: Option<u64>,
    ) -> Result<Vec<Listed>, Fail> {
        let mut entries: Vec<Listed> = Vec::new();
        let mut cursor = after;
        loop {
            let page = archive
                .list(&ListRequest {
                    source: source.to_owned(),
                    after: Some(cursor),
                    limit: Some(LIST_PAGE),
                    text_chars: Some(self.settings.message_chars as u64),
                })
                .map_err(|e| daemon(&e))?;
            let more = page.more;
            for entry in page.entries {
                if end.is_some_and(|end| entry.seq > end) {
                    return Ok(entries);
                }
                cursor = entry.seq;
                entries.push(entry);
                if end.is_none() && entries.len() >= WINDOW {
                    return Ok(entries);
                }
            }
            if !more || end.is_some_and(|end| cursor >= end) {
                return Ok(entries);
            }
        }
    }

    fn parts(&self, source: &str, entries: &[Listed]) -> Vec<Part> {
        let s = self.settings;
        render::group(entries, &s.separator, &s.roles, s.message_chars)
            .iter()
            .flat_map(|session| render::parts(source, session, self.transcript_budget()))
            .collect()
    }

    fn history(&mut self) -> Result<Stop, Fail> {
        if self.settings.sources.is_empty() {
            return Ok(Stop::Done);
        }
        let garden = self.garden()?;
        let archive = self.archive()?;
        let dir = need(self.state.as_ref(), STATE)?;
        let mut cursor = Cursor::load(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let save = |cursor: &Cursor| {
            cursor
                .save(&dir)
                .map_err(|e| format!("{}: {e}", dir.join(state::CURSOR).display()))
        };
        let stop = self.walk(&garden, &archive, &mut cursor, &save);
        let sources = &self.settings.sources;
        self.totals.waiting = sources
            .iter()
            .filter_map(|s| cursor.get(s))
            .map(Source::waiting)
            .sum();
        match stop? {
            Stop::Done if self.totals.waiting > 0 => Ok(Stop::DoneWithFailures),
            stop => Ok(stop),
        }
    }

    /// Walks every source: first the failed sessions and parts of closed windows, then the
    /// windows after the cursor. A failed one does not stop the walk; its window closes with
    /// it kept for the next run.
    fn walk(
        &mut self,
        garden: &Garden,
        archive: &Archive,
        cursor: &mut Cursor,
        save: &dyn Fn(&Cursor) -> Result<(), Fail>,
    ) -> Result<Stop, Fail> {
        for source in self.settings.sources.clone() {
            // A snapshot: the cursor drops each key once it is done or given up.
            for retry in cursor.source(&source).retry.clone() {
                let entries = self.window(archive, &source, retry.after, Some(retry.end))?;
                let parts = self.parts(&source, &entries);
                for (key, failures) in &retry.failed {
                    // A key no longer in its window (its entries were deleted) is dropped.
                    let done = match parts.iter().find(|p| &p.key == key) {
                        Some(part) => {
                            match self.attempt(garden, archive, &source, part, *failures)? {
                                Ok(done) => done,
                                Err(stop) => return Ok(stop),
                            }
                        }
                        None => true,
                    };
                    let given_up = !done && failures + 1 >= GIVE_UP_AFTER;
                    self.totals.given_up += u64::from(given_up);
                    cursor
                        .source(&source)
                        .retried(retry.end, key, done || given_up);
                    save(cursor)?;
                }
            }
            loop {
                let st = cursor.source(&source).clone();
                let entries = self.window(archive, &source, st.after, st.window_end)?;
                let Some(last) = entries.last().map(|e| e.seq) else {
                    if st.window_end.is_some() {
                        cursor.source(&source).close();
                        save(cursor)?;
                    }
                    break;
                };
                if st.window_end.is_none() {
                    cursor.source(&source).open(last);
                    save(cursor)?;
                }
                for part in &self.parts(&source, &entries) {
                    if cursor.source(&source).is_done(&part.key) {
                        continue;
                    }
                    let failures = cursor.source(&source).failures(&part.key);
                    let done = match self.attempt(garden, archive, &source, part, failures)? {
                        Ok(done) => done,
                        Err(stop) => return Ok(stop),
                    };
                    let st = cursor.source(&source);
                    if done {
                        st.done.push(part.key.clone());
                    } else if st.fail(&part.key) >= GIVE_UP_AFTER {
                        st.done.push(part.key.clone());
                        self.totals.given_up += 1;
                    }
                    save(cursor)?;
                }
                // Every part of the window was tried in this run: the failed ones move to the
                // retries of the next run.
                cursor.source(&source).close();
                save(cursor)?;
            }
        }
        Ok(Stop::Done)
    }

    /// Sends `part`: whether it is done, or the stop when the session limit or the budget ends
    /// the run first.
    fn attempt(
        &mut self,
        garden: &Garden,
        archive: &Archive,
        source: &str,
        part: &Part,
        failures: u32,
    ) -> Result<Result<bool, Stop>, Fail> {
        if self.options.limit_sessions.is_some_and(|n| self.tried >= n) {
            return Ok(Err(Stop::SessionLimit));
        }
        if self.budget.exhausted() {
            return Ok(Err(Stop::SpendLimit));
        }
        self.tried += 1;
        Ok(
            match self.session(garden, archive, source, part, failures)? {
                Sent::Done => Ok(true),
                Sent::Failed => Ok(false),
                Sent::OverBudget => Err(Stop::SpendLimit),
            },
        )
    }

    /// Sends one session or part, hands its candidates to the garden and stores its summary in
    /// the forest.
    fn session(
        &mut self,
        garden: &Garden,
        archive: &Archive,
        source: &str,
        part: &Part,
        failures: u32,
    ) -> Result<Sent, Fail> {
        let count = part.messages.len();
        let (answer, attempts, usage) =
            self.extract(HISTORY_SYSTEM, &part.transcript, false, |text| {
                extract::parse_history(text, count)
            })?;
        let span = part.span();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let time = |at: f64| zone::rfc3339(at.max(0.0) as u64);
        let mut line = json!({
            "type": "session",
            "source": source,
            "session": part.session,
            "part": part.index,
            "parts": part.count,
            "messages": count,
            "from": span.map(|(from, _)| time(from)),
            "to": span.map(|(_, to)| time(to)),
            "attempts": attempts,
            "tokens_in": usage.0,
            "tokens_out": usage.1,
            "usd": usage.2,
        });
        let History {
            summary,
            candidates,
        } = match answer {
            Extracted::OverBudget => return Ok(Sent::OverBudget),
            Extracted::Failed(why) => {
                self.totals.malformed += 1;
                let given_up = failures + 1 >= GIVE_UP_AFTER;
                line["outcome"] = if given_up { "given_up" } else { "left" }.into();
                line["why"] = why.into();
                self.line(&line)?;
                return Ok(Sent::Failed);
            }
            Extracted::Answer(history) => history,
        };
        self.totals.sessions += 1;
        self.totals.messages += count as u64;
        let mut seeded: Vec<String> = Vec::new();
        let mut refused = 0u64; // answered `rejected` by `propose`
        for candidate in &candidates {
            let Some(message) = candidate.message.and_then(|n| part.messages.get(n - 1)) else {
                continue;
            };
            let request = history_proposal(candidate, &message.id, message.at);
            let answer = garden.propose(&request).map_err(|e| daemon(&e))?;
            if answer.status == ProposalStatus::Seeded {
                if !seeded.contains(&answer.proposal_id) {
                    seeded.push(answer.proposal_id);
                }
            } else {
                self.totals.proposed += 1;
                self.totals.count(&answer);
                refused += 1;
            }
        }
        let items: Vec<SeedbedDoneItem> = seeded
            .iter()
            .map(|r| SeedbedDoneItem {
                r#ref: r.clone(),
                candidates: None,
            })
            .collect();
        let stats = if items.is_empty() {
            json!({})
        } else {
            self.done(garden, &items)?
        };
        let forest = archive
            .forest_put(&self.forest_record(source, part, summary, span))
            .map_err(|e| daemon(&e))?;
        line["outcome"] = "done".into();
        line["forest"] = forest.id.into();
        line["forest_status"] = forest.status.as_str().into();
        if let Some(replaced) = forest.replaced {
            line["forest_replaced"] = replaced.into();
        }
        line["candidates"] = candidates.len().into();
        line["refused_at_propose"] = refused.into();
        line["outcomes"] = stats;
        self.line(&line)?;
        Ok(Sent::Done)
    }

    /// The forest record of `part` of `source`: its summary, span and messages, made by this run
    /// on the night's route.
    fn forest_record(
        &self,
        source: &str,
        part: &Part,
        summary: String,
        span: Option<(f64, f64)>,
    ) -> ForestPutRequest {
        let run = self
            .report
            .as_ref()
            .and_then(|r| r.path().file_stem())
            .and_then(|stem| stem.to_str())
            .map_or_else(
                || format!("{}-{}", self.started, std::process::id()),
                str::to_owned,
            );
        ForestPutRequest {
            source: source.to_owned(),
            session: part.session.clone(),
            text: summary,
            from: span.map(|(from, _)| from),
            to: span.map(|(_, to)| to),
            links: part
                .messages
                .iter()
                .map(|m| ForestLink {
                    seq: m.seq,
                    id: m.id.clone(),
                })
                .collect(),
            producer: Producer {
                run,
                route: self.settings.route.clone(),
            },
        }
    }

    fn dry_history(&mut self) -> Result<Stop, Fail> {
        let mut estimate = Estimate::default();
        if self.settings.sources.is_empty() {
            self.print(Stage::History, &estimate)?;
            return Ok(Stop::Done);
        }
        let archive = self.archive()?;
        let cursor = match self.state.as_ref() {
            Some(dir) => Cursor::load(dir).map_err(|e| format!("{}: {e}", dir.display()))?,
            None => Cursor::default(),
        };
        let limit = self.options.limit_sessions.unwrap_or(u64::MAX);
        'sources: for source in &self.settings.sources {
            let mut st = cursor.get(source).cloned().unwrap_or_default();
            for retry in &st.retry {
                let entries = self.window(&archive, source, retry.after, Some(retry.end))?;
                for part in self.parts(source, &entries) {
                    if !retry.failed.iter().any(|(k, _)| *k == part.key) {
                        continue;
                    }
                    if estimate.sessions >= limit {
                        break 'sources;
                    }
                    estimate.sessions += 1;
                    estimate.waiting += 1;
                    estimate.messages += part.messages.len() as u64;
                    self.estimate_call(&mut estimate, HISTORY_SYSTEM, &part.transcript);
                }
            }
            loop {
                let entries = self.window(&archive, source, st.after, st.window_end)?;
                let Some(last) = entries.last().map(|e| e.seq) else {
                    break;
                };
                estimate.items += entries.len() as u64;
                for part in self.parts(source, &entries) {
                    if st.is_done(&part.key) {
                        continue;
                    }
                    if estimate.sessions >= limit {
                        break 'sources;
                    }
                    estimate.sessions += 1;
                    estimate.waiting += u64::from(st.failures(&part.key) > 0);
                    estimate.messages += part.messages.len() as u64;
                    self.estimate_call(&mut estimate, HISTORY_SYSTEM, &part.transcript);
                }
                st.window_end = Some(last);
                st.close();
            }
        }
        self.print(Stage::History, &estimate)?;
        Ok(Stop::Done)
    }
}

/// How sending one session or part ended.
enum Sent {
    Done,
    Failed,
    OverBudget,
}

fn fact_text(fact: &str) -> String {
    match fact.char_indices().nth(MAX_FACT_CHARS) {
        Some((at, _)) => fact[..at].to_owned(),
        None => fact.to_owned(),
    }
}

/// The proposal of `candidate` from seedbed record `record`: the record's evidence, source kind,
/// observation time and privacy, the model's typing.
#[must_use]
pub fn seed_proposal(record: &SeedRecord, candidate: &Candidate) -> ProposeRequest {
    let own = &record.candidate;
    ProposeRequest {
        fact: fact_text(&candidate.fact),
        entity: Some(candidate.entity.clone()),
        source_ref: own.source_ref.clone(),
        valid_from: candidate.valid_from,
        kind: Some(candidate.kind),
        domain: None,
        attribute: candidate.attribute.clone(),
        valid_to: candidate.valid_to,
        source_kind: own.source_kind,
        observed_at: own.observed_at,
        confidence: candidate.confidence,
        ttl_kind: candidate.ttl_kind,
        secret_ref: None,
        private: own.private,
        pin: None,
    }
}

/// The proposal of `candidate` from the archive item `id` written at `at`: raw material with
/// the item as its evidence (ADR-0055 section 6).
#[must_use]
pub fn history_proposal(candidate: &Candidate, id: &str, at: Option<f64>) -> ProposeRequest {
    ProposeRequest {
        fact: fact_text(&candidate.fact),
        entity: Some(candidate.entity.clone()),
        source_ref: format!("archive:{id}"),
        valid_from: candidate.valid_from,
        kind: Some(candidate.kind),
        domain: None,
        attribute: candidate.attribute.clone(),
        valid_to: candidate.valid_to,
        source_kind: Some(SourceKind::Raw),
        observed_at: at,
        confidence: candidate.confidence,
        ttl_kind: candidate.ttl_kind,
        secret_ref: None,
        private: None,
        pin: None,
    }
}
