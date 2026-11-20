//! The edge: the intake queue in front of the garden. Every candidate is rejected, found to be
//! a duplicate, accepted on its own, or queued for the owner as a contradiction or a doubt
//! (ADR-0046). The queue lives in the edge's own table on the store's connection.

use std::fmt;

use rusqlite::{OptionalExtension, params};

use crate::model::{
    Attrs, Entity, EntityId, EntityKind, Fact, FactId, Provenance, Sensitivity, SourceKind, Ttl,
    TtlKind, UnixTime,
};
use crate::store::{Store, StoreError};
use crate::wire::c7;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS edge_proposal (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    status TEXT NOT NULL,
    reason TEXT,
    candidate TEXT,
    conflicts_with TEXT,
    fact_id TEXT,
    source_ref TEXT,
    created_at INTEGER NOT NULL,
    resolved_at INTEGER
);
CREATE INDEX IF NOT EXISTS edge_proposal_open ON edge_proposal(status, id);
";

/// Tunables of the edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeConfig {
    /// Candidates with a `confidence` below this go to the digest as `low_confidence`.
    pub min_confidence: f64,
}

impl Default for EdgeConfig {
    fn default() -> Self {
        Self {
            min_confidence: 0.6,
        }
    }
}

/// Why a candidate was refused. Refused candidates are never stored or logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// The candidate contains something that looks like a secret value.
    SecretValue,
    /// Live data (a never-cached `ttl_kind`) is never stored.
    LiveData,
    /// The candidate is malformed: blank text, bad time interval, ambiguous entity.
    Invalid,
    /// The owner rejected it from the digest.
    OwnerRejected,
}

impl Rejection {
    /// The C7 reason.
    pub fn wire(self) -> c7::ProposalReason {
        match self {
            Self::SecretValue => c7::ProposalReason::SecretValue,
            Self::LiveData => c7::ProposalReason::LiveData,
            Self::Invalid => c7::ProposalReason::Invalid,
            Self::OwnerRejected => c7::ProposalReason::OwnerRejected,
        }
    }
}

/// What the edge did with a candidate.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Written to the garden as `fact`.
    Accepted {
        /// Proposal id.
        proposal: String,
        /// The new fact.
        fact: FactId,
    },
    /// The entity already holds an equal current fact.
    Duplicate {
        /// Proposal id.
        proposal: String,
        /// The fact already held.
        fact: FactId,
    },
    /// Waits in the digest for the owner.
    Queued {
        /// Proposal id.
        proposal: String,
        /// Why.
        reason: c7::DigestReason,
        /// The current fact it conflicts with, if any.
        conflicts_with: Option<FactId>,
    },
    /// Refused.
    Rejected {
        /// Proposal id.
        proposal: String,
        /// Why.
        reason: Rejection,
    },
}

impl Outcome {
    /// The C7 answer to `propose` or `resolve`.
    pub fn to_wire(&self) -> c7::ProposeResponse {
        let (proposal_id, status, fact_id, reason, conflicts_with) = match self {
            Self::Accepted { proposal, fact } => (
                proposal,
                c7::ProposalStatus::Accepted,
                Some(fact.0.clone()),
                None,
                None,
            ),
            Self::Duplicate { proposal, fact } => (
                proposal,
                c7::ProposalStatus::Duplicate,
                Some(fact.0.clone()),
                None,
                None,
            ),
            Self::Queued {
                proposal,
                reason,
                conflicts_with,
            } => (
                proposal,
                c7::ProposalStatus::Queued,
                None,
                Some(match reason {
                    c7::DigestReason::Contradiction => c7::ProposalReason::Contradiction,
                    c7::DigestReason::LowConfidence => c7::ProposalReason::LowConfidence,
                }),
                conflicts_with.as_ref().map(|f| f.0.clone()),
            ),
            Self::Rejected { proposal, reason } => (
                proposal,
                c7::ProposalStatus::Rejected,
                None,
                Some(reason.wire()),
                None,
            ),
        };
        c7::ProposeResponse {
            proposal_id: proposal_id.clone(),
            status,
            fact_id,
            reason,
            conflicts_with,
        }
    }
}

/// A failure of the edge itself (not a decision about a candidate).
#[derive(Debug)]
pub enum EdgeError {
    /// The store failed.
    Store(StoreError),
    /// The edge's own table failed.
    Sqlite(rusqlite::Error),
    /// No open digest item has this proposal id.
    NotOpen(String),
    /// A `replace` without a `replacement`.
    MissingReplacement,
    /// A queued candidate could not be read back.
    Corrupt(String),
    /// The owner's `replace` carried a replacement the edge refuses; the item stays open.
    ReplacementRefused(Rejection),
}

impl fmt::Display for EdgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store: {e}"),
            Self::Sqlite(e) => write!(f, "edge tables: {e}"),
            Self::NotOpen(id) => write!(f, "no open digest item {id:?}"),
            Self::MissingReplacement => f.write_str("`replace` needs a `replacement`"),
            Self::Corrupt(id) => write!(f, "digest item {id:?} cannot be read"),
            Self::ReplacementRefused(r) => {
                write!(f, "the replacement is refused: {}", r.wire().as_str())
            }
        }
    }
}

impl std::error::Error for EdgeError {}

impl From<StoreError> for EdgeError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<rusqlite::Error> for EdgeError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

/// Rank of a source for reconciliation: owner > raw > internet > agent.
pub fn source_rank(kind: SourceKind) -> u8 {
    match kind {
        SourceKind::Owner => 3,
        SourceKind::Raw => 2,
        SourceKind::Internet => 1,
        SourceKind::Agent => 0,
    }
}

/// Text normalised for duplicate detection: case-folded, whitespace collapsed, trailing
/// sentence punctuation dropped.
pub fn normalise(text: &str) -> String {
    let s = text.split_whitespace().collect::<Vec<_>>().join(" ");
    s.trim_end_matches(['.', '!', '?', ';', ',', '…'])
        .trim_end()
        .to_lowercase()
}

/// The edge over a store. Creates its table on construction.
pub struct Edge<'a> {
    store: &'a mut Store,
    config: EdgeConfig,
}

/// What reconciliation found for a candidate that passed the checks.
enum Verdict {
    Duplicate(FactId),
    /// Write it, after retracting these earlier statements of the same text.
    Accept(Vec<Fact>),
    /// Loses to the fact; the owner may still accept it over these restatements.
    Conflict(FactId, Vec<Fact>),
}

/// Where a candidate lands on the valid-time axis.
#[derive(Clone, Copy)]
struct Span {
    from: UnixTime,
    to: Option<UnixTime>,
}

impl Span {
    fn overlaps(self, f: &Fact) -> bool {
        self.to.is_none_or(|t| f.valid_from < t) && f.valid_to.is_none_or(|t| t > self.from)
    }
}

/// The parts of `old` outside `span` that stay believed once a restatement over `span`
/// replaces it: the time before `span` always, the time after it unless the restatement took
/// its start from `old` (then it only moves the end).
fn kept_slices(old: &Fact, span: Span, inherited_start: bool) -> Vec<(UnixTime, Option<UnixTime>)> {
    let mut keep = Vec::new();
    if old.valid_from < span.from {
        keep.push((old.valid_from, Some(span.from)));
    }
    if let Some(end) = span.to
        && !inherited_start
        && old.valid_to.is_none_or(|t| t > end)
    {
        keep.push((end, old.valid_to));
    }
    keep
}

impl<'a> Edge<'a> {
    /// Opens the edge over `store`, creating the edge table if needed.
    ///
    /// # Errors
    ///
    /// [`EdgeError::Sqlite`] if the table cannot be created.
    pub fn new(store: &'a mut Store, config: EdgeConfig) -> Result<Self, EdgeError> {
        store.conn().execute_batch(SCHEMA)?;
        Ok(Self { store, config })
    }

    /// The store under the edge.
    pub fn store(&self) -> &Store {
        self.store
    }

    /// Decides `candidate` at time `now`: rejects it, reports a duplicate, writes it, or
    /// queues it for the owner.
    ///
    /// # Errors
    ///
    /// Only failures of the store or of the edge table; decisions are [`Outcome`]s.
    pub fn propose(
        &mut self,
        candidate: &c7::ProposeRequest,
        now: UnixTime,
    ) -> Result<Outcome, EdgeError> {
        let entity = match self.check(candidate)? {
            Ok(e) => e,
            Err(reason) => {
                let id = self.record("rejected", Some(reason.wire().as_str()), None, None, now)?;
                return Ok(Outcome::Rejected {
                    proposal: id,
                    reason,
                });
            }
        };
        let (verdict, span) = self.reconcile(entity.as_ref(), candidate, now, false)?;
        if let Some(reason) = self.preflight(candidate, entity.as_ref(), span, now) {
            let id = self.record("rejected", Some(reason.wire().as_str()), None, None, now)?;
            return Ok(Outcome::Rejected {
                proposal: id,
                reason,
            });
        }
        let low = candidate
            .confidence
            .is_some_and(|c| c < self.config.min_confidence);
        match verdict {
            Verdict::Duplicate(fact) => {
                // The source is kept so that confirmations of a fact can be counted later.
                let id = self.record("duplicate", None, None, None, now)?;
                self.close(row_id(&id), "duplicate", None, Some(&fact), now)?;
                self.store.conn().execute(
                    "UPDATE edge_proposal SET source_ref = ?2 WHERE id = ?1",
                    params![row_id(&id), candidate.source_ref],
                )?;
                Ok(Outcome::Duplicate { proposal: id, fact })
            }
            Verdict::Conflict(old, _) => {
                let reason = if low {
                    c7::DigestReason::LowConfidence
                } else {
                    c7::DigestReason::Contradiction
                };
                self.queue(candidate, reason, Some(old), now)
            }
            Verdict::Accept(_) if low => {
                self.queue(candidate, c7::DigestReason::LowConfidence, None, now)
            }
            Verdict::Accept(restated) => {
                let id = self.record("pending", None, None, None, now)?;
                self.decide(row_id(&id), &id, entity, candidate, span, &restated, now)
            }
        }
    }

    /// Open digest items, oldest first, at most `limit`.
    ///
    /// # Errors
    ///
    /// Failures of the edge table or the store.
    pub fn digest(&self, limit: usize) -> Result<Vec<c7::DigestItem>, EdgeError> {
        let mut stmt = self.store.conn().prepare(
            "SELECT id, reason, candidate, conflicts_with, created_at FROM edge_proposal
             WHERE status = 'queued' ORDER BY id LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut items = Vec::with_capacity(rows.len());
        for (id, reason, candidate, conflicts_with, created_at) in rows {
            let pid = proposal_name(id);
            let reason = match reason.as_deref() {
                Some("contradiction") => c7::DigestReason::Contradiction,
                Some("low_confidence") => c7::DigestReason::LowConfidence,
                _ => return Err(EdgeError::Corrupt(pid)),
            };
            let candidate = candidate
                .and_then(|c| serde_json::from_str::<c7::ProposeRequest>(&c).ok())
                .ok_or_else(|| EdgeError::Corrupt(pid.clone()))?;
            let conflicting_fact = match &conflicts_with {
                Some(f) => self.store.fact(&FactId(f.clone()))?.map(|f| f.text),
                None => None,
            };
            items.push(c7::DigestItem {
                proposal_id: pid,
                reason,
                candidate,
                conflicts_with,
                conflicting_fact,
                queued_at: as_f64(created_at),
            });
        }
        Ok(items)
    }

    /// Applies the owner's decision on an open digest item: `accept` writes the candidate
    /// (the store closes the conflicting version), `reject` drops it, `replace` writes
    /// `replacement` instead. A replacement passes the same refusal checks as a proposal; a
    /// refused one leaves the item open.
    ///
    /// # Errors
    ///
    /// [`EdgeError::NotOpen`] for an unknown or already resolved id,
    /// [`EdgeError::MissingReplacement`] for `replace` without one, and store failures.
    pub fn resolve(
        &mut self,
        proposal_id: &str,
        action: c7::ResolveAction,
        replacement: Option<&c7::ProposeRequest>,
        now: UnixTime,
    ) -> Result<Outcome, EdgeError> {
        let not_open = || EdgeError::NotOpen(proposal_id.to_owned());
        let id = row_id(proposal_id);
        let queued = self
            .store
            .conn()
            .query_row(
                "SELECT candidate FROM edge_proposal WHERE id = ?1 AND status = 'queued'",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or_else(not_open)?
            .and_then(|c| serde_json::from_str::<c7::ProposeRequest>(&c).ok())
            .ok_or_else(|| EdgeError::Corrupt(proposal_id.to_owned()))?;
        let chosen = match action {
            c7::ResolveAction::Reject => {
                self.close(id, "rejected", Some("owner_rejected"), None, now)?;
                return Ok(Outcome::Rejected {
                    proposal: proposal_id.to_owned(),
                    reason: Rejection::OwnerRejected,
                });
            }
            c7::ResolveAction::Accept => queued,
            c7::ResolveAction::Replace => replacement.ok_or(EdgeError::MissingReplacement)?.clone(),
        };
        let entity = self
            .check(&chosen)?
            .map_err(EdgeError::ReplacementRefused)?;
        let (verdict, span) = self.reconcile(entity.as_ref(), &chosen, now, true)?;
        if let Some(reason) = self.preflight(&chosen, entity.as_ref(), span, now) {
            return Err(EdgeError::ReplacementRefused(reason));
        }
        match verdict {
            Verdict::Duplicate(fact) => {
                self.close(id, "duplicate", None, Some(&fact), now)?;
                Ok(Outcome::Duplicate {
                    proposal: proposal_id.to_owned(),
                    fact,
                })
            }
            // The owner decided: a conflict is what they chose to override.
            Verdict::Accept(restated) | Verdict::Conflict(_, restated) => {
                self.decide(id, proposal_id, entity, &chosen, span, &restated, now)
            }
        }
    }

    /// Refusal checks that need no history; on success, the entity the candidate names if it
    /// exists.
    fn check(
        &self,
        c: &c7::ProposeRequest,
    ) -> Result<Result<Option<Entity>, Rejection>, EdgeError> {
        if let Some(reason) = refusal(c) {
            return Ok(Err(reason));
        }
        if self.store.ttl_table().rule(ttl_kind(c)).ttl == Ttl::NeverCache {
            return Ok(Err(Rejection::LiveData));
        }
        if let Some(d) = &c.domain
            && self.store.domain(d)?.is_none()
        {
            return Ok(Err(Rejection::Invalid));
        }
        if let Some(e) = self.store.entity(&EntityId(c.entity.clone()))? {
            return Ok(Ok(Some(e)));
        }
        let mut found = self.store.find_entities(&c.entity)?;
        Ok(match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            _ => Err(Rejection::Invalid),
        })
    }

    /// Places `c` on the valid-time axis and weighs it against every version the garden holds
    /// now that it would overlap: versions of the same single-valued attribute (the ones the
    /// store closes) and earlier statements of the same text. The candidate loses to a version
    /// of a higher-ranked source, and to one of an equal source that starts later; without
    /// `owner_decided` the first loss is the conflict.
    fn reconcile(
        &self,
        entity: Option<&Entity>,
        c: &c7::ProposeRequest,
        now: UnixTime,
        owner_decided: bool,
    ) -> Result<(Verdict, Span), EdgeError> {
        let given = Span {
            from: valid_from(c, now),
            to: c.valid_to.map(secs),
        };
        let Some(entity) = entity else {
            return Ok((Verdict::Accept(Vec::new()), given));
        };
        let known: Vec<Fact> = self
            .store
            .history(&entity.id)?
            .into_iter()
            .filter(|f| f.superseded_at.is_none() && f.recorded_at <= now)
            .collect();
        let norm = normalise(&c.fact);
        let same_text = |f: &Fact| normalise(&f.text) == norm;
        let same_attribute = |f: &Fact| c.attribute.is_some() && f.attribute == c.attribute;
        // A restatement without its own start continues the statement it repeats: the one
        // valid now, else the latest one that starts before the restated end.
        let repeats = |f: &&Fact| {
            same_text(f)
                && (Span {
                    from: now,
                    to: Some(now + 1),
                }
                .overlaps(f)
                    || given.to.is_some_and(|t| f.valid_from < t))
        };
        let from = c
            .valid_from
            .map(secs)
            .or_else(|| known.iter().filter(repeats).map(|f| f.valid_from).max())
            .unwrap_or(now);
        // An open-ended version that starts before a later one of the attribute ends where
        // that one begins: old material fills in history, it does not replace the present.
        let to = given.to.or_else(|| {
            known
                .iter()
                .filter(|f| same_attribute(f) && f.valid_from > from)
                .map(|f| f.valid_from)
                .min()
        });
        let span = Span { from, to };
        if let Some(f) = known
            .iter()
            .find(|f| same_text(f) && f.valid_from <= span.from && f.valid_to == span.to)
        {
            return Ok((Verdict::Duplicate(f.id.clone()), span));
        }
        let rivals: Vec<&Fact> = known
            .iter()
            .filter(|f| span.overlaps(f) && (same_text(f) || same_attribute(f)))
            .collect();
        let rank = source_rank(source_kind(c));
        let loss = rivals.iter().find(|old| {
            let old_rank = source_rank(old.provenance.kind);
            !(rank > old_rank || (rank == old_rank && span.from >= old.valid_from))
        });
        let restated: Vec<Fact> = rivals
            .iter()
            .filter(|f| same_text(f))
            .map(|&f| f.clone())
            .collect();
        Ok(match loss {
            Some(old) if !owner_decided => (Verdict::Conflict(old.id.clone(), restated), span),
            _ => (Verdict::Accept(restated), span),
        })
    }

    /// Writes `c` for proposal row `id` and records the outcome; a write the store refuses is
    /// an `invalid` rejection.
    #[allow(clippy::too_many_arguments)]
    fn decide(
        &mut self,
        id: i64,
        name: &str,
        entity: Option<Entity>,
        c: &c7::ProposeRequest,
        span: Span,
        restated: &[Fact],
        now: UnixTime,
    ) -> Result<Outcome, EdgeError> {
        match self.write(entity, c, span, restated, id, now) {
            Ok(fact) => {
                self.close(id, "accepted", None, Some(&fact), now)?;
                Ok(Outcome::Accepted {
                    proposal: name.to_owned(),
                    fact,
                })
            }
            Err(EdgeError::Store(StoreError::Invalid(_))) => {
                // The preflight already ran the pointer-only check, so a refusal here is about
                // the write (e.g. a version recorded after `now`), never a leaked secret.
                let reason = Rejection::Invalid;
                self.close(id, "rejected", Some(reason.wire().as_str()), None, now)?;
                Ok(Outcome::Rejected {
                    proposal: name.to_owned(),
                    reason,
                })
            }
            Err(e) => {
                // Best effort: the row must not claim an outcome that did not happen.
                let _ = self.close(id, "failed", None, None, now);
                Err(e)
            }
        }
    }

    /// Writes `c` over `span` as a new current fact; the `restated` versions are retracted in
    /// its favour but keep what they say outside `span`, and for a single-valued `attribute`
    /// the store closes and slices the other versions it replaces.
    fn write(
        &mut self,
        entity: Option<Entity>,
        c: &c7::ProposeRequest,
        span: Span,
        restated: &[Fact],
        pid: i64,
        now: UnixTime,
    ) -> Result<FactId, EdgeError> {
        let entity = match entity {
            Some(e) => e.id,
            None => self.create_entity(c)?,
        };
        let fact = build_fact(c, entity, span, pid, now);
        for old in restated {
            let keep = kept_slices(old, span, c.valid_from.is_none());
            self.store.supersede_by(&old.id, now, &fact.id, &keep)?;
        }
        self.store.insert_fact(&fact)?;
        Ok(fact.id)
    }

    fn create_entity(&mut self, c: &c7::ProposeRequest) -> Result<EntityId, EdgeError> {
        let base = slug(&c.entity);
        let mut id = EntityId(base.clone());
        let mut n = 1;
        while self.store.entity(&id)?.is_some() {
            n += 1;
            id = EntityId(format!("{base}-{n}"));
        }
        self.store.upsert_entity(&new_entity(c, id.clone()))?;
        Ok(id)
    }

    /// The store's checks of the fact `c` would become over `span`, run before anything is
    /// written so that a refused candidate leaves no entity or fact behind.
    fn preflight(
        &self,
        c: &c7::ProposeRequest,
        entity: Option<&Entity>,
        span: Span,
        now: UnixTime,
    ) -> Option<Rejection> {
        let id = entity.map_or_else(|| EntityId(slug(&c.entity)), |e| e.id.clone());
        if entity.is_none() && new_entity(c, id.clone()).validate().is_err() {
            return Some(Rejection::Invalid);
        }
        let fact = build_fact(c, id, span, 1, now);
        // A check that only a secret pointer fails is about its value leaking.
        let public = Fact {
            sensitivity: Sensitivity::Public,
            ..fact.clone()
        };
        if public.validate().is_err() {
            return Some(Rejection::Invalid);
        }
        if fact.validate().is_err() {
            return Some(Rejection::SecretValue);
        }
        let observed = fact.provenance.observed_at;
        let stale = self
            .store
            .ttl_table()
            .expiry(fact.ttl, observed)
            .is_some_and(|x| x <= now || x <= observed);
        stale.then_some(Rejection::Invalid)
    }

    fn queue(
        &mut self,
        c: &c7::ProposeRequest,
        reason: c7::DigestReason,
        conflicts_with: Option<FactId>,
        now: UnixTime,
    ) -> Result<Outcome, EdgeError> {
        let json = serde_json::to_string(c).map_err(|_| EdgeError::Corrupt(String::new()))?;
        let reason_text = match reason {
            c7::DigestReason::Contradiction => "contradiction",
            c7::DigestReason::LowConfidence => "low_confidence",
        };
        let id = self.record(
            "queued",
            Some(reason_text),
            Some(&json),
            conflicts_with.as_ref().map(|f| f.0.as_str()),
            now,
        )?;
        Ok(Outcome::Queued {
            proposal: id,
            reason,
            conflicts_with,
        })
    }

    /// Inserts a proposal row and returns its name; `link` is the conflicting fact of a queued
    /// row.
    fn record(
        &mut self,
        status: &str,
        reason: Option<&str>,
        candidate: Option<&str>,
        link: Option<&str>,
        now: UnixTime,
    ) -> Result<String, EdgeError> {
        self.store.conn().execute(
            "INSERT INTO edge_proposal (status, reason, candidate, conflicts_with, created_at, resolved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, CASE WHEN ?1 = 'queued' THEN NULL ELSE ?5 END)",
            params![status, reason, candidate, link, now],
        )?;
        Ok(proposal_name(self.store.conn().last_insert_rowid()))
    }

    /// Marks row `id` decided; a decided row no longer holds its candidate.
    fn close(
        &mut self,
        id: i64,
        status: &str,
        reason: Option<&str>,
        fact: Option<&FactId>,
        now: UnixTime,
    ) -> Result<(), EdgeError> {
        self.store.conn().execute(
            "UPDATE edge_proposal SET status = ?2, reason = COALESCE(?3, reason),
             fact_id = ?4, candidate = NULL, resolved_at = ?5 WHERE id = ?1",
            params![id, status, reason, fact.map(|f| f.0.as_str()), now],
        )?;
        Ok(())
    }
}

/// The fact `c` becomes over `span` as proposal `pid`; the store fills the expiry.
fn build_fact(
    c: &c7::ProposeRequest,
    entity: EntityId,
    span: Span,
    pid: i64,
    now: UnixTime,
) -> Fact {
    Fact {
        id: FactId(fact_name(pid)),
        entity,
        text: c.fact.trim().to_owned(),
        domain: c.domain.clone(),
        attribute: c.attribute.clone(),
        ttl: ttl_kind(c),
        valid_from: span.from,
        valid_to: span.to,
        recorded_at: now,
        superseded_at: None,
        superseded_by: None,
        provenance: Provenance {
            kind: source_kind(c),
            source_ref: c.source_ref.clone(),
            observed_at: c.observed_at.map_or(now, secs),
            quote: None,
            proof_count: 1,
        },
        expires_at: None,
        sensitivity: match (&c.secret_ref, c.private) {
            (Some(r), _) => Sensitivity::Secret {
                vault_ref: r.clone(),
            },
            (None, Some(true)) => Sensitivity::Private,
            (None, _) => Sensitivity::Public,
        },
        #[allow(clippy::cast_possible_truncation)]
        confidence: c.confidence.map_or(1.0, |x| x as f32),
    }
}

/// The entity an unknown name in `c` creates, with `id`.
fn new_entity(c: &c7::ProposeRequest, id: EntityId) -> Entity {
    Entity {
        id,
        kind: c.kind.map_or(EntityKind::Item, entity_kind),
        name: c.entity.trim().to_owned(),
        aliases: Vec::new(),
        domain: c.domain.clone(),
        attrs: Attrs::None,
    }
}

fn proposal_name(id: i64) -> String {
    format!("p{id}")
}

/// Row id of a name made by [`proposal_name`]; 0 (no row) for anything else.
fn row_id(name: &str) -> i64 {
    name.strip_prefix('p')
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[allow(clippy::cast_precision_loss)]
fn as_f64(t: UnixTime) -> f64 {
    t as f64
}

fn fact_name(pid: i64) -> String {
    format!("f{pid}")
}

#[allow(clippy::cast_possible_truncation)]
fn secs(t: f64) -> UnixTime {
    t.floor() as UnixTime
}

fn valid_from(c: &c7::ProposeRequest, now: UnixTime) -> UnixTime {
    c.valid_from.map_or(now, secs)
}

fn source_kind(c: &c7::ProposeRequest) -> SourceKind {
    match c.source_kind {
        Some(c7::SourceKind::Owner) => SourceKind::Owner,
        Some(c7::SourceKind::Raw) => SourceKind::Raw,
        Some(c7::SourceKind::Internet) => SourceKind::Internet,
        Some(c7::SourceKind::Agent) | None => SourceKind::Agent,
    }
}

/// The lifetime type of `c`: as proposed, else `personal` for the owner's words and raw
/// material and `general` for the rest (neither expires).
fn ttl_kind(c: &c7::ProposeRequest) -> TtlKind {
    let Some(k) = c.ttl_kind else {
        return match source_kind(c) {
            SourceKind::Owner | SourceKind::Raw => TtlKind::Personal,
            SourceKind::Internet | SourceKind::Agent => TtlKind::General,
        };
    };
    TtlKind::parse(k.as_str()).unwrap_or(TtlKind::General)
}

/// The model kind of a C7 entity kind.
pub fn entity_kind(k: c7::EntityKind) -> EntityKind {
    EntityKind::parse(k.as_str()).unwrap_or(EntityKind::Item)
}

/// The C7 kind of a model entity kind.
pub fn wire_kind(k: EntityKind) -> c7::EntityKind {
    match k {
        EntityKind::Person => c7::EntityKind::Person,
        EntityKind::Place => c7::EntityKind::Place,
        EntityKind::Event => c7::EntityKind::Event,
        EntityKind::Item => c7::EntityKind::Item,
        EntityKind::Preference => c7::EntityKind::Preference,
        EntityKind::Visit => c7::EntityKind::Visit,
    }
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() { "entity".into() } else { out }
}

/// Why `c` must be refused before it touches the garden, if it must.
fn refusal(c: &c7::ProposeRequest) -> Option<Rejection> {
    let fields = [
        Some(c.fact.as_str()),
        Some(c.entity.as_str()),
        Some(c.source_ref.as_str()),
        c.attribute.as_deref(),
        c.domain.as_deref(),
        c.secret_ref.as_deref(),
    ];
    if fields.into_iter().flatten().any(looks_secret) {
        return Some(Rejection::SecretValue);
    }
    // Anything but a vault reference in `secret_ref` may be the value itself.
    if c.secret_ref.as_deref().is_some_and(|r| !is_vault_ref(r)) {
        return Some(Rejection::SecretValue);
    }
    let bad_interval = matches!((c.valid_from, c.valid_to), (Some(a), Some(b)) if b <= a);
    let blank = |s: &str| s.trim().is_empty();
    if blank(&c.fact)
        || blank(&c.entity)
        || blank(&c.source_ref)
        || c.attribute.as_deref().is_some_and(blank)
        || bad_interval
    {
        return Some(Rejection::Invalid);
    }
    None
}

/// Whether `r` is a vault reference: `vault:` and a path without whitespace.
pub fn is_vault_ref(r: &str) -> bool {
    r.strip_prefix("vault:")
        .is_some_and(|p| !p.is_empty() && !p.chars().any(char::is_whitespace))
}

/// Whether `text` looks like it carries a secret value: a payment card number (Luhn), an IBAN
/// (mod 97), a `password: ...`-style assignment, a key followed by a PIN-like token, a
/// well-known API key prefix or a long random token. Errs towards yes.
pub fn looks_secret(text: &str) -> bool {
    has_card_number(text)
        || has_iban(text)
        || has_assignment(text)
        || text.split_whitespace().any(|w| {
            let w = w.trim_matches(|c: char| "\"'`()[]{}<>,;".contains(c));
            has_key_prefix(w) || is_random_token(w)
        })
        || text.contains("-----BEGIN")
}

fn has_card_number(text: &str) -> bool {
    // Digits joined by at most single spaces or dashes form one run.
    let check = |d: &[u8]| (13..=19).contains(&d.len()) && luhn(d);
    let mut digits = Vec::new();
    let mut gap = false;
    for ch in text.chars() {
        if let Some(v) = ch.to_digit(10) {
            digits.push(u8::try_from(v).unwrap_or(0));
            gap = false;
        } else if (ch == ' ' || ch == '-') && !gap && !digits.is_empty() {
            gap = true;
        } else {
            if check(&digits) {
                return true;
            }
            digits.clear();
            gap = false;
        }
    }
    check(&digits)
}

fn luhn(d: &[u8]) -> bool {
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &v)| {
            let v = u32::from(v);
            if i % 2 == 1 {
                let x = v * 2;
                if x > 9 { x - 9 } else { x }
            } else {
                v
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

fn has_iban(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len()).any(|i| {
        if !chars[i].is_ascii_alphabetic() || (i > 0 && chars[i - 1].is_alphanumeric()) {
            return false;
        }
        // An IBAN is often written in groups of four separated by single spaces.
        let mut s = Vec::with_capacity(34);
        let mut prev_space = false;
        for &c in &chars[i..] {
            if c.is_ascii_alphanumeric() && s.len() < 34 {
                s.push(c.to_ascii_uppercase());
                prev_space = false;
            } else if c == ' ' && !prev_space {
                prev_space = true;
            } else {
                break;
            }
        }
        s.len() >= 15
            && s[1].is_ascii_alphabetic()
            && s[2].is_ascii_digit()
            && s[3].is_ascii_digit()
            && (15..=s.len()).any(|n| iban_mod97(&s[..n]))
    })
}

fn iban_mod97(s: &[char]) -> bool {
    let mut rem: u32 = 0;
    for &c in s[4..].iter().chain(&s[..4]) {
        let Some(v) = c.to_digit(36) else {
            return false;
        };
        rem = if v < 10 {
            (rem * 10 + v) % 97
        } else {
            (rem * 100 + v) % 97
        };
    }
    rem == 1
}

/// Words naming a secret. A Cyrillic key is a stem and may take up to two more letters of an
/// ending (пароль, пароля, паролем); a Latin key may take a plural `s`.
const SECRET_KEYS: [&str; 28] = [
    "password",
    "passwd",
    "passcode",
    "passphrase",
    "pwd",
    "pin",
    "pin code",
    "pin-code",
    "door code",
    "gate code",
    "access code",
    "entry code",
    "lock code",
    "alarm code",
    "security code",
    "cvv",
    "cvc",
    "token",
    "api_key",
    "api-key",
    "apikey",
    "api key",
    "secret",
    "парол",
    "пин",
    "пин-код",
    "код",
    "токен",
];

/// Words between a key and its `:` or `=` ("password for the cottage: ...").
const MAX_GAP_WORDS: usize = 3;

fn has_assignment(text: &str) -> bool {
    let lower = text.to_lowercase();
    SECRET_KEYS.iter().any(|k| {
        lower.match_indices(k).any(|(i, _)| {
            if lower[..i]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
            {
                return false;
            }
            let rest = &lower[i + k.len()..];
            let ending = rest.chars().take_while(|c| c.is_alphanumeric()).count();
            let cyrillic = k.chars().any(|c| ('а'..='я').contains(&c));
            let allowed = if cyrillic {
                ending <= 2
            } else {
                ending == 0 || rest.starts_with('s') && ending == 1
            };
            if !allowed {
                return false;
            }
            let rest: String = rest.chars().skip(ending).collect();
            value_follows(&rest)
        })
    })
}

/// Whether the text after a secret key holds a value: one after `:` or `=` within
/// [`MAX_GAP_WORDS`] words, a PIN-like token (four or more characters with a digit) right
/// after the key, or a shaped word after `is`.
fn value_follows(rest: &str) -> bool {
    let words: Vec<&str> = rest.split_whitespace().collect();
    let clean = |w: &str| {
        w.trim_matches(|c: char| "\"'`.,;:=()".contains(c))
            .to_owned()
    };
    let value = |w: &str| w.chars().count() >= 4 && !w.starts_with("vault");
    let pin_like = |w: &str| value(w) && w.chars().any(|c| c.is_ascii_digit());
    for (n, w) in words.iter().enumerate().take(MAX_GAP_WORDS + 1) {
        if w.contains("://") {
            return false;
        }
        if let Some(at) = w.find([':', '=']) {
            let tail = clean(&w[at + 1..]);
            let v = if tail.is_empty() {
                words.get(n + 1).map(|w| clean(w)).unwrap_or_default()
            } else {
                tail
            };
            return value(&v);
        }
        if n == 0 {
            if pin_like(&clean(w)) {
                return true;
            }
            if *w == "is" {
                // After "is" only a word with a digit or a symbol counts: "is weak" is prose.
                let v = words.get(1).map(|w| clean(w)).unwrap_or_default();
                return value(&v) && v.chars().any(|c| !c.is_alphabetic());
            }
        }
        if w.ends_with(['.', '!', '?']) {
            return false;
        }
    }
    false
}

fn has_key_prefix(w: &str) -> bool {
    const PREFIXES: [&str; 9] = [
        "sk-",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "AIza",
    ];
    let long = w.len() >= 20;
    (long && PREFIXES.iter().any(|p| w.starts_with(p)))
        || (w.len() == 20
            && w.starts_with("AKIA")
            && w[4..]
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
}

fn is_random_token(w: &str) -> bool {
    if w.len() < 32 || w.contains("://") {
        return false;
    }
    let charset = w
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_+/=".contains(&b));
    let letters = w.bytes().filter(u8::is_ascii_alphabetic).count();
    let digits = w.bytes().filter(u8::is_ascii_digit).count();
    charset && letters > 0 && digits > 0 && digits * 10 >= w.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_detectors() {
        assert!(looks_secret("card 4111 1111 1111 1111 for groceries"));
        assert!(
            !looks_secret("card 4111 1111 1111 1112 for groceries"),
            "Luhn fails"
        );
        assert!(looks_secret("salary goes to GB82 WEST 1234 5698 7654 32"));
        assert!(looks_secret("wifi password: hunter2x"));
        assert!(looks_secret("пароль = qwerty12"));
        assert!(looks_secret("my password is hunter2"));
        assert!(looks_secret("door code: 4821#"));
        for leak in [
            "Ada's wifi password for the cottage: Sunflower77",
            "Ada's wifi password for the cottage: Sunflower",
            "пароль от почты: qwerty12",
            "Пароль от домофона = 4821",
            "PIN 4821",
            "PIN 4821.",
            "door code 4821",
            "the pin-code is 4821",
            "пин-код 4821",
            "код 4821",
            "Passwords: hunter2x",
            "password:hunter2",
        ] {
            assert!(looks_secret(leak), "{leak}");
        }
        // Key-shaped samples are assembled at run time so secret scanners of the repository
        // do not flag the test itself.
        let gh = ["ghp", "_abcdefghijklmnopqrstuvwxyz0123"].concat();
        assert!(looks_secret(&format!("key {gh}")));
        let aws = ["AK", "IA", &"Q".repeat(16)].concat();
        assert!(looks_secret(&aws));
        assert!(looks_secret("x 9f8e7d6c5b4a39281706f5e4d3c2b1a0aa11bb22"));
        for ok in [
            "The passport is in the vault.",
            "The password is stored in the vault.",
            "Ada Example lives at 7 Sample Lane.",
            "Dentist appointment on 2030-04-01 at 10:00, phone +1 555 0100.",
            "Read https://example.invalid/articles/2030/garden-memory-and-retrieval",
            "The pin code of the bike lock is in the vault",
            "The token is valid until spring.",
            "The zip code of the office is in the letter.",
            "Trudy pinned 2030 as the year of the move.",
            "Налоговый кодекс 2031 года лежит на полке.",
            "The password of the bank is in the vault.",
            "The door code changed in 2030.",
            "PIN of the bike lock: vault entry",
            "Check the token docs at https://example.invalid/a:b",
        ] {
            assert!(!looks_secret(ok), "{ok}");
        }
    }

    #[test]
    fn normalisation() {
        assert_eq!(normalise("  Likes   Tea. "), "likes tea");
        assert_eq!(normalise("likes tea"), normalise("LIKES TEA!"));
    }

    #[test]
    fn slugs() {
        assert_eq!(slug(" Ada  Example "), "ada-example");
        assert_eq!(slug("!!"), "entity");
    }
}
