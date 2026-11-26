//! One night run (ADR-0078 sections 2 to 6): stage b clears the forget queue, stage a sorts the
//! seedbed, stage c sorts the history of the configured archive sources into the garden and its
//! session summaries into the forest (ADR-0079), all within the spend limit; or, with
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
use crate::night::render::{self, Part};
use crate::night::spend::{Budget, estimate_output, estimate_tokens};
use crate::night::state::{self, Cursor, Report, Source};
use crate::night::{MAX_SEED_BATCH, STATE, Settings};
use crate::wire::c4g::StopReason;
use crate::wire::c7::{
    ForgetDoneRequest, ForgetTakeRequest, ProposalStatus, ProposeRequest, ProposeResponse,
    SeedRecord, SeedbedDoneItem, SeedbedDoneRequest, SeedbedTakeRequest, SourceKind,
};
use crate::wire::c10::{ForestLink, ForestPutRequest, ListRequest, Listed, Producer};
use crate::zone;

/// Most entries of one window of a history walk.
pub const WINDOW: usize = 1_000;
/// Entries per `list` page.
const LIST_PAGE: u64 = 500;
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
}

impl Stage {
    /// The stage of its letter.
    #[must_use]
    pub fn parse(letter: &str) -> Option<Self> {
        match letter {
            "a" => Some(Self::Seedbed),
            "b" => Some(Self::Forget),
            "c" => Some(Self::History),
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
        }
    }
}

/// What a run does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Call no model and write nothing; print the estimates.
    pub dry_run: bool,
    /// The stages to run; they run in the order b, a, c whatever the order given.
    pub stages: Vec<Stage>,
    /// Most sessions or parts stage c sends.
    pub limit_sessions: Option<u64>,
}

/// Why a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Every selected stage ran to its end.
    Done,
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
    /// Forget queue rows taken.
    pub forget_rows: u64,
    /// Facts the forget queue closed.
    pub cleared: u64,
    /// Sessions or parts sent.
    pub sessions: u64,
    /// Sessions or parts given up.
    pub given_up: u64,
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
            "ulei-engine night: {} taken {} proposed {} accepted {} digest {} refused {} malformed {} cleared {} sessions {} tokens {}/{} usd {:.4} in {seconds:.0} s",
            stop.word(),
            self.taken,
            self.proposed,
            self.accepted,
            self.digest,
            self.refused,
            self.malformed,
            self.cleared,
            self.sessions,
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
            "forget_rows": self.forget_rows,
            "cleared": self.cleared,
            "sessions": self.sessions,
            "given_up": self.given_up,
            "messages": self.messages,
            "calls": self.calls,
            "tokens_in": self.tokens_in,
            "tokens_out": self.tokens_out,
            "usd": self.usd,
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
        for stage in [Stage::Forget, Stage::Seedbed, Stage::History] {
            if !self.options.stages.contains(&stage) {
                continue;
            }
            let result = match (stage, self.options.dry_run) {
                (Stage::Forget, false) => self.forget(),
                (Stage::Forget, true) => self.dry_forget(),
                (Stage::Seedbed, false) => self.seedbed(),
                (Stage::Seedbed, true) => self.dry_seedbed(),
                (Stage::History, false) => self.history(),
                (Stage::History, true) => self.dry_history(),
            };
            match result {
                Ok(Stop::Done) => {}
                Ok(stop) => return stop,
                Err(e) => return Stop::Failed(e),
            }
        }
        Stop::Done
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
        let line = json!({
            "stage": stage.letter(),
            "items": estimate.items,
            "sessions": estimate.sessions,
            "messages": estimate.messages,
            "calls": estimate.calls,
            "tokens_in": estimate.tokens_in,
            "tokens_out": estimate.tokens_out,
            "usd": estimate.usd,
        });
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

    /// Sends one batch; `Some` stops the run.
    fn seed_batch(&mut self, garden: &Garden, batch: &[SeedRecord]) -> Result<Option<Stop>, Fail> {
        let prompt: String = batch.iter().map(extract::seed_line).collect();
        let refs: Vec<String> = batch.iter().map(|r| r.r#ref.clone()).collect();
        let (answer, attempts, usage) = self.extract(SEED_SYSTEM, &prompt, |text| {
            extract::parse_seed(text, &refs)
        })?;
        let first = refs.first().cloned().unwrap_or_default();
        let last = refs.last().cloned().unwrap_or_default();
        let candidates = match answer {
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
    /// with the attempts and the tokens in, out and USD of all of them.
    fn extract<T>(
        &mut self,
        system: &str,
        prompt: &str,
        parse: impl Fn(&str) -> Result<T, extract::Malformed>,
    ) -> Result<(Extracted<T>, u32, Usage), Fail> {
        let estimate = estimate_tokens(system) + estimate_tokens(prompt);
        let mut usage = (0, 0, 0.0);
        let mut why = "";
        for attempt in 1..=2 {
            if !self
                .budget
                .allows(self.settings.prices.input_cost(estimate))
            {
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
                Err(CallError::Gateway(e)) => return Err(e),
            };
            let (tokens_in, tokens_out, usd) = self.charge(&reply, estimate);
            usage = (usage.0 + tokens_in, usage.1 + tokens_out, usage.2 + usd);
            if reply.stop == StopReason::MaxTokens {
                why = "the answer hit max_tokens";
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
        self.budget.charge(usd);
        self.totals.calls += 1;
        self.totals.tokens_in += tokens_in;
        self.totals.tokens_out += tokens_out;
        self.totals.usd += usd;
        (tokens_in, tokens_out, usd)
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
        for source in self.settings.sources.clone() {
            loop {
                let st = cursor.source(&source).clone();
                let entries = self.window(&archive, &source, st.after, st.window_end)?;
                let Some(last) = entries.last().map(|e| e.seq) else {
                    if st.window_end.is_some() {
                        cursor.source(&source).close();
                        save(&cursor)?;
                    }
                    break;
                };
                if st.window_end.is_none() {
                    cursor.source(&source).open(last);
                    save(&cursor)?;
                }
                let parts = self.parts(&source, &entries);
                for part in &parts {
                    if cursor.source(&source).is_done(&part.key) {
                        continue;
                    }
                    if self.options.limit_sessions.is_some_and(|n| self.tried >= n) {
                        return Ok(Stop::SessionLimit);
                    }
                    if self.budget.exhausted() {
                        return Ok(Stop::SpendLimit);
                    }
                    self.tried += 1;
                    let failures = cursor.source(&source).failures(&part.key);
                    match self.session(&garden, &archive, &source, part, failures)? {
                        Sent::Done => cursor.source(&source).done.push(part.key.clone()),
                        Sent::Failed => {
                            let st = cursor.source(&source);
                            if st.fail(&part.key) >= GIVE_UP_AFTER {
                                st.done.push(part.key.clone());
                                self.totals.given_up += 1;
                            }
                        }
                        Sent::OverBudget => return Ok(Stop::SpendLimit),
                    }
                    save(&cursor)?;
                }
                let st: &mut Source = cursor.source(&source);
                if parts.iter().all(|p| st.is_done(&p.key)) {
                    st.close();
                    save(&cursor)?;
                } else {
                    break;
                }
            }
        }
        Ok(Stop::Done)
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
        let (answer, attempts, usage) = self.extract(HISTORY_SYSTEM, &part.transcript, |text| {
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
