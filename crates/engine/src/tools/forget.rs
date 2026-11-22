//! The `forget` tool: candidate refs from the history index and the garden's seedbed, forget
//! marks on journal messages and seedbed records made and lifted, each ref also sent to the
//! garden's C7 `forget`, only in a turn whose prompt is the owner's.

use std::path::PathBuf;

use serde_json::{Map, Value, json};
use ulei_contracts::generated::{c4g, c7};

use super::{Args, Context, FORGET, Output, json_array, json_object, json_str, object_schema};
use crate::agent::utc;
use crate::garden;
use crate::journal::marks::{Target, provenance};
use crate::journal::search::{Search, Sort};
use crate::journal::{Journal, Turn, unix_now};
use crate::policy::Origin;

/// Most candidates a `query` answers from each side.
const MAX_CANDIDATES: u32 = 20;
/// Most refs of one call and of one C7 `forget`, so every seedbed ref fits the first request.
const MAX_REFS: usize = 100;
/// Facts a `query` asks of the garden's `search` (its maximum): garden facts rank among the
/// seedbed records and are left out of the candidates.
const GARDEN_K: u64 = 50;

const DESCRIPTION: &str = "Forget records: a forgotten journal message is left out of every \
    history search, and a forgotten seedbed record (the garden's unprocessed records of the \
    day) out of every garden search, until its mark is lifted; nothing is deleted, and the \
    garden queues the facts drawn from them for its night review. Use only when the owner asks. \
    `query` only lists candidate refs from the history and the seedbed and marks nothing. `refs` \
    with `reason` marks them; `refs` with `undo` lifts their marks. A ref is `c8:<n>` for one \
    message, `c8:session:<id>` for the messages that session holds at that moment (its later \
    messages are not covered), or `seed:<seq>` for one seedbed record.";

const NOT_OWNER: &str = "forget runs only when the owner asks: this turn's prompt is not the \
    owner's, so nothing was marked or lifted";
const NOTHING_MARKED: &str = "Nothing was marked. To forget, call forget with refs and reason.";
const NO_GARDEN: &str = "the garden is not configured (garden.socket is unset)";

/// The tool of one turn: the journal and the garden it marks and the turn and origin of its
/// provenance.
pub struct Forget {
    journal: PathBuf,
    garden: Option<garden::Client>,
    turn: Turn,
    origin: Origin,
}

/// A ref of `refs`.
enum Ref {
    Journal(Target),
    Seed(u64),
}

/// What the garden did with the refs of one call.
#[derive(Default)]
struct Sent {
    /// Seedbed marks written, or lifted with `undo`.
    marks: u64,
    /// Seedbed refs whose record had a live mark already, or with `undo` none.
    unchanged: u64,
    /// Queue rows written, or removed with `undo`; `None` when no request was answered.
    queue: Option<u64>,
    /// Why the garden did not take the last refs, how many it did not take and of how many.
    failed: Option<(String, usize, usize)>,
}

impl Forget {
    /// The tool of turn `turn`, whose prompt has `origin`, over the journal file `journal` and
    /// the garden of `garden`, `None` when no garden is configured.
    pub fn new(
        journal: PathBuf,
        garden: Option<garden::Client>,
        turn: Turn,
        origin: Origin,
    ) -> Self {
        Self {
            journal,
            garden,
            turn,
            origin,
        }
    }

    fn call(&self, args: &Args<'_>) -> Result<Output, String> {
        let query = args.opt_str("query")?;
        let refs = args.present("refs");
        let undo = args.opt_bool("undo")?.unwrap_or(false);
        let reason = args.opt_str("reason")?.filter(|r| !r.trim().is_empty());
        let filtered = ["session_id", "since", "until"]
            .iter()
            .any(|name| args.present(name).is_some());
        match (query, refs) {
            (Some(_), Some(_)) => Err("give either query or refs, not both".into()),
            (None, None) => Err("give query to list candidates, or refs to mark".into()),
            (Some(query), None) => {
                if undo || reason.is_some() {
                    return Err("query only lists candidates; reason and undo go with refs".into());
                }
                self.candidates(args, query)
            }
            (None, Some(refs)) => {
                if filtered {
                    return Err("session_id, since and until go only with query".into());
                }
                let refs = parse(refs)?;
                if undo {
                    self.lift(&refs)
                } else {
                    let reason = reason.ok_or("reason is required to mark refs")?;
                    self.mark(&refs, reason)
                }
            }
        }
    }

    fn open(&self) -> Result<Journal, String> {
        Journal::open(&self.journal).map_err(|e| e.to_string())
    }

    /// The history candidates of `query` within the filters of `args`, then the seedbed's.
    fn candidates(&self, args: &Args<'_>, query: &str) -> Result<Output, String> {
        let search = Search {
            query,
            roles: &[],
            session: args
                .opt_i64("session_id")?
                .map(|id| u64::try_from(id).map_err(|_| "session_id must be positive"))
                .transpose()?,
            since: args.opt_utc("since", false)?,
            until: args.opt_utc("until", true)?,
            sort: Sort::Rank,
            limit: MAX_CANDIDATES,
        };
        let hits = self.open()?.search(&search).map_err(|e| e.to_string())?;
        let mut candidates: Vec<String> = hits
            .iter()
            .map(|hit| {
                let mut fields = vec![
                    ("ref", json_str(&format!("c8:{}", hit.n))),
                    ("session_id", hit.session.to_string()),
                ];
                if let Some(at) = hit.at {
                    fields.push(("when", json_str(&utc(at))));
                }
                fields.push(("role", json_str(hit.role.as_str())));
                fields.push(("snippet", json_str(&hit.snippet)));
                json_object(fields)
            })
            .collect();
        let mut garden_line = None;
        match self.seedbed(query) {
            Ok(seeds) => candidates.extend(seeds),
            Err(why) => {
                garden_line = Some(format!("{why}; seedbed candidates are not listed"));
            }
        }
        let mut fields = vec![
            ("success", "true".to_owned()),
            ("candidates", json_array(candidates)),
        ];
        if let Some(line) = garden_line {
            fields.push(("garden", json_str(&line)));
        }
        fields.push(("note", json_str(NOTHING_MARKED)));
        Ok(Output::ok(json_object(fields)))
    }

    /// The seedbed records the garden's `search` finds for `query`, as candidates.
    fn seedbed(&self, query: &str) -> Result<Vec<String>, String> {
        let garden = self.garden.as_ref().ok_or(NO_GARDEN)?;
        let request = c7::SearchRequest {
            q: query.to_owned(),
            k: Some(GARDEN_K),
            as_of: None,
            private: Some(true),
        };
        let found = garden.search(&request).map_err(|e| garden_failure(&e))?;
        Ok(found
            .facts
            .iter()
            .filter_map(|fact| {
                let id = fact
                    .fact_id
                    .as_deref()
                    .filter(|id| id.starts_with("seed:"))?;
                Some(json_object([
                    ("ref", json_str(id)),
                    ("when", json_str(&utc(whole_seconds(fact.valid_from)))),
                    ("source_ref", json_str(&fact.source_ref)),
                    ("snippet", json_str(&fact.fact)),
                ]))
            })
            .take(MAX_CANDIDATES as usize)
            .collect())
    }

    /// Marks `refs` for `reason`: the garden first, then the journal.
    fn mark(&self, refs: &[Ref], reason: &str) -> Result<Output, String> {
        let (journal, messages) = self.messages(refs)?;
        let sent = self.send(refs, &messages, Some(reason))?;
        let mut marked = sent.marks;
        let mut already = sent.unchanged;
        if let Some(mut journal) = journal {
            let targets: Vec<Target> = messages.iter().copied().map(Target::Message).collect();
            let marking = journal
                .mark(&targets, reason, self.turn, unix_now())
                .map_err(|e| e.to_string())?;
            marked += marking.marked as u64;
            already += marking.already as u64;
        }
        let fields = vec![
            ("success", "true".to_owned()),
            ("marked", marked.to_string()),
            ("already_forgotten", already.to_string()),
        ];
        Ok(Output::ok(json_object(garden_fields(
            fields,
            &sent,
            ("queued", "the journal marks are kept, but"),
        ))))
    }

    /// Lifts the marks of `refs`: the garden first, then the journal.
    fn lift(&self, refs: &[Ref]) -> Result<Output, String> {
        let (journal, messages) = self.messages(refs)?;
        let sent = self.send(refs, &messages, None)?;
        let mut lifted = sent.marks;
        if let Some(mut journal) = journal {
            let targets: Vec<Target> = messages.iter().copied().map(Target::Message).collect();
            let count = journal
                .lift(&targets, self.turn, unix_now())
                .map_err(|e| e.to_string())?;
            lifted += count as u64;
        }
        let fields = vec![
            ("success", "true".to_owned()),
            ("lifted", lifted.to_string()),
        ];
        Ok(Output::ok(json_object(garden_fields(
            fields,
            &sent,
            ("unqueued", "the journal marks are lifted, but"),
        ))))
    }

    /// The journal and the `n` of every message the journal refs of `refs` name, or neither
    /// when `refs` has no journal ref.
    fn messages(&self, refs: &[Ref]) -> Result<(Option<Journal>, Vec<u64>), String> {
        let targets: Vec<Target> = refs
            .iter()
            .filter_map(|r| match r {
                Ref::Journal(target) => Some(*target),
                Ref::Seed(_) => None,
            })
            .collect();
        if targets.is_empty() {
            return Ok((None, Vec::new()));
        }
        let journal = self.open()?;
        let messages = journal.messages_of(&targets).map_err(|e| e.to_string())?;
        Ok((Some(journal), messages))
    }

    /// Sends the seedbed refs of `refs`, then `c8:<n>` of each of `messages`, to C7 `forget`
    /// with `reason`, or with `undo` when `reason` is `None`, in requests of at most
    /// [`MAX_REFS`]. A failure before the seedbed refs were taken refuses the call.
    fn send(&self, refs: &[Ref], messages: &[u64], reason: Option<&str>) -> Result<Sent, String> {
        let seeds: Vec<String> = refs
            .iter()
            .filter_map(|r| match r {
                Ref::Seed(seq) => Some(format!("seed:{seq}")),
                Ref::Journal(_) => None,
            })
            .collect();
        let refused = |why: &str| {
            let undone = if reason.is_some() { "marked" } else { "lifted" };
            format!("{} refused: {why}; nothing was {undone}", seeds.join(", "))
        };
        let all: Vec<String> = seeds
            .iter()
            .cloned()
            .chain(messages.iter().map(|n| format!("c8:{n}")))
            .collect();
        let mut sent = Sent::default();
        if all.is_empty() {
            return Ok(sent);
        }
        let Some(garden) = &self.garden else {
            if !seeds.is_empty() {
                return Err(refused(NO_GARDEN));
            }
            sent.failed = Some((NO_GARDEN.to_owned(), all.len(), all.len()));
            return Ok(sent);
        };
        for (i, chunk) in all.chunks(MAX_REFS).enumerate() {
            let request = c7::ForgetRequest {
                refs: chunk.to_vec(),
                reason: reason.map(str::to_owned),
                provenance: provenance(self.turn),
                undo: reason.is_none().then_some(true),
            };
            match garden.forget(&request) {
                Ok(answer) => {
                    for item in &answer.items {
                        if item.r#ref.starts_with("seed:") {
                            if item.marks == 0 {
                                sent.unchanged += 1;
                            }
                            sent.marks += item.marks;
                        }
                    }
                    let queue = answer.items.iter().map(|item| item.queue).sum::<u64>();
                    sent.queue = Some(sent.queue.unwrap_or(0) + queue);
                }
                Err(e) if i == 0 && !seeds.is_empty() => return Err(refused(&garden_failure(&e))),
                Err(e) => {
                    let unsent = all.len() - i * MAX_REFS;
                    sent.failed = Some((garden_failure(&e), unsent, all.len()));
                    break;
                }
            }
        }
        Ok(sent)
    }
}

/// `fields` with the garden's queue count under `name` when it answered, and a `garden` line
/// saying how much of its queue was not updated when it failed; `kept` says what the journal
/// did all the same.
fn garden_fields<'a>(
    mut fields: Vec<(&'a str, String)>,
    sent: &Sent,
    (name, kept): (&'a str, &str),
) -> Vec<(&'a str, String)> {
    if let Some(queue) = sent.queue {
        fields.push((name, queue.to_string()));
    }
    if let Some((why, unsent, total)) = &sent.failed {
        let line = format!(
            "{why}; {kept} the garden's forget queue was not updated for {unsent} of {total} refs"
        );
        fields.push(("garden", json_str(&line)));
    }
    fields
}

/// The line of a failed garden call.
fn garden_failure(e: &garden::Error) -> String {
    match e {
        garden::Error::Unreachable(_) => format!("the garden is not reachable ({e})"),
        garden::Error::Rejected { .. } => format!("the {e}"),
    }
}

/// The C7 time `at`, at least 0, as whole Unix seconds; the cast saturates.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a C7 time is a non-negative number of seconds"
)]
fn whole_seconds(at: f64) -> u64 {
    at as u64
}

impl super::Tool for Forget {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: FORGET.into(),
            description: DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "Words to look for in the history and the seedbed; answers candidate refs and marks nothing."
                    },
                    "session_id": {
                        "type": "integer",
                        "description": "With query: only history messages of this session."
                    },
                    "since": {
                        "type": "string",
                        "description": "With query: only history messages not before this UTC time, RFC 3339 or YYYY-MM-DD."
                    },
                    "until": {
                        "type": "string",
                        "description": "With query: only history messages not after this UTC time, RFC 3339 or YYYY-MM-DD."
                    },
                    "refs": {
                        "type": "array",
                        "items": {"type": "string"},
                        "maxItems": MAX_REFS,
                        "description": "Records to mark, or with undo to bring back: c8:<n>, c8:session:<id> or seed:<seq>."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why the owner wants them forgotten; required to mark."
                    },
                    "undo": {
                        "type": "boolean",
                        "description": "Lift the marks of refs instead of marking them."
                    }
                }),
                &[],
            ),
            native: None,
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        if self.origin != Origin::TopLevelOwner {
            return Output::error(NOT_OWNER);
        }
        self.call(&Args(input)).unwrap_or_else(Output::error)
    }
}

/// The refs of `refs`, a non-empty array of at most [`MAX_REFS`] refs, each `c8:<n>`,
/// `c8:session:<id>` or `seed:<seq>` with a positive number without leading zeros; any other
/// ref refuses the whole call.
fn parse(refs: &Value) -> Result<Vec<Ref>, String> {
    let refs = refs
        .as_array()
        .filter(|refs| !refs.is_empty())
        .ok_or("refs must be a non-empty array of refs")?;
    if refs.len() > MAX_REFS {
        return Err(format!("at most {MAX_REFS} refs per call"));
    }
    refs.iter()
        .map(|r| {
            let r = r.as_str().ok_or("every ref must be a string")?;
            let id = |text: &str| {
                text.parse::<u64>()
                    .ok()
                    .filter(|id| *id > 0 && id.to_string() == text)
            };
            if let Some(session) = r.strip_prefix("c8:session:") {
                id(session)
                    .map(|id| Ref::Journal(Target::Session(id)))
                    .ok_or_else(|| {
                        format!("invalid ref {r}: c8:session:<id> takes a session id without leading zeros")
                    })
            } else if let Some(n) = r.strip_prefix("c8:") {
                id(n).map(|n| Ref::Journal(Target::Message(n))).ok_or_else(|| {
                    format!("invalid ref {r}: c8:<n> takes a message n without leading zeros")
                })
            } else if let Some(seq) = r.strip_prefix("seed:") {
                id(seq).map(Ref::Seed).ok_or_else(|| {
                    format!("invalid ref {r}: seed:<seq> takes a record seq without leading zeros")
                })
            } else if r.starts_with("archive:") {
                Err(format!(
                    "{r} refused: the archive is not marked or changed by forget; nothing was marked"
                ))
            } else {
                Err(format!(
                    "unknown ref {r}: use c8:<n>, c8:session:<id> or seed:<seq>"
                ))
            }
        })
        .collect()
}
