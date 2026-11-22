//! The `forget` tool: candidate refs from the history index, and forget marks on journal
//! messages made and lifted, only in a turn whose prompt is the owner's.

use std::path::PathBuf;

use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::{Args, Context, FORGET, Output, json_array, json_object, json_str, object_schema};
use crate::agent::utc;
use crate::journal::marks::Target;
use crate::journal::search::{Search, Sort};
use crate::journal::{Journal, Turn, unix_now};
use crate::policy::Origin;

/// Most candidates a `query` answers.
const MAX_CANDIDATES: u32 = 20;

const DESCRIPTION: &str = "Forget journal messages: a forgotten message is left out of every \
    history search until its mark is lifted; nothing is deleted. Use only when the owner asks. \
    `query` only lists candidate refs and marks nothing. `refs` with `reason` marks them; `refs` \
    with `undo` lifts their marks. A ref is `c8:<n>` for one message or `c8:session:<id>` for \
    the messages that session holds at that moment; its later messages are not covered.";

const NOT_OWNER: &str = "forget runs only when the owner asks: this turn's prompt is not the \
    owner's, so nothing was marked or lifted";
const NOTHING_MARKED: &str = "Nothing was marked. To forget, call forget with refs and reason.";

/// The tool of one turn: the journal it marks and the turn and origin of its provenance.
pub struct Forget {
    journal: PathBuf,
    turn: Turn,
    origin: Origin,
}

impl Forget {
    /// The tool of turn `turn`, whose prompt has `origin`, over the journal file `journal`.
    pub fn new(journal: PathBuf, turn: Turn, origin: Origin) -> Self {
        Self {
            journal,
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
        let journal = || Journal::open(&self.journal).map_err(|e| e.to_string());
        match (query, refs) {
            (Some(_), Some(_)) => Err("give either query or refs, not both".into()),
            (None, None) => Err("give query to list candidates, or refs to mark".into()),
            (Some(query), None) => {
                if undo || reason.is_some() {
                    return Err("query only lists candidates; reason and undo go with refs".into());
                }
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
                let hits = journal()?.search(&search).map_err(|e| e.to_string())?;
                let candidates = hits.iter().map(|hit| {
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
                });
                Ok(Output::ok(json_object([
                    ("success", "true".into()),
                    ("candidates", json_array(candidates)),
                    ("note", json_str(NOTHING_MARKED)),
                ])))
            }
            (None, Some(refs)) => {
                if filtered {
                    return Err("session_id, since and until go only with query".into());
                }
                let targets = targets(refs)?;
                let now = unix_now();
                if undo {
                    let lifted = journal()?
                        .lift(&targets, self.turn, now)
                        .map_err(|e| e.to_string())?;
                    return Ok(Output::ok(json_object([
                        ("success", "true".into()),
                        ("lifted", lifted.to_string()),
                    ])));
                }
                let reason = reason.ok_or("reason is required to mark refs")?;
                let marking = journal()?
                    .mark(&targets, reason, self.turn, now)
                    .map_err(|e| e.to_string())?;
                Ok(Output::ok(json_object([
                    ("success", "true".into()),
                    ("marked", marking.marked.to_string()),
                    ("already_forgotten", marking.already.to_string()),
                ])))
            }
        }
    }
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
                        "description": "Words to look for in the history; answers candidate refs and marks nothing."
                    },
                    "session_id": {
                        "type": "integer",
                        "description": "With query: only this session."
                    },
                    "since": {
                        "type": "string",
                        "description": "With query: not before this UTC time, RFC 3339 or YYYY-MM-DD."
                    },
                    "until": {
                        "type": "string",
                        "description": "With query: not after this UTC time, RFC 3339 or YYYY-MM-DD."
                    },
                    "refs": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Messages to mark, or with undo to bring back: c8:<n> or c8:session:<id>."
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

/// The targets of `refs`, a non-empty array of `c8:<n>` and `c8:session:<id>`; any other ref
/// refuses the whole call.
fn targets(refs: &Value) -> Result<Vec<Target>, String> {
    let refs = refs
        .as_array()
        .filter(|refs| !refs.is_empty())
        .ok_or("refs must be a non-empty array of refs")?;
    refs.iter()
        .map(|r| {
            let r = r.as_str().ok_or("every ref must be a string")?;
            let id = |text: &str| text.parse::<u64>().ok().filter(|id| *id > 0);
            if let Some(session) = r.strip_prefix("c8:session:") {
                id(session)
                    .map(Target::Session)
                    .ok_or_else(|| format!("invalid ref {r}: c8:session:<id> takes a session id"))
            } else if let Some(n) = r.strip_prefix("c8:") {
                id(n)
                    .map(Target::Message)
                    .ok_or_else(|| format!("invalid ref {r}: c8:<n> takes a message n"))
            } else if r.starts_with("archive:") {
                Err(format!(
                    "{r} refused: the archive is not marked or changed by forget; nothing was marked"
                ))
            } else if r.starts_with("seed:") {
                Err(format!(
                    "{r}: seedbed refs are not supported yet; nothing was marked"
                ))
            } else {
                Err(format!("unknown ref {r}: use c8:<n> or c8:session:<id>"))
            }
        })
        .collect()
}
