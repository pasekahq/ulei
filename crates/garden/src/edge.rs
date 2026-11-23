//! The edge: the intake queue in front of the garden. A candidate of the day passes its refusal
//! screen and is written into the seedbed ([`Edge::seed`], ADR-0055); a candidate the edge
//! decides is rejected, found to be a duplicate, accepted on its own, or queued for the owner as
//! a contradiction, a doubt or an ambiguous entity name (ADR-0046); an accepted fact without an
//! attribute waits in `reconcile_queue` until the worker weighs it against the facts most like
//! it. The queues live in tables the store's migrations create; a seedbed write, a propose, a
//! resolve or the processing of a seedbed record is one unit of work of the store.

use std::fmt;

use rusqlite::{OptionalExtension, params};

use crate::model::{
    Attrs, Entity, EntityId, EntityKind, Fact, FactId, Provenance, Sensitivity, SourceKind, Ttl,
    TtlKind, UnixTime,
};
use crate::search::{self, SearchError};
use crate::secret::looks_secret;
use crate::store::{Store, StoreError, sqlite_class};
use crate::translit;
use crate::wire::c7;

/// Transliterated names shorter than this resolve only when equal, never one edit apart.
const MIN_FUZZY_CHARS: usize = 5;

/// [`proof_count`] of the fact `?1`, transitively over merges: a merge's copy is the only
/// version that replaces one on another entity.
const PROOFS: &str = "
WITH RECURSIVE lineage(id, entity) AS (
    SELECT id, entity FROM facts WHERE id = ?1
    UNION
    SELECT f.id, f.entity FROM lineage l
    JOIN facts f ON f.superseded_by = l.id AND f.entity <> l.entity
)
SELECT 1 + count(*) FROM edge_proposal
WHERE status = 'duplicate' AND fact_id IN (SELECT id FROM lineage)";

/// Facts most like a new fact that its reconciliation weighs ([`Edge::reconcile_similar`]).
pub const RECONCILE_K: usize = 5;

/// Tunables of the edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeConfig {
    /// Candidates with a `confidence` below this go to the digest as `low_confidence`.
    pub min_confidence: f64,
    /// Least cosine at which a new fact without an attribute contradicts a current fact of its
    /// entity with another normalised text.
    pub reconcile_min_cosine: f32,
}

impl Default for EdgeConfig {
    fn default() -> Self {
        Self {
            min_confidence: 0.6,
            reconcile_min_cosine: 0.85,
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
    /// The candidate is malformed: blank text, bad time interval, unknown domain, or a
    /// replacement whose entity name is ambiguous.
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
    /// Stored as a seedbed record for the night.
    Seeded {
        /// The record's ref `seed:<seq>`.
        record: String,
        /// The current fact the record disputes.
        conflicts_with: Option<FactId>,
    },
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
            Self::Seeded {
                record,
                conflicts_with,
            } => (
                record,
                c7::ProposalStatus::Seeded,
                None,
                None,
                conflicts_with.as_ref().map(|f| f.0.clone()),
            ),
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
                    c7::DigestReason::AmbiguousEntity => c7::ProposalReason::AmbiguousEntity,
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
    /// An `accept` of an `ambiguous_entity` item without an `entity`.
    MissingEntity,
    /// An `entity` that is not one of the item's candidates, or given to an action other than
    /// `accept`.
    NotACandidate(String),
    /// A queued candidate could not be read back.
    Corrupt(String),
    /// The owner's `replace` carried a replacement the edge refuses; the item stays open.
    ReplacementRefused(Rejection),
    /// Retrieval of the facts like a new fact failed outside the store.
    Search(SearchError),
}

impl fmt::Display for EdgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store: {e}"),
            Self::Sqlite(e) => write!(f, "edge tables: {e}"),
            Self::NotOpen(id) => write!(f, "no open digest item {id:?}"),
            Self::MissingReplacement => f.write_str("`replace` needs a `replacement`"),
            Self::MissingEntity => {
                f.write_str("`accept` of an ambiguous item needs an `entity` from its candidates")
            }
            Self::NotACandidate(e) => {
                write!(f, "`entity` {e:?} is not a candidate this item can accept")
            }
            Self::Corrupt(id) => write!(f, "digest item {id:?} cannot be read"),
            Self::ReplacementRefused(r) => {
                write!(f, "the replacement is refused: {}", r.wire().as_str())
            }
            Self::Search(e) => write!(f, "{e}"),
        }
    }
}

impl EdgeError {
    /// A short name of the kind of failure, free of any stored or proposed text.
    pub fn class(&self) -> &'static str {
        match self {
            Self::Store(e) | Self::Search(SearchError::Store(e)) => e.class(),
            Self::Sqlite(e) => sqlite_class(e),
            Self::NotOpen(_) => "not_open",
            Self::MissingReplacement => "missing_replacement",
            Self::MissingEntity => "missing_entity",
            Self::NotACandidate(_) => "not_a_candidate",
            Self::Corrupt(_) => "corrupt_item",
            Self::ReplacementRefused(_) => "replacement_refused",
            Self::Search(SearchError::Embed(_)) => "embed",
            Self::Search(SearchError::Invalid(_)) => "invalid_search",
        }
    }
}

impl std::error::Error for EdgeError {}

impl From<StoreError> for EdgeError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<SearchError> for EdgeError {
    fn from(e: SearchError) -> Self {
        match e {
            SearchError::Store(e) => Self::Store(e),
            e => Self::Search(e),
        }
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
    seed: Option<Seed>,
}

/// The seedbed record whose candidates the edge decides.
#[derive(Clone, Copy)]
struct Seed {
    seq: i64,
    written_at: UnixTime,
}

/// The entity a candidate's name resolves to.
enum Named {
    Known(Entity),
    New,
    /// Two or more entities found on the first level that finds any, by id.
    Ambiguous(Vec<Entity>),
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
    /// The edge over `store`.
    pub fn new(store: &'a mut Store, config: EdgeConfig) -> Self {
        Self {
            store,
            config,
            seed: None,
        }
    }

    /// The store under the edge.
    pub fn store(&self) -> &Store {
        self.store
    }

    /// Writes `candidate` into the seedbed at `now` as one unit of work (ADR-0055 sections 2
    /// and 4). A candidate the screen refuses, or already past its expiry, is rejected and leaves
    /// only its proposal row. One equal to a stored record not yet expired writes nothing and
    /// answers as that record's write did. Otherwise one record is written, disputing the first
    /// current fact of its entity and attribute with another text.
    ///
    /// # Errors
    ///
    /// Only failures of the store or of the edge table; refusals are [`Outcome`]s.
    pub fn seed(
        &mut self,
        candidate: &c7::ProposeRequest,
        now: UnixTime,
    ) -> Result<Outcome, EdgeError> {
        let config = self.config;
        self.store.atomically(|store| {
            let mut edge = Edge::new(store, config);
            if let Some(reason) = edge.screen(candidate)? {
                return edge.reject(reason, now);
            }
            if let Some(stored) = edge
                .store
                .seeds_said(candidate, now)?
                .into_iter()
                .find(|r| same_candidate(&r.candidate, candidate))
            {
                return Ok(Outcome::Seeded {
                    record: seed_ref(stored.seq),
                    conflicts_with: stored.conflicts_with,
                });
            }
            let (entity, conflicts_with) = edge.dispute(candidate, now)?;
            match edge
                .store
                .write_seed(candidate, entity.as_ref(), conflicts_with.as_ref(), now)
            {
                Ok(seq) => Ok(Outcome::Seeded {
                    record: seed_ref(seq),
                    conflicts_with,
                }),
                // Past the screen the store refuses only a record already expired or with an
                // empty valid interval.
                Err(StoreError::Invalid(_)) => edge.reject(Rejection::Invalid, now),
                Err(e) => Err(e.into()),
            }
        })
    }

    /// The entity a seedbed record of `c` written at `now` resolves to and the fact it
    /// disputes, for a candidate with an entity and an attribute and without a secret pointer:
    /// the one entity [`Edge::name`] finds, and the first by `recorded_at` of its versions of
    /// that attribute believed and valid at `now` whose normalised text differs, when the
    /// candidate itself holds at `now`.
    fn dispute(
        &self,
        c: &c7::ProposeRequest,
        now: UnixTime,
    ) -> Result<(Option<EntityId>, Option<FactId>), EdgeError> {
        let Some(attribute) = c.attribute.as_deref() else {
            return Ok((None, None));
        };
        if c.secret_ref.is_some() {
            return Ok((None, None));
        }
        let Named::Known(entity) = self.name(c)? else {
            return Ok((None, None));
        };
        let holds =
            c.valid_from.map_or(now, secs) <= now && c.valid_to.map(secs).is_none_or(|t| now < t);
        if !holds {
            return Ok((Some(entity.id), None));
        }
        let norm = normalise(&c.fact);
        let rival = self
            .store
            .facts_of(&entity.id, now, now)?
            .into_iter()
            .filter(|f| f.attribute.as_deref() == Some(attribute) && normalise(&f.text) != norm)
            .min_by_key(|f| f.recorded_at)
            .map(|f| f.id);
        Ok((Some(entity.id), rival))
    }

    /// Decides `candidate` at time `now`: rejects it, reports a duplicate, writes it, or
    /// queues it for the owner. The proposal row, a new entity, the fact and the versions it
    /// closes or slices commit together; a failure leaves none of them. The daemon never calls
    /// it: a day's candidate is decided only when [`Edge::process_seed`] processes its record,
    /// with the same decision inside the record's unit of work.
    ///
    /// # Errors
    ///
    /// Only failures of the store or of the edge table; decisions are [`Outcome`]s.
    pub fn propose(
        &mut self,
        candidate: &c7::ProposeRequest,
        now: UnixTime,
    ) -> Result<Outcome, EdgeError> {
        let config = self.config;
        self.store
            .atomically(|store| Edge::new(store, config).intake(candidate, now))
    }

    /// Processes the seedbed record `seq` at `now` in one unit of work: decides `candidates`,
    /// else the record's own candidate, each starting at the record's `written_at` when it has
    /// no `valid_from` and recorded under the record, then marks the record processed with the
    /// answers. A record already processed answers its stored answers and writes nothing.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown record, failures of the store or of the edge
    /// tables; the record then stays unprocessed.
    pub fn process_seed(
        &mut self,
        seq: i64,
        candidates: Option<&[c7::ProposeRequest]>,
        now: UnixTime,
    ) -> Result<Vec<c7::ProposeResponse>, EdgeError> {
        let config = self.config;
        self.store.atomically(|store| {
            let record = store
                .seed(seq)?
                .ok_or_else(|| StoreError::NotFound(format!("record {seq}")))?;
            if let Some((_, outcome)) = record.processed {
                return Ok(outcome);
            }
            let own = [record.candidate];
            let mut edge = Edge {
                store,
                config,
                seed: Some(Seed {
                    seq,
                    written_at: record.written_at,
                }),
            };
            let outcomes = candidates
                .unwrap_or(&own)
                .iter()
                .map(|c| edge.intake(c, now).map(|o| o.to_wire()))
                .collect::<Result<Vec<_>, _>>()?;
            edge.store.mark_seed_processed(seq, now, &outcomes)?;
            Ok(outcomes)
        })
    }

    fn intake(
        &mut self,
        candidate: &c7::ProposeRequest,
        now: UnixTime,
    ) -> Result<Outcome, EdgeError> {
        if let Some(reason) = self.screen(candidate)? {
            return self.reject(reason, now);
        }
        let entity = match self.name(candidate)? {
            Named::Known(e) => Some(e),
            Named::New => None,
            Named::Ambiguous(found) => return self.queue_ambiguous(candidate, &found, now),
        };
        let (verdict, span) = self.reconcile(entity.as_ref(), candidate, now, false)?;
        if let Some(reason) = self.preflight(candidate, entity.as_ref(), span, now) {
            return self.reject(reason, now);
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
                self.queue(candidate, reason, Some(old), &[], now)
            }
            Verdict::Accept(_) if low => {
                self.queue(candidate, c7::DigestReason::LowConfidence, None, &[], now)
            }
            Verdict::Accept(restated) => {
                let id = self.record("pending", None, None, None, now)?;
                let outcome =
                    self.decide(row_id(&id), &id, entity, candidate, span, &restated, now)?;
                if let Outcome::Accepted { fact, .. } = &outcome
                    && candidate.attribute.is_none()
                    && candidate.secret_ref.is_none()
                {
                    let json = serde_json::to_string(candidate)
                        .map_err(|_| EdgeError::Corrupt(id.clone()))?;
                    self.store.conn().execute(
                        "INSERT INTO reconcile_queue (fact, candidate) VALUES (?1, ?2)",
                        params![fact.0, json],
                    )?;
                }
                Ok(outcome)
            }
        }
    }

    /// The oldest fact waiting for [`Edge::reconcile_similar`] that is ready for it under the
    /// embedding model `model`: it has a vector of `model`, or it is no longer current.
    ///
    /// # Errors
    ///
    /// Failures of the edge's tables.
    pub fn unreconciled(&self, model: &str) -> Result<Option<FactId>, EdgeError> {
        Ok(self
            .store
            .conn()
            .query_row(
                "SELECT q.fact FROM reconcile_queue q JOIN facts f ON f.id = q.fact
                 WHERE f.superseded_at IS NOT NULL
                    OR EXISTS (SELECT 1 FROM fact_vectors v WHERE v.model = ?1 AND v.fact = f.seq)
                 ORDER BY q.rowid LIMIT 1",
                [model],
                |r| r.get(0),
            )
            .optional()?
            .map(FactId))
    }

    /// Reconciles the waiting fact `id` once, at `now`, with the [`RECONCILE_K`] current facts
    /// of its entity most like it ([`search::similar`] under `model`) that do not wait
    /// themselves (each pair is weighed when its later fact is): the first of them with a cosine
    /// of at least `reconcile_min_cosine` and another normalised text makes a `contradiction`
    /// digest item for the written fact. A fact no longer current only leaves the queue.
    ///
    /// # Errors
    ///
    /// Failures of the store, the retrieval or the edge's tables; the fact then stays queued.
    pub fn reconcile_similar(
        &mut self,
        id: &FactId,
        model: &str,
        now: UnixTime,
    ) -> Result<(), EdgeError> {
        let config = self.config;
        self.store.atomically(|store| {
            let mut edge = Edge::new(store, config);
            let candidate: String = edge.store.conn().query_row(
                "SELECT candidate FROM reconcile_queue WHERE fact = ?1",
                [&id.0],
                |r| r.get(0),
            )?;
            edge.store
                .conn()
                .execute("DELETE FROM reconcile_queue WHERE fact = ?1", [&id.0])?;
            let fact = edge
                .store
                .fact(id)?
                .ok_or_else(|| EdgeError::Corrupt(id.0.clone()))?;
            if fact.superseded_at.is_some() {
                return Ok(());
            }
            let at = match fact.valid_to {
                Some(end) if end <= now => end - 1,
                _ => now,
            }
            .max(fact.valid_from);
            let mut waiting = edge
                .store
                .conn()
                .prepare("SELECT 1 FROM reconcile_queue WHERE fact = ?1")?;
            let mut rivals = Vec::with_capacity(RECONCILE_K);
            for (rival, cosine) in search::similar(edge.store, &fact, model, usize::MAX, at, now)? {
                if rivals.len() == RECONCILE_K {
                    break;
                }
                if !waiting.exists([&rival.0])? {
                    rivals.push((rival, cosine));
                }
            }
            drop(waiting);
            let norm = normalise(&fact.text);
            for (rival, cosine) in rivals {
                if cosine.is_none_or(|c| c < config.reconcile_min_cosine) {
                    continue;
                }
                let differs = edge
                    .store
                    .fact(&rival)?
                    .is_some_and(|r| normalise(&r.text) != norm);
                if differs {
                    let item = edge.record(
                        "queued",
                        Some(c7::DigestReason::Contradiction.as_str()),
                        Some(&candidate),
                        Some(&rival.0),
                        now,
                    )?;
                    edge.store.conn().execute(
                        "UPDATE edge_proposal SET fact_id = ?2 WHERE id = ?1",
                        params![row_id(&item), id.0],
                    )?;
                    break;
                }
            }
            Ok(())
        })
    }

    fn reject(&mut self, reason: Rejection, now: UnixTime) -> Result<Outcome, EdgeError> {
        let id = self.record("rejected", Some(reason.wire().as_str()), None, None, now)?;
        Ok(Outcome::Rejected {
            proposal: id,
            reason,
        })
    }

    /// Queues `c` as `ambiguous_entity` between `found`, unless it would be refused whichever
    /// of them the owner picks.
    fn queue_ambiguous(
        &mut self,
        c: &c7::ProposeRequest,
        found: &[Entity],
        now: UnixTime,
    ) -> Result<Outcome, EdgeError> {
        let (_, span) = self.reconcile(None, c, now, false)?;
        if let Some(reason) = self.preflight(c, found.first(), span, now) {
            return self.reject(reason, now);
        }
        self.queue(c, c7::DigestReason::AmbiguousEntity, None, found, now)
    }

    /// Open digest items, oldest first, at most `limit`.
    ///
    /// # Errors
    ///
    /// Failures of the edge table or the store.
    pub fn digest(&self, limit: usize) -> Result<Vec<c7::DigestItem>, EdgeError> {
        let mut stmt = self.store.conn().prepare(
            "SELECT id, reason, candidate, conflicts_with, created_at, entity_candidates
             FROM edge_proposal WHERE status = 'queued' ORDER BY id LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut items = Vec::with_capacity(rows.len());
        for (id, reason, candidate, conflicts_with, created_at, named) in rows {
            let pid = proposal_name(id);
            let reason = match reason.as_deref() {
                Some("contradiction") => c7::DigestReason::Contradiction,
                Some("low_confidence") => c7::DigestReason::LowConfidence,
                Some("ambiguous_entity") => c7::DigestReason::AmbiguousEntity,
                _ => return Err(EdgeError::Corrupt(pid)),
            };
            let entity_candidates = match named {
                Some(ids) => Some(
                    self.named(&ids, &pid)?
                        .into_iter()
                        .map(|e| c7::EntityCandidate {
                            entity: e.id.0,
                            name: e.name,
                            kind: wire_kind(e.kind),
                        })
                        .collect(),
                ),
                None => None,
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
                entity_candidates,
                queued_at: as_f64(created_at),
            });
        }
        Ok(items)
    }

    /// Applies the owner's decision on an open digest item: `accept` writes the candidate
    /// (the store closes the conflicting version), to `entity` for an `ambiguous_entity` item,
    /// `reject` drops it, `replace` writes `replacement` instead. On an item of a fact already
    /// written ([`Edge::reconcile_similar`]) `reject` retracts that fact in favour of its rival,
    /// `accept` the rival in favour of it outside its interval, and `replace` retracts it in
    /// favour of the replacement. A replacement passes the same refusal checks as a proposal; a
    /// refused one leaves the item open. Like a propose, it commits whole or not at all.
    ///
    /// # Errors
    ///
    /// [`EdgeError::NotOpen`] for an unknown or already resolved id,
    /// [`EdgeError::MissingReplacement`] for `replace` without one,
    /// [`EdgeError::MissingEntity`] and [`EdgeError::NotACandidate`] for a missing or wrong
    /// `entity`, and store failures.
    pub fn resolve(
        &mut self,
        proposal_id: &str,
        action: c7::ResolveAction,
        replacement: Option<&c7::ProposeRequest>,
        entity: Option<&str>,
        now: UnixTime,
    ) -> Result<Outcome, EdgeError> {
        let config = self.config;
        self.store.atomically(|store| {
            Edge::new(store, config).settle(proposal_id, action, replacement, entity, now)
        })
    }

    fn settle(
        &mut self,
        proposal_id: &str,
        action: c7::ResolveAction,
        replacement: Option<&c7::ProposeRequest>,
        entity: Option<&str>,
        now: UnixTime,
    ) -> Result<Outcome, EdgeError> {
        let not_open = || EdgeError::NotOpen(proposal_id.to_owned());
        let corrupt = || EdgeError::Corrupt(proposal_id.to_owned());
        let id = row_id(proposal_id);
        let (queued, named, written, rival) = self
            .store
            .conn()
            .query_row(
                "SELECT candidate, entity_candidates, fact_id, conflicts_with FROM edge_proposal
                 WHERE id = ?1 AND status = 'queued'",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?.map(FactId),
                        r.get::<_, Option<String>>(3)?.map(FactId),
                    ))
                },
            )
            .optional()?
            .ok_or_else(not_open)?;
        let queued = queued
            .and_then(|c| serde_json::from_str::<c7::ProposeRequest>(&c).ok())
            .ok_or_else(corrupt)?;
        let named = match named {
            Some(ids) => self.named(&ids, proposal_id)?,
            None => Vec::new(),
        };
        let picked = match (action, entity) {
            (c7::ResolveAction::Accept, Some(e)) => Some(
                named
                    .into_iter()
                    .find(|n| n.id.0 == e)
                    .ok_or_else(|| EdgeError::NotACandidate(e.to_owned()))?,
            ),
            (_, Some(e)) => return Err(EdgeError::NotACandidate(e.to_owned())),
            (c7::ResolveAction::Accept, None) if !named.is_empty() => {
                return Err(EdgeError::MissingEntity);
            }
            (_, None) => None,
        };
        if let Some(written) = &written
            && let Some(outcome) =
                self.settle_written(id, action, written, &rival.ok_or_else(corrupt)?, now)?
        {
            return Ok(outcome);
        }
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
        if let Some(reason) = self.screen(&chosen)? {
            return Err(EdgeError::ReplacementRefused(reason));
        }
        let entity = match picked {
            Some(e) => Some(e),
            None => match self.name(&chosen)? {
                Named::Known(e) => Some(e),
                Named::New => None,
                Named::Ambiguous(_) => {
                    return Err(EdgeError::ReplacementRefused(Rejection::Invalid));
                }
            },
        };
        let (verdict, span) = self.reconcile(entity.as_ref(), &chosen, now, true)?;
        if let Some(reason) = self.preflight(&chosen, entity.as_ref(), span, now) {
            return Err(EdgeError::ReplacementRefused(reason));
        }
        let outcome = match verdict {
            Verdict::Duplicate(fact) => {
                self.close(id, "duplicate", None, Some(&fact), now)?;
                Outcome::Duplicate {
                    proposal: proposal_id.to_owned(),
                    fact,
                }
            }
            // The owner decided: a conflict is what they chose to override.
            Verdict::Accept(restated) | Verdict::Conflict(_, restated) => {
                self.decide(id, proposal_id, entity, &chosen, span, &restated, now)?
            }
        };
        if let (Some(written), Outcome::Accepted { fact, .. } | Outcome::Duplicate { fact, .. }) =
            (&written, &outcome)
            && fact != written
        {
            self.retract(written, fact, false, now)?;
        }
        Ok(outcome)
    }

    /// The part of the owner's decision on item `id`, a contradiction between the written fact
    /// `written` and `rival`, that differs from other items: `accept` retracts the rival and
    /// closes the item; `reject` retracts `written` and `replace` nothing, and both then close
    /// the item as any other.
    fn settle_written(
        &mut self,
        id: i64,
        action: c7::ResolveAction,
        written: &FactId,
        rival: &FactId,
        now: UnixTime,
    ) -> Result<Option<Outcome>, EdgeError> {
        match action {
            c7::ResolveAction::Reject => self.retract(written, rival, false, now)?,
            c7::ResolveAction::Accept => {
                self.retract(rival, written, true, now)?;
                self.close(id, "accepted", None, Some(written), now)?;
                return Ok(Some(Outcome::Accepted {
                    proposal: proposal_name(id),
                    fact: written.clone(),
                }));
            }
            c7::ResolveAction::Replace => {}
        }
        Ok(None)
    }

    /// Retracts `old`, while it is current, at `now` in favour of `by`; with `keep_outside` it
    /// stays believed outside the valid interval of `by`.
    fn retract(
        &mut self,
        old: &FactId,
        by: &FactId,
        keep_outside: bool,
        now: UnixTime,
    ) -> Result<(), EdgeError> {
        let Some(old) = self.store.fact(old)?.filter(|f| f.superseded_at.is_none()) else {
            return Ok(());
        };
        let keep = match self.store.fact(by)? {
            Some(by) if keep_outside => kept_slices(
                &old,
                Span {
                    from: by.valid_from,
                    to: by.valid_to,
                },
                false,
            ),
            _ => Vec::new(),
        };
        self.store.supersede_by(&old.id, now, by, &keep)?;
        Ok(())
    }

    /// Refusal checks that need no history: the reason to refuse `c`, if any.
    fn screen(&self, c: &c7::ProposeRequest) -> Result<Option<Rejection>, EdgeError> {
        if let Some(reason) = refusal(c) {
            return Ok(Some(reason));
        }
        if c.pin.is_some() && (c.secret_ref.is_some() || source_kind(c) != SourceKind::Owner) {
            return Ok(Some(Rejection::Invalid));
        }
        if self.store.ttl_table().rule(ttl_kind(c)).ttl == Ttl::NeverCache {
            return Ok(Some(Rejection::LiveData));
        }
        if let Some(d) = &c.domain
            && self.store.domain(d)?.is_none()
        {
            return Ok(Some(Rejection::Invalid));
        }
        Ok(None)
    }

    /// The entity `c` names: by id (a merged one stands for its survivor), else, unless the
    /// candidate's kind is one a name does not identify ([`EntityKind::is_identity`]), the
    /// entities of its kind (of every identity kind without one) found on the first of these
    /// levels that finds any: an equal name or alias, an equal transliteration
    /// ([`translit::key`]), a transliteration one edit apart ([`MIN_FUZZY_CHARS`]).
    /// A candidate without an entity names none and is [`Named::New`].
    fn name(&self, c: &c7::ProposeRequest) -> Result<Named, EdgeError> {
        let Some(name) = c.entity.as_deref() else {
            return Ok(Named::New);
        };
        if let Some(e) = self.store.survivor(&EntityId(name.to_owned()))? {
            return Ok(Named::Known(e));
        }
        let kind = c.kind.map(entity_kind);
        if kind.is_some_and(|k| !k.is_identity()) {
            return Ok(Named::New);
        }
        let fits = |k: EntityKind| kind.map_or(k.is_identity(), |want| k == want);
        let mut found = self.store.find_entities(name)?;
        found.retain(|e| fits(e.kind));
        if found.is_empty() {
            found = self.near(name, fits)?;
        }
        Ok(match found.len() {
            0 => Named::New,
            1 => Named::Known(found.swap_remove(0)),
            _ => Named::Ambiguous(found),
        })
    }

    /// Entities of a kind that `fits` with a name or alias transliterated as `name` is, else,
    /// when both have [`MIN_FUZZY_CHARS`] characters or more, one edit apart from it; by id.
    fn near(
        &self,
        name: &str,
        fits: impl Fn(EntityKind) -> bool,
    ) -> Result<Vec<Entity>, EdgeError> {
        let want: Vec<char> = translit::key(name).chars().collect();
        if want.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<(EntityId, Vec<char>)> = self
            .store
            .entity_names()?
            .into_iter()
            .filter(|(_, kind, _)| fits(*kind))
            .map(|(id, _, n)| (id, translit::key(&n).chars().collect()))
            .collect();
        let mut ids: Vec<&EntityId> = keys
            .iter()
            .filter(|(_, k)| *k == want)
            .map(|(id, _)| id)
            .collect();
        if ids.is_empty() && want.len() >= MIN_FUZZY_CHARS {
            ids = keys
                .iter()
                .filter(|(_, k)| k.len() >= MIN_FUZZY_CHARS && within_one_edit(&want, k))
                .map(|(id, _)| id)
                .collect();
        }
        ids.dedup();
        ids.into_iter()
            .map(|id| {
                self.store
                    .entity(id)?
                    .ok_or_else(|| EdgeError::Corrupt(id.0.clone()))
            })
            .collect()
    }

    /// The live entities of an `ambiguous_entity` item from its stored ids, `ids_json`: a merged
    /// one as its survivor, each once.
    fn named(&self, ids_json: &str, proposal_id: &str) -> Result<Vec<Entity>, EdgeError> {
        let corrupt = || EdgeError::Corrupt(proposal_id.to_owned());
        let ids: Vec<String> = serde_json::from_str(ids_json).map_err(|_| corrupt())?;
        let mut out: Vec<Entity> = Vec::with_capacity(ids.len());
        for id in ids {
            let e = self.store.survivor(&EntityId(id))?.ok_or_else(corrupt)?;
            if !out.iter().any(|o| o.id == e.id) {
                out.push(e);
            }
        }
        Ok(out)
    }

    /// Places `c` on the valid-time axis, from the time it was said (the seedbed record's
    /// `written_at`, else `now`) when it has no `valid_from`, and weighs it against every
    /// version the garden holds now that it would overlap: versions of the same single-valued
    /// attribute (the ones the store closes) and earlier statements of the same text. The
    /// candidate loses to a version of a higher-ranked source, and to one of an equal source that
    /// starts later; without `owner_decided` the first loss is the conflict.
    fn reconcile(
        &self,
        entity: Option<&Entity>,
        c: &c7::ProposeRequest,
        now: UnixTime,
        owner_decided: bool,
    ) -> Result<(Verdict, Span), EdgeError> {
        let said = self.seed.map_or(now, |s| s.written_at);
        let given = Span {
            from: c.valid_from.map_or(said, secs),
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
        // valid when it was said, else the latest one that starts before the restated end.
        let repeats = |f: &&Fact| {
            same_text(f)
                && (Span {
                    from: said,
                    to: Some(said + 1),
                }
                .overlaps(f)
                    || given.to.is_some_and(|t| f.valid_from < t))
        };
        let from = c
            .valid_from
            .map(secs)
            .or_else(|| known.iter().filter(repeats).map(|f| f.valid_from).max())
            .unwrap_or(said);
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
            let pinned = self.store.pinned_at(&f.id)?.is_some();
            if c.pin.is_none_or(|pin| pin == pinned) {
                return Ok((Verdict::Duplicate(f.id.clone()), span));
            }
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
    /// an `invalid` rejection that keeps nothing of the write.
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
        let written = self
            .store
            .atomically(|store| write(store, entity, c, span, restated, id, now));
        match written {
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
            Err(e) => Err(e),
        }
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
        let id = match (entity, c.entity.as_deref()) {
            (Some(e), _) => e.id.clone(),
            (None, Some(name)) => {
                let id = EntityId(slug(name));
                if new_entity(c, name, id.clone()).validate().is_err() {
                    return Some(Rejection::Invalid);
                }
                id
            }
            (None, None) => return Some(Rejection::Invalid),
        };
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

    /// Queues `c` for the owner; `named` are the entity candidates of an `ambiguous_entity`.
    fn queue(
        &mut self,
        c: &c7::ProposeRequest,
        reason: c7::DigestReason,
        conflicts_with: Option<FactId>,
        named: &[Entity],
        now: UnixTime,
    ) -> Result<Outcome, EdgeError> {
        let json = serde_json::to_string(c).map_err(|_| EdgeError::Corrupt(String::new()))?;
        let id = self.record(
            "queued",
            Some(reason.as_str()),
            Some(&json),
            conflicts_with.as_ref().map(|f| f.0.as_str()),
            now,
        )?;
        if !named.is_empty() {
            let ids: Vec<&str> = named.iter().map(|e| e.id.0.as_str()).collect();
            let ids = serde_json::to_string(&ids).map_err(|_| EdgeError::Corrupt(id.clone()))?;
            self.store.conn().execute(
                "UPDATE edge_proposal SET entity_candidates = ?2 WHERE id = ?1",
                params![row_id(&id), ids],
            )?;
        }
        Ok(Outcome::Queued {
            proposal: id,
            reason,
            conflicts_with,
        })
    }

    /// Inserts a proposal row, naming the seedbed record being processed, and returns its name;
    /// `link` is the conflicting fact of a queued row.
    fn record(
        &mut self,
        status: &str,
        reason: Option<&str>,
        candidate: Option<&str>,
        link: Option<&str>,
        now: UnixTime,
    ) -> Result<String, EdgeError> {
        self.store.conn().execute(
            "INSERT INTO edge_proposal
                 (status, reason, candidate, conflicts_with, created_at, resolved_at, seed)
             VALUES (?1, ?2, ?3, ?4, ?5, CASE WHEN ?1 = 'queued' THEN NULL ELSE ?5 END, ?6)",
            params![
                status,
                reason,
                candidate,
                link,
                now,
                self.seed.map(|s| s.seq)
            ],
        )?;
        Ok(proposal_name(self.store.conn().last_insert_rowid()))
    }

    /// Marks row `id` decided; a decided row no longer holds its candidate or entity candidates.
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
             fact_id = ?4, candidate = NULL, entity_candidates = NULL, resolved_at = ?5
             WHERE id = ?1",
            params![id, status, reason, fact.map(|f| f.0.as_str()), now],
        )?;
        Ok(())
    }
}

/// The C7 proof count of fact `id`: 1 plus the `duplicate` proposals that named it or a
/// version a merge of entities copied into it.
///
/// # Errors
///
/// Failures of the store or of the edge table.
pub fn proof_count(store: &Store, id: &FactId) -> Result<u64, EdgeError> {
    let n: i64 = store.conn().query_row(PROOFS, [&id.0], |r| r.get(0))?;
    u64::try_from(n).map_err(|_| EdgeError::Corrupt(id.0.clone()))
}

/// Writes `c` over `span` as a new current fact for proposal `pid`; the `restated` versions
/// are retracted in its favour but keep what they say outside `span`, and for a single-valued
/// `attribute` the store closes and slices the other versions it replaces. The fact is pinned
/// with `pin: true`, and without `pin` when it is no secret pointer and closes a pinned version
/// of its entity and attribute, keeping the first pin time of what it closes.
fn write(
    store: &mut Store,
    entity: Option<Entity>,
    c: &c7::ProposeRequest,
    span: Span,
    restated: &[Fact],
    pid: i64,
    now: UnixTime,
) -> Result<FactId, EdgeError> {
    let entity = match entity {
        Some(e) => e.id,
        None => create_entity(store, c)?,
    };
    let fact = build_fact(c, entity, span, pid, now);
    for old in restated {
        let keep = kept_slices(old, span, c.valid_from.is_none());
        store.supersede_by(&old.id, now, &fact.id, &keep)?;
    }
    store.insert_fact(&fact)?;
    let pinned_at = match c.pin {
        Some(true) => Some(store.inherited_pin(&fact.id)?.unwrap_or(now)),
        None if c.secret_ref.is_none() => store.inherited_pin(&fact.id)?,
        Some(false) | None => None,
    };
    if let Some(at) = pinned_at {
        store.pin(&fact.id, at)?;
    }
    Ok(fact.id)
}

fn create_entity(store: &mut Store, c: &c7::ProposeRequest) -> Result<EntityId, EdgeError> {
    let Some(name) = c.entity.as_deref() else {
        return Err(StoreError::Invalid("a candidate without an entity names none".into()).into());
    };
    let base = slug(name);
    let mut id = EntityId(base.clone());
    let mut n = 1;
    while store.entity(&id)?.is_some() {
        n += 1;
        id = EntityId(format!("{base}-{n}"));
    }
    Ok(store.upsert_entity(&new_entity(c, name, id))?)
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

/// The entity the unknown `name` of `c` creates, with `id`.
fn new_entity(c: &c7::ProposeRequest, name: &str, id: EntityId) -> Entity {
    Entity {
        id,
        kind: c.kind.map_or(EntityKind::Item, entity_kind),
        name: name.trim().to_owned(),
        aliases: Vec::new(),
        domain: c.domain.clone(),
        attrs: Attrs::None,
    }
}

/// Whether a write of `b` repeats the candidate `a`: an equal text after [`normalise`] and every
/// other field exactly equal.
fn same_candidate(a: &c7::ProposeRequest, b: &c7::ProposeRequest) -> bool {
    let rest = |c: &c7::ProposeRequest| c7::ProposeRequest {
        fact: String::new(),
        ..c.clone()
    };
    normalise(&a.fact) == normalise(&b.fact) && rest(a) == rest(b)
}

/// The ref `seed:<seq>` of a seedbed record.
pub(crate) fn seed_ref(seq: i64) -> String {
    format!("seed:{seq}")
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
pub(crate) fn secs(t: f64) -> UnixTime {
    t.floor() as UnixTime
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
pub(crate) fn ttl_kind(c: &c7::ProposeRequest) -> TtlKind {
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
        c.entity.as_deref(),
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
        || c.entity.as_deref().is_some_and(blank)
        || blank(&c.source_ref)
        || c.attribute.as_deref().is_some_and(blank)
        || bad_interval
    {
        return Some(Rejection::Invalid);
    }
    None
}

/// Whether `a` and `b` are at most one insertion, deletion or substitution apart.
fn within_one_edit(a: &[char], b: &[char]) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if long.len() - short.len() > 1 {
        return false;
    }
    let same = short.iter().zip(long).take_while(|(x, y)| x == y).count();
    if same == short.len() {
        return true;
    }
    let skip = usize::from(short.len() == long.len());
    short[same + skip..] == long[same + 1..]
}

/// Whether `r` is a vault reference: `vault:` and a path without whitespace.
pub fn is_vault_ref(r: &str) -> bool {
    r.strip_prefix("vault:")
        .is_some_and(|p| !p.is_empty() && !p.chars().any(char::is_whitespace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation() {
        assert_eq!(normalise("  Likes   Tea. "), "likes tea");
        assert_eq!(normalise("likes tea"), normalise("LIKES TEA!"));
    }

    #[test]
    fn edits() {
        let chars = |s: &str| s.chars().collect::<Vec<_>>();
        let near = |a: &str, b: &str| within_one_edit(&chars(a), &chars(b));
        assert!(near("velde", "velda"));
        assert!(near("velde", "veld"));
        assert!(near("velde", "vellde"));
        assert!(near("velde", "velde"));
        assert!(!near("velde", "vedla"));
        assert!(!near("velde", "vel"));
    }

    #[test]
    fn slugs() {
        assert_eq!(slug(" Ada  Example "), "ada-example");
        assert_eq!(slug("!!"), "entity");
    }
}
