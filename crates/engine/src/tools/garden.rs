//! The `garden_search`, `garden_propose` and `garden_review` tools (ADR-0047 section 3c,
//! ADR-0077, ADR-0096): the garden's C7 `search` with full facts and their provenance, C7
//! `propose`, which writes the model's candidate into the seedbed as an unprocessed record for
//! the night to decide, and C7 `reconcile_review`, what the night's judge changed on its own.

use serde_json::{Map, Value, json};
use ulei_contracts::generated::{c4g, c7};

use super::forget::garden_failure;
use super::{
    Args, Context, GARDEN_PROPOSE, GARDEN_REVIEW, GARDEN_SEARCH, Output, json_array, json_object,
    json_str, object_schema,
};
use crate::compress::to_u64;
use crate::garden;
use crate::policy::Origin;
use crate::zone::rfc3339;

/// Most facts of a search, C7's cap.
const MAX_K: i64 = 50;

const SEARCH_DESCRIPTION: &str = "Search the garden: the typed facts about the owner's life \
    (people, places, events, items, preferences, visits) with their provenance, and the \
    seedbed's unprocessed records of the day. Use it first for a fact about the owner or their \
    world, and when a garden pointer in the message names a fact you need. Every fact carries \
    its ref, entity, validity, source and score; recheck_before_acting asks you to verify the \
    fact before you act on it, an unprocessed record has not been reviewed yet, and a disputed \
    one conflicts with the fact conflicts_with (conflict_note says why): when it matters for \
    what you do, ask the owner which holds. as_of answers \
    as the garden stood at that time. For past conversations and raw originals use \
    archive_search, which also asks the garden first.";

const PROPOSE_DESCRIPTION: &str = "Propose a fact for the garden when the owner asks you to \
    remember something or corrects a fact. It is recorded in the seedbed as an unprocessed \
    record; the garden's night review checks it against what the garden holds and decides. \
    One self-contained fact per call, about one entity, with source_ref saying where it comes \
    from (for example the message). Never put a secret value (a password, token or card \
    number) into a fact: say only that the secret exists and where it is kept.";

/// `garden_search` over the garden of `garden`.
pub struct GardenSearch {
    garden: garden::Client,
}

impl GardenSearch {
    /// The tool over the garden of `garden`.
    pub fn new(garden: garden::Client) -> Self {
        Self { garden }
    }

    fn call(&self, args: &Args<'_>) -> Result<Output, String> {
        let q = args.str("q")?.trim();
        if q.is_empty() {
            return Err("q must not be empty".into());
        }
        let k = args
            .opt_i64("k")?
            .map(|k| k.clamp(1, MAX_K).cast_unsigned());
        let as_of = args.opt_utc("as_of", true)?;
        let request = c7::SearchRequest {
            q: q.to_owned(),
            k,
            as_of: as_of.map(|at| unix(at.max(0))),
            private: Some(true),
            no_miss: None,
        };
        let found = self
            .garden
            .search(&request)
            .map_err(|e| format!("the garden was not searched: {}", garden_failure(&e)))?;
        let mut fields = vec![
            ("success", "true".to_owned()),
            ("facts", json_array(found.facts.iter().map(fact))),
        ];
        if found.lexical_only == Some(true) {
            fields.push((
                "note",
                json_str("the garden's dense side is down: these facts are lexical matches only"),
            ));
        }
        Ok(Output::ok(json_object(fields)))
    }
}

impl super::Tool for GardenSearch {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: GARDEN_SEARCH.into(),
            description: SEARCH_DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "q": {
                        "type": "string",
                        "description": "What to look for, in words."
                    },
                    "k": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_K,
                        "description": "Most facts to return (default: the garden's, 5)."
                    },
                    "as_of": {
                        "type": "string",
                        "description": "Answer as the garden stood at this UTC time, RFC 3339 or YYYY-MM-DD (its end)."
                    }
                }),
                &["q"],
            ),
            native: None,
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        self.call(&Args(input)).unwrap_or_else(Output::error)
    }
}

/// `garden_propose` of one turn over the garden of `garden`; the candidate's source is the
/// owner in a top-level owner turn and the agent in any other.
pub struct GardenPropose {
    garden: garden::Client,
    origin: Origin,
}

impl GardenPropose {
    /// The tool of a turn whose prompt has `origin`, over the garden of `garden`.
    pub fn new(garden: garden::Client, origin: Origin) -> Self {
        Self { garden, origin }
    }

    fn call(&self, args: &Args<'_>) -> Result<Output, String> {
        let text = |name: &str| -> Result<String, String> {
            let value = args.str(name)?.trim();
            if value.is_empty() {
                return Err(format!("{name} must not be empty"));
            }
            Ok(value.to_owned())
        };
        let source_kind = if self.origin == Origin::TopLevelOwner {
            c7::SourceKind::Owner
        } else {
            c7::SourceKind::Agent
        };
        let request = c7::ProposeRequest {
            fact: text("fact")?,
            entity: Some(text("entity")?),
            source_ref: text("source_ref")?,
            valid_from: None,
            kind: None,
            domain: None,
            attribute: None,
            valid_to: None,
            source_kind: Some(source_kind),
            observed_at: None,
            confidence: None,
            ttl_kind: None,
            secret_ref: None,
            private: None,
            pin: None,
        };
        let answer = self
            .garden
            .propose(&request)
            .map_err(|e| format!("the fact was not proposed: {}", garden_failure(&e)))?;
        if answer.status == c7::ProposalStatus::Rejected {
            return Err(format!(
                "the garden refused the fact: {}",
                answer.reason.map_or("no reason given", refusal)
            ));
        }
        let mut fields = vec![
            ("success", "true".to_owned()),
            ("status", json_str(answer.status.as_str())),
            ("ref", json_str(&answer.proposal_id)),
        ];
        if answer.status == c7::ProposalStatus::Seeded {
            fields.push((
                "note",
                json_str(
                    "recorded in the seedbed as an unprocessed record; the garden's night \
                     review checks it against what the garden holds and decides",
                ),
            ));
        }
        if let Some(fact) = &answer.fact_id {
            fields.push(("fact_id", json_str(fact)));
        }
        if let Some(reason) = answer.reason {
            fields.push(("reason", json_str(reason.as_str())));
        }
        if let Some(other) = &answer.conflicts_with {
            fields.push(("conflicts_with", json_str(other)));
            fields.push((
                "disputed",
                json_str(&format!(
                    "it conflicts with the current fact {other} until the night's review"
                )),
            ));
        }
        Ok(Output::ok(json_object(fields)))
    }
}

impl super::Tool for GardenPropose {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: GARDEN_PROPOSE.into(),
            description: PROPOSE_DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "fact": {
                        "type": "string",
                        "description": "The fact, one self-contained statement."
                    },
                    "entity": {
                        "type": "string",
                        "description": "The person, place, event, item, preference or visit the fact is about, by name."
                    },
                    "source_ref": {
                        "type": "string",
                        "description": "Where the fact comes from: the owner's message, a document, a URL."
                    }
                }),
                &["fact", "entity", "source_ref"],
            ),
            native: None,
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        self.call(&Args(input)).unwrap_or_else(Output::error)
    }
}

const REVIEW_DESCRIPTION: &str = "List what the garden's night review changed on its own, \
    newest first: new facts merged into an earlier fact that says the same (merged), earlier \
    facts replaced by a newer one (superseded), and pairs of facts recorded as contradicting \
    each other (contradiction, open until the owner settles it). Each change carries both \
    facts, the reviewer's one-line reason and its state. Use it when the owner asks what the \
    garden changed or why a fact is gone or disputed. Do not report these changes on your own; \
    the owner can have one reversed.";

/// Most changes of a review.
const MAX_REVIEW: i64 = 50;

/// `garden_review` over the garden of `garden`.
pub struct GardenReview {
    garden: garden::Client,
}

impl GardenReview {
    /// The tool over the garden of `garden`.
    pub fn new(garden: garden::Client) -> Self {
        Self { garden }
    }

    fn call(&self, args: &Args<'_>) -> Result<Output, String> {
        let relation = match args.opt_str("relation")? {
            None => None,
            Some("duplicate") => Some(c7::ReconcileRelation::Duplicate),
            Some("supersedes") => Some(c7::ReconcileRelation::Supersedes),
            Some("contradiction") => Some(c7::ReconcileRelation::Contradiction),
            Some(_) => return Err("relation must be duplicate, supersedes or contradiction".into()),
        };
        let request = c7::ReconcileReviewRequest {
            limit: args
                .opt_i64("limit")?
                .map(|k| k.clamp(1, MAX_REVIEW).cast_unsigned()),
            before: args.opt_i64("before")?.map(|b| b.max(1).cast_unsigned()),
            entity: args.opt_str("entity")?.map(str::to_owned),
            relation,
            open: args.opt_bool("open")?,
        };
        let found = self
            .garden
            .reconcile_review(&request)
            .map_err(|e| format!("the garden was not read: {}", garden_failure(&e)))?;
        let time = |at: f64| json_str(&rfc3339(to_u64(at)));
        let change = |c: &c7::ReconcileChange| {
            let mut fields = vec![
                ("change", c.change_id.to_string()),
                ("at", time(c.at)),
                (
                    "entity",
                    json_str(c.entity_name.as_deref().unwrap_or(&c.entity)),
                ),
                ("relation", json_str(c.relation.as_str())),
                ("applied", json_str(c.applied.as_str())),
                ("state", json_str(c.state.as_str())),
                ("new", json_str(&c.new.fact)),
                ("new_ref", json_str(&c.new.fact_id)),
                ("earlier", json_str(&c.earlier.fact)),
                ("earlier_ref", json_str(&c.earlier.fact_id)),
            ];
            if let Some(note) = &c.note {
                fields.push(("why", json_str(note)));
            }
            json_object(fields)
        };
        let mut fields = vec![
            ("success", "true".to_owned()),
            ("changes", json_array(found.changes.iter().map(change))),
        ];
        if let Some(next) = found.next {
            fields.push(("next_before", next.to_string()));
        }
        Ok(Output::ok(json_object(fields)))
    }
}

impl super::Tool for GardenReview {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: GARDEN_REVIEW.into(),
            description: REVIEW_DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "entity": {
                        "type": "string",
                        "description": "Only changes about this entity, by its id (the entity of a garden fact)."
                    },
                    "relation": {
                        "type": "string",
                        "enum": ["duplicate", "supersedes", "contradiction"],
                        "description": "Only changes of this kind."
                    },
                    "open": {
                        "type": "boolean",
                        "description": "Only contradictions still open."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_REVIEW,
                        "description": "Most changes to return (default 20)."
                    },
                    "before": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Only changes older than this change number (next_before of the last answer)."
                    }
                }),
                &[],
            ),
            native: None,
        }
    }

    fn run(&self, input: &Map<String, Value>, _ctx: Context<'_>) -> Output {
        self.call(&Args(input)).unwrap_or_else(Output::error)
    }
}

/// Why the garden refused a candidate, in words.
fn refusal(reason: c7::ProposalReason) -> &'static str {
    match reason {
        c7::ProposalReason::SecretValue => {
            "secret_value: it looks like a secret value; say only that the secret exists and \
             where it is kept"
        }
        c7::ProposalReason::LiveData => {
            "live_data: it is live data (open now, live arrivals), which the garden never stores"
        }
        c7::ProposalReason::Invalid => {
            "invalid: an empty fact, entity or source_ref, an unknown domain or a fact already \
             past its expiry"
        }
        c7::ProposalReason::LowConfidence => "low_confidence",
        c7::ProposalReason::Contradiction => "contradiction",
        c7::ProposalReason::OwnerRejected => "owner_rejected: the owner rejected this fact",
        c7::ProposalReason::AmbiguousEntity => {
            "ambiguous_entity: the entity name could name more than one entity"
        }
    }
}

#[allow(clippy::cast_precision_loss, reason = "Unix times stay far below 2^52")]
fn unix(at: i64) -> f64 {
    at as f64
}

/// A garden fact or seedbed record with its provenance as the answer shows it.
fn fact(fact: &c7::Fact) -> String {
    let time = |at: f64| json_str(&rfc3339(to_u64(at)));
    let flag = |on: Option<bool>| on == Some(true);
    let mut fields = Vec::new();
    if let Some(id) = &fact.fact_id {
        fields.push(("ref", json_str(id)));
    }
    fields.push(("fact", json_str(&fact.fact)));
    fields.push(("entity", json_str(&fact.entity)));
    if let Some(kind) = fact.kind {
        fields.push(("kind", json_str(kind.as_str())));
    }
    if let Some(domain) = &fact.domain {
        fields.push(("domain", json_str(domain)));
    }
    fields.push(("valid_from", time(fact.valid_from)));
    if let Some(to) = fact.valid_to {
        fields.push(("valid_to", time(to)));
    }
    fields.push(("source_ref", json_str(&fact.source_ref)));
    if let Some(kind) = fact.source_kind {
        fields.push(("source_kind", json_str(kind.as_str())));
    }
    if let Some(at) = fact.expires_at {
        fields.push(("expires_at", time(at)));
    }
    if let Some(count) = fact.proof_count {
        fields.push(("proof_count", count.to_string()));
    }
    for (name, on) in [
        ("pinned", fact.pinned),
        ("recheck_before_acting", fact.recheck_before_acting),
        ("unprocessed", fact.unprocessed),
        ("disputed", fact.disputed),
    ] {
        if flag(on) {
            fields.push((name, "true".to_owned()));
        }
    }
    if let Some(other) = &fact.conflicts_with {
        fields.push(("conflicts_with", json_str(other)));
    }
    fields.push(("score", Value::from(fact.score).to_string()));
    json_object(fields)
}
