//! The garden's side of the night's judge (ADR-0096): the facts waiting with their hints for
//! `reconcile_take`, the verdicts `reconcile_done` applies silently and records, the changes
//! `reconcile_review` lists and `reconcile_undo` reverses, the open judged contradictions a
//! `search` marks, and the anonymous counters of `health`. The garden calls no model: the judge
//! runs in `ulei-engine night`.

use std::collections::{HashMap, HashSet};
use std::fmt;

use rusqlite::{Connection, OptionalExtension, params};

use crate::edge::{Edge, EdgeConfig, EdgeError, bump, source_rank};
use crate::model::{Entity, EntityId, Fact, FactId, UnixTime};
use crate::search::Basis;
use crate::store::{Store, StoreError};
use crate::wire::c7;

/// Upper bounds in seconds of the buckets of the time from a fact's recording to its judgement;
/// a last bucket holds the rest (ADR-0096 section 5).
pub const LATENCY_BUCKETS: [UnixTime; 5] = [3_600, 21_600, 86_400, 259_200, 604_800];

/// A fact waiting for the judge with its hints, as [`waiting`] reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct Waiting {
    /// Its place in the queue.
    pub seq: i64,
    /// The new fact.
    pub fact: Fact,
    /// Its entity.
    pub entity: Entity,
    /// Its hints, nearest first; a hint no longer current is left out.
    pub hints: Vec<Fact>,
    /// How they were ranked.
    pub basis: Basis,
    /// The embedder's model of dense hints.
    pub model: Option<String>,
}

/// The other fact of an open judged contradiction and the judge's reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The other fact.
    pub other: FactId,
    /// The judge's one-line reason.
    pub note: Option<String>,
}

/// Why `reconcile_done` or `reconcile_undo` refused a request before changing anything.
#[derive(Debug)]
pub enum Refused {
    /// A fact that neither waits with hints nor was judged, or an unknown change.
    NotFound(String),
    /// The request breaks a rule; the text says which.
    Invalid(String),
    /// The store or the edge failed.
    Edge(EdgeError),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(s) | Self::Invalid(s) => f.write_str(s),
            Self::Edge(e) => write!(f, "{e}"),
        }
    }
}

impl From<EdgeError> for Refused {
    fn from(e: EdgeError) -> Self {
        Self::Edge(e)
    }
}

impl From<StoreError> for Refused {
    fn from(e: StoreError) -> Self {
        Self::Edge(e.into())
    }
}

impl From<rusqlite::Error> for Refused {
    fn from(e: rusqlite::Error) -> Self {
        Self::Edge(e.into())
    }
}

/// Most facts [`hint_lexically`] hints in one `reconcile_take`; the night's next page takes
/// the rest.
pub const LEXICAL_PER_TAKE: usize = 500;

/// Gives lexical hints ([`Edge::hint`] without a model) to the waiting facts that get them at
/// `now` ([`Edge::lexical_waiting`] under `all` and `ready`), at most [`LEXICAL_PER_TAKE`], each
/// in its own unit of work; how many. The night's take runs it, so the daemon's worker does no
/// work for facts the dense side cannot hint, and a garden without an embedder idles as before
/// (ADR-0096 section 1).
///
/// # Errors
///
/// Failures of the store or the edge tables; the facts hinted before stay hinted.
pub fn hint_lexically(
    store: &mut Store,
    config: EdgeConfig,
    all: bool,
    ready: Option<&str>,
    now: UnixTime,
) -> Result<usize, EdgeError> {
    for n in 0..LEXICAL_PER_TAKE {
        let mut edge = Edge::new(store, config);
        let Some(fact) = edge.lexical_waiting(all, ready, now)? else {
            return Ok(n);
        };
        edge.hint(&fact, None, now)?;
    }
    Ok(LEXICAL_PER_TAKE)
}

/// Up to `limit` facts waiting for the judge with their hints after queue place `after`, in
/// queue order. A fact no longer current, or with no hint current any more, is skipped: it is
/// settled by `reconcile_done` as stale. Reads only.
///
/// # Errors
///
/// Failures of the store.
pub fn waiting(store: &Store, after: i64, limit: usize) -> Result<Vec<Waiting>, EdgeError> {
    let rows: Vec<(i64, String)> = store
        .conn()
        .prepare(
            "SELECT rowid, fact FROM reconcile_queue WHERE state = 'hinted' AND rowid > ?1
             ORDER BY rowid LIMIT ?2",
        )?
        .query_map(
            params![after, i64::try_from(limit).unwrap_or(i64::MAX)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (seq, id) in rows {
        let fact = store
            .fact(&FactId(id.clone()))?
            .ok_or_else(|| EdgeError::Corrupt(id.clone()))?;
        let entity = store
            .entity(&fact.entity)?
            .ok_or_else(|| EdgeError::Corrupt(id.clone()))?;
        let (hints, basis, model) = hints_of(store.conn(), &fact.id)?;
        let mut current = Vec::with_capacity(hints.len());
        for h in hints {
            if let Some(f) = store.fact(&h)?.filter(|f| f.superseded_at.is_none()) {
                current.push(f);
            }
        }
        if fact.superseded_at.is_some() || current.is_empty() {
            continue;
        }
        out.push(Waiting {
            seq,
            fact,
            entity,
            hints: current,
            basis,
            model,
        });
    }
    Ok(out)
}

/// The hints of `fact`, best first, with their basis and model.
fn hints_of(
    c: &Connection,
    fact: &FactId,
) -> Result<(Vec<FactId>, Basis, Option<String>), EdgeError> {
    let rows: Vec<(String, String, Option<String>)> = c
        .prepare("SELECT hint, basis, model FROM reconcile_hint WHERE fact = ?1 ORDER BY rank")?
        .query_map([&fact.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let basis = match rows.first().map(|r| r.1.as_str()) {
        Some("dense") => Basis::Dense,
        _ => Basis::Lexical,
    };
    let model = rows.first().and_then(|r| r.2.clone());
    Ok((
        rows.into_iter().map(|r| FactId(r.0)).collect(),
        basis,
        model,
    ))
}

/// Checks a `reconcile_done` request before any item is applied: every fact waits with hints
/// or was judged, no fact is named twice, and every verdict of a waiting fact names one of its
/// hints, at most once.
///
/// # Errors
///
/// [`Refused`].
pub fn check(store: &Store, items: &[c7::ReconcileJudged]) -> Result<(), Refused> {
    let mut seen = HashSet::new();
    for item in items {
        if !seen.insert(item.fact_id.as_str()) {
            return Err(Refused::Invalid(format!(
                "fact {:?} is named twice",
                item.fact_id
            )));
        }
        let id = FactId(item.fact_id.clone());
        if judged(store.conn(), &id)? {
            continue;
        }
        let hinted: bool = store
            .conn()
            .prepare("SELECT 1 FROM reconcile_queue WHERE fact = ?1 AND state = 'hinted'")?
            .exists([&id.0])?;
        if !hinted {
            return Err(Refused::NotFound(format!(
                "fact {:?} neither waits with hints nor was judged",
                item.fact_id
            )));
        }
        let (hints, ..) = hints_of(store.conn(), &id)?;
        let mut named = HashSet::new();
        for v in &item.verdicts {
            if !hints.iter().any(|h| h.0 == v.hint) || !named.insert(v.hint.as_str()) {
                return Err(Refused::Invalid(format!(
                    "{:?} is not a hint of fact {:?}, or has two verdicts",
                    v.hint, item.fact_id
                )));
            }
        }
    }
    Ok(())
}

fn judged(c: &Connection, id: &FactId) -> Result<bool, rusqlite::Error> {
    c.prepare("SELECT 1 FROM reconcile_judged WHERE fact = ?1")?
        .exists([&id.0])
}

/// The outcomes stored for the judged fact `id`, in the order they were applied.
fn stored(c: &Connection, id: &FactId) -> Result<c7::ReconcileDoneItem, EdgeError> {
    let rows: Vec<(String, String, String, i64)> = c
        .prepare(
            "SELECT hint, relation, applied, id FROM reconcile_judgement WHERE fact = ?1
             ORDER BY id",
        )?
        .query_map([&id.0], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut outcomes = Vec::with_capacity(rows.len());
    for (hint, relation, applied, change) in rows {
        let relation = relation_of(&relation).ok_or_else(|| EdgeError::Corrupt(id.0.clone()))?;
        let applied = applied_of(&applied).ok_or_else(|| EdgeError::Corrupt(id.0.clone()))?;
        outcomes.push(c7::ReconcileOutcome {
            hint,
            relation,
            applied,
            change_id: is_change(applied).then(|| change.unsigned_abs()),
        });
    }
    Ok(c7::ReconcileDoneItem {
        fact_id: id.0.clone(),
        outcomes,
    })
}

fn relation_of(s: &str) -> Option<c7::ReconcileRelation> {
    [
        c7::ReconcileRelation::Duplicate,
        c7::ReconcileRelation::Supersedes,
        c7::ReconcileRelation::Contradiction,
    ]
    .into_iter()
    .find(|r| r.as_str() == s)
}

fn applied_of(s: &str) -> Option<c7::ReconcileApplied> {
    [
        c7::ReconcileApplied::Merged,
        c7::ReconcileApplied::Superseded,
        c7::ReconcileApplied::Contradiction,
        c7::ReconcileApplied::NotApplied,
        c7::ReconcileApplied::Stale,
    ]
    .into_iter()
    .find(|r| r.as_str() == s)
}

fn is_change(a: c7::ReconcileApplied) -> bool {
    matches!(
        a,
        c7::ReconcileApplied::Merged
            | c7::ReconcileApplied::Superseded
            | c7::ReconcileApplied::Contradiction
    )
}

/// Precedence of a relation inside one item: contradictions, then supersedes, then duplicates.
fn precedence(r: c7::ReconcileRelation) -> u8 {
    match r {
        c7::ReconcileRelation::Contradiction => 0,
        c7::ReconcileRelation::Supersedes => 1,
        c7::ReconcileRelation::Duplicate => 2,
    }
}

/// Applies the judge `judge`'s verdicts on one fact at `now` in one unit of work (ADR-0096
/// section 2), after [`check`]: every contradiction is recorded as an open digest item; only
/// without one are the supersedes applied by the source-rank rule (one the rule forbids is
/// recorded as a contradiction); only without either is the first duplicate, by hint rank,
/// applied, which retracts the new fact in favour of the earlier one and adds a proof to it. A
/// verdict whose hint or new fact is no longer current is `stale`, and one another verdict took
/// precedence over `not_applied`. Every verdict is a row of `reconcile_judgement`, the fact
/// leaves the queue, and the counters move. A fact judged before answers its stored outcomes
/// and changes nothing.
///
/// # Errors
///
/// Failures of the store or the edge tables; the item then changes nothing.
pub fn apply(
    store: &mut Store,
    config: EdgeConfig,
    item: &c7::ReconcileJudged,
    judge: &str,
    now: UnixTime,
) -> Result<c7::ReconcileDoneItem, EdgeError> {
    let id = FactId(item.fact_id.clone());
    store.atomically(|store| {
        if judged(store.conn(), &id)? {
            return stored(store.conn(), &id);
        }
        let (hints, basis, model) = hints_of(store.conn(), &id)?;
        let candidate: String = store.conn().query_row(
            "SELECT candidate FROM reconcile_queue WHERE fact = ?1",
            [&id.0],
            |r| r.get(0),
        )?;
        let fact = store
            .fact(&id)?
            .ok_or_else(|| EdgeError::Corrupt(id.0.clone()))?;
        let rank = |h: &str| hints.iter().position(|x| x.0 == h).unwrap_or(usize::MAX);
        let mut verdicts: Vec<&c7::ReconcileVerdict> = item.verdicts.iter().collect();
        verdicts.sort_by_key(|v| (precedence(v.relation), rank(&v.hint)));
        let mut kept: HashSet<String> = HashSet::new();
        for v in &verdicts {
            if kept_pair(store.conn(), &id, &FactId(v.hint.clone()))? {
                kept.insert(v.hint.clone());
            }
        }
        let mut edge = Edge::new(store, config);
        let mut done = Applied::default();
        let mut outcomes = Vec::with_capacity(verdicts.len());
        for v in verdicts {
            let (applied, proposal) = if kept.contains(&v.hint) {
                (c7::ReconcileApplied::NotApplied, None)
            } else {
                done.apply(&mut edge, &fact, &candidate, v, now)?
            };
            let change = record(
                edge.store().conn(),
                &Judged {
                    fact: &fact,
                    judge,
                    basis,
                    model: model.as_deref(),
                    now,
                },
                v,
                applied,
                proposal,
            )?;
            outcomes.push(c7::ReconcileOutcome {
                hint: v.hint.clone(),
                relation: v.relation,
                applied,
                change_id: is_change(applied).then(|| change.unsigned_abs()),
            });
        }
        settle(
            edge.store().conn(),
            &fact,
            judge,
            basis,
            hints.len().saturating_sub(item.verdicts.len()),
            now,
        )?;
        Ok(c7::ReconcileDoneItem {
            fact_id: id.0.clone(),
            outcomes,
        })
    })
}

/// What the verdicts of one fact applied so far.
#[derive(Default)]
struct Applied {
    disputed: bool,
    superseded: bool,
    merged: bool,
}

impl Applied {
    /// Applies verdict `v` on the new `fact` (queued with `candidate`) at `now` after the ones
    /// before it, by the precedence of [`apply`]: what was applied and its proposal row.
    fn apply(
        &mut self,
        edge: &mut Edge<'_>,
        fact: &Fact,
        candidate: &str,
        v: &c7::ReconcileVerdict,
        now: UnixTime,
    ) -> Result<(c7::ReconcileApplied, Option<i64>), EdgeError> {
        let id = &fact.id;
        let hint_id = FactId(v.hint.clone());
        let hint = edge
            .store()
            .fact(&hint_id)?
            .filter(|h| h.superseded_at.is_none());
        let new_current = edge
            .store()
            .fact(id)?
            .is_some_and(|f| f.superseded_at.is_none());
        let Some(hint) = hint.filter(|_| new_current) else {
            return Ok((c7::ReconcileApplied::Stale, None));
        };
        Ok(match v.relation {
            c7::ReconcileRelation::Contradiction => {
                self.disputed = true;
                let row = dispute(edge, candidate, id, &hint_id, now)?;
                (c7::ReconcileApplied::Contradiction, Some(row))
            }
            c7::ReconcileRelation::Supersedes if self.disputed => {
                (c7::ReconcileApplied::NotApplied, None)
            }
            c7::ReconcileRelation::Supersedes => {
                let (new_rank, old_rank) = (
                    source_rank(fact.provenance.kind),
                    source_rank(hint.provenance.kind),
                );
                if new_rank > old_rank
                    || (new_rank == old_rank && fact.valid_from >= hint.valid_from)
                {
                    edge.retract(&hint_id, id, true, now)?;
                    self.superseded = true;
                    (c7::ReconcileApplied::Superseded, None)
                } else {
                    self.disputed = true;
                    let row = dispute(edge, candidate, id, &hint_id, now)?;
                    (c7::ReconcileApplied::Contradiction, Some(row))
                }
            }
            c7::ReconcileRelation::Duplicate if self.disputed || self.superseded || self.merged => {
                (c7::ReconcileApplied::NotApplied, None)
            }
            c7::ReconcileRelation::Duplicate => {
                edge.retract(id, &hint_id, false, now)?;
                self.merged = true;
                let row = proof(edge, &hint_id, &fact.provenance.source_ref, now)?;
                (c7::ReconcileApplied::Merged, Some(row))
            }
        })
    }
}

/// What one judged fact's records share.
struct Judged<'a> {
    fact: &'a Fact,
    judge: &'a str,
    basis: Basis,
    model: Option<&'a str>,
    now: UnixTime,
}

/// Records verdict `v` of `j` with what was `applied` and its proposal row as an audit row and
/// in the counters; the row's id.
fn record(
    conn: &Connection,
    j: &Judged<'_>,
    v: &c7::ReconcileVerdict,
    applied: c7::ReconcileApplied,
    proposal: Option<i64>,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO reconcile_judgement (fact, hint, entity, relation, applied, note,
             judge, basis, embedder, at, proposal)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            j.fact.id.0,
            v.hint,
            j.fact.entity.0,
            v.relation.as_str(),
            applied.as_str(),
            v.note.as_deref().map(|n| clip(n, MAX_NOTE)),
            j.judge,
            j.basis.as_str(),
            j.model,
            j.now,
            proposal
        ],
    )?;
    let change = conn.last_insert_rowid();
    bump(conn, &format!("verdict_{}", v.relation.as_str()), 1)?;
    let counter = match applied {
        c7::ReconcileApplied::Merged => "applied_duplicate",
        c7::ReconcileApplied::Superseded => "applied_supersedes",
        c7::ReconcileApplied::Contradiction => "applied_contradiction",
        c7::ReconcileApplied::Stale => "stale",
        c7::ReconcileApplied::NotApplied => "not_applied",
    };
    bump(conn, counter, 1)?;
    Ok(change)
}

/// Marks `fact` judged by `judge` at `now`, takes it and its hints out of the queue, and counts
/// it with its `compatible` hints and the time it waited.
fn settle(
    conn: &Connection,
    fact: &Fact,
    judge: &str,
    basis: Basis,
    compatible: usize,
    now: UnixTime,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO reconcile_judged (fact, judged_at, judge) VALUES (?1, ?2, ?3)",
        params![fact.id.0, now, judge],
    )?;
    conn.execute("DELETE FROM reconcile_hint WHERE fact = ?1", [&fact.id.0])?;
    conn.execute("DELETE FROM reconcile_queue WHERE fact = ?1", [&fact.id.0])?;
    bump(conn, "judged", 1)?;
    if basis == Basis::Lexical {
        bump(conn, "judged_lexical", 1)?;
    }
    bump(conn, "compatible", compatible)?;
    let waited = now.saturating_sub(fact.recorded_at);
    let bucket = LATENCY_BUCKETS
        .iter()
        .find(|&&le| waited <= le)
        .map_or_else(
            || "latency_more".to_owned(),
            |le| format!("latency_le_{le}"),
        );
    bump(conn, &bucket, 1)
}

/// Characters of a judge's note kept at most.
const MAX_NOTE: usize = 500;

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((at, _)) => s[..at].to_owned(),
        None => s.to_owned(),
    }
}

/// Records the contradiction between the written fact `new` and `earlier` as an open digest
/// item with the fact's candidate; its proposal row.
fn dispute(
    edge: &mut Edge<'_>,
    candidate: &str,
    new: &FactId,
    earlier: &FactId,
    now: UnixTime,
) -> Result<i64, EdgeError> {
    edge.record(
        "queued",
        Some(c7::DigestReason::Contradiction.as_str()),
        Some(candidate),
        Some(&earlier.0),
        now,
    )?;
    let conn = edge.store().conn();
    let row = conn.last_insert_rowid();
    conn.execute(
        "UPDATE edge_proposal SET fact_id = ?2 WHERE id = ?1",
        params![row, new.0],
    )?;
    Ok(row)
}

/// A proof of `earlier` from evidence `source_ref`: a decided `duplicate` proposal row naming it,
/// which [`crate::edge::proof_count`] counts; its row.
fn proof(
    edge: &Edge<'_>,
    earlier: &FactId,
    source_ref: &str,
    now: UnixTime,
) -> Result<i64, EdgeError> {
    let conn = edge.store().conn();
    conn.execute(
        "INSERT INTO edge_proposal (status, fact_id, source_ref, created_at, resolved_at)
         VALUES ('duplicate', ?1, ?2, ?3, ?3)",
        params![earlier.0, source_ref, now],
    )?;
    Ok(conn.last_insert_rowid())
}

fn kept_pair(c: &Connection, a: &FactId, b: &FactId) -> Result<bool, rusqlite::Error> {
    c.prepare("SELECT 1 FROM reconcile_kept WHERE a = ?1 AND b = ?2")?
        .exists(params![a.0.as_str().min(&b.0), a.0.as_str().max(&b.0)])
}

fn keep(c: &Connection, a: &FactId, b: &FactId, now: UnixTime) -> Result<(), rusqlite::Error> {
    if a == b {
        return Ok(());
    }
    c.execute(
        "INSERT OR IGNORE INTO reconcile_kept (a, b, kept_at) VALUES (?1, ?2, ?3)",
        params![a.0.as_str().min(&b.0), a.0.as_str().max(&b.0), now],
    )?;
    Ok(())
}

/// Adds the judge's own counts to the counters.
///
/// # Errors
///
/// SQLite errors.
pub fn add_stats(store: &mut Store, stats: &c7::JudgeStats) -> Result<(), StoreError> {
    let as_usize = |n: Option<u64>| n.map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));
    store.atomically(|store| {
        let c = store.conn();
        for (name, n) in [
            ("judge_calls", stats.calls),
            ("judge_failures", stats.failures),
            ("judge_timeouts", stats.timeouts),
            ("judge_tokens_in", stats.tokens_in),
            ("judge_tokens_out", stats.tokens_out),
        ] {
            bump(c, name, as_usize(n))?;
        }
        Ok(())
    })
}

/// Whether the garden holds any open contradiction the judge recorded: a `search` looks up
/// the conflicts of its hits only then.
///
/// # Errors
///
/// SQLite errors.
pub fn any_open_conflict(store: &Store) -> Result<bool, StoreError> {
    Ok(store
        .conn()
        .prepare(
            "SELECT 1 FROM edge_proposal WHERE status = 'queued' AND reason = 'contradiction'
               AND fact_id IS NOT NULL LIMIT 1",
        )?
        .exists([])?)
}

/// The open contradiction the judge recorded that `id` is part of, the oldest first.
///
/// # Errors
///
/// SQLite errors.
pub fn open_conflict(store: &Store, id: &FactId) -> Result<Option<Conflict>, StoreError> {
    Ok(store
        .conn()
        .query_row(
            "SELECT CASE WHEN p.fact_id = ?1 THEN p.conflicts_with ELSE p.fact_id END, j.note
             FROM edge_proposal p JOIN reconcile_judgement j ON j.proposal = p.id
             WHERE p.status = 'queued' AND p.reason = 'contradiction'
               AND (p.fact_id = ?1 OR p.conflicts_with = ?1)
             ORDER BY p.id LIMIT 1",
            [&id.0],
            |r| {
                Ok(Conflict {
                    other: FactId(r.get(0)?),
                    note: r.get(1)?,
                })
            },
        )
        .optional()?)
}

/// One row of `reconcile_judgement` that is a change.
struct Row {
    id: i64,
    fact: FactId,
    hint: FactId,
    entity: EntityId,
    relation: c7::ReconcileRelation,
    applied: c7::ReconcileApplied,
    note: Option<String>,
    judge: String,
    basis: String,
    embedder: Option<String>,
    at: UnixTime,
    proposal: Option<i64>,
    restored: Option<String>,
    undone_at: Option<UnixTime>,
}

const ROW_COLS: &str = "id, fact, hint, entity, relation, applied, note, judge, basis, embedder, \
    at, proposal, restored, undone_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(Row, String, String)> {
    let relation: String = r.get(4)?;
    let applied: String = r.get(5)?;
    Ok((
        Row {
            id: r.get(0)?,
            fact: FactId(r.get(1)?),
            hint: FactId(r.get(2)?),
            entity: EntityId(r.get(3)?),
            relation: c7::ReconcileRelation::Duplicate,
            applied: c7::ReconcileApplied::Stale,
            note: r.get(6)?,
            judge: r.get(7)?,
            basis: r.get(8)?,
            embedder: r.get(9)?,
            at: r.get(10)?,
            proposal: r.get(11)?,
            restored: r.get(12)?,
            undone_at: r.get(13)?,
        },
        relation,
        applied,
    ))
}

fn decode(raw: (Row, String, String)) -> Result<Row, EdgeError> {
    let (mut row, relation, applied) = raw;
    row.relation = relation_of(&relation).ok_or_else(|| EdgeError::Corrupt(row.fact.0.clone()))?;
    row.applied = applied_of(&applied).ok_or_else(|| EdgeError::Corrupt(row.fact.0.clone()))?;
    Ok(row)
}

/// The change `row` as C7 answers it.
fn change(store: &Store, row: Row) -> Result<c7::ReconcileChange, EdgeError> {
    let text = |id: &FactId| -> Result<c7::ChangeFact, EdgeError> {
        Ok(c7::ChangeFact {
            fact_id: id.0.clone(),
            fact: store.fact(id)?.map(|f| f.text).unwrap_or_default(),
        })
    };
    let status: Option<String> = match row.proposal {
        Some(p) => store
            .conn()
            .query_row("SELECT status FROM edge_proposal WHERE id = ?1", [p], |r| {
                r.get(0)
            })
            .optional()?,
        None => None,
    };
    let state = if row.undone_at.is_some() {
        c7::ChangeState::Undone
    } else if row.applied == c7::ReconcileApplied::Contradiction {
        if status.as_deref() == Some("queued") {
            c7::ChangeState::Open
        } else {
            c7::ChangeState::Resolved
        }
    } else {
        c7::ChangeState::Applied
    };
    #[allow(clippy::cast_precision_loss)]
    let time = |t: UnixTime| t as f64;
    Ok(c7::ReconcileChange {
        change_id: row.id.unsigned_abs(),
        at: time(row.at),
        entity_name: store.entity(&row.entity)?.map(|e| e.name),
        entity: row.entity.0,
        relation: row.relation,
        applied: row.applied,
        state,
        new: text(&row.fact)?,
        earlier: text(&row.hint)?,
        note: row.note,
        judge: row.judge,
        embedder: row.embedder,
        basis: if row.basis == "dense" {
            c7::HintBasis::Dense
        } else {
            c7::HintBasis::Lexical
        },
        proposal_id: (row.applied == c7::ReconcileApplied::Contradiction)
            .then(|| row.proposal.map(|p| format!("p{p}")))
            .flatten(),
        restored: row.restored,
        undone_at: row.undone_at.map(time),
    })
}

/// Default and most changes of one `reconcile_review` page.
pub const REVIEW_PAGE: usize = 20;
/// Most changes of one `reconcile_review` page.
pub const MAX_REVIEW_PAGE: usize = 200;

/// The changes the judge made, newest first, filtered and paged by `req`. Reads only.
///
/// # Errors
///
/// Failures of the store.
pub fn review(
    store: &Store,
    req: &c7::ReconcileReviewRequest,
) -> Result<c7::ReconcileReviewResponse, EdgeError> {
    let limit = req
        .limit
        .map_or(REVIEW_PAGE, |l| {
            usize::try_from(l).unwrap_or(MAX_REVIEW_PAGE)
        })
        .clamp(1, MAX_REVIEW_PAGE);
    let before = req
        .before
        .map_or(i64::MAX, |b| i64::try_from(b).unwrap_or(i64::MAX));
    let sql = format!(
        "SELECT {ROW_COLS} FROM reconcile_judgement j
         WHERE j.applied IN ('merged', 'superseded', 'contradiction') AND j.id < ?1
           AND (?2 IS NULL OR j.entity = ?2) AND (?3 IS NULL OR j.relation = ?3)
           AND (NOT ?4 OR (j.applied = 'contradiction' AND j.undone_at IS NULL
                AND EXISTS (SELECT 1 FROM edge_proposal p
                            WHERE p.id = j.proposal AND p.status = 'queued')))
         ORDER BY j.id DESC LIMIT ?5"
    );
    let raw: Vec<(Row, String, String)> = store
        .conn()
        .prepare(&sql)?
        .query_map(
            params![
                before,
                req.entity,
                req.relation.map(c7::ReconcileRelation::as_str),
                req.open.unwrap_or(false),
                i64::try_from(limit + 1).unwrap_or(i64::MAX)
            ],
            row,
        )?
        .collect::<Result<_, _>>()?;
    let more = raw.len() > limit;
    let mut changes = Vec::with_capacity(limit);
    for r in raw.into_iter().take(limit) {
        changes.push(change(store, decode(r)?)?);
    }
    let next = if more {
        changes.last().map(|c| c.change_id)
    } else {
        None
    };
    Ok(c7::ReconcileReviewResponse { changes, next })
}

/// Reverses the change `change_id` at `now` in one unit of work (ADR-0096 section 4): a merge
/// writes the retracted new fact back as a current copy and withdraws its proof row; a
/// supersede writes the retracted earlier fact back as a current copy and closes the slices of
/// it the retraction kept; a contradiction withdraws its open digest item. The pair, and the
/// copy with the other fact, are then kept apart from the judge. Returns the change as it now
/// stands.
///
/// # Errors
///
/// [`Refused::NotFound`] for an unknown change; [`Refused::Invalid`] for one already undone, a
/// contradiction the owner resolved, or a merge or supersede whose retracted fact was closed
/// again since by something else; failures of the store. Nothing changes then.
pub fn undo(
    store: &mut Store,
    change_id: u64,
    now: UnixTime,
) -> Result<c7::ReconcileChange, Refused> {
    let id = i64::try_from(change_id).unwrap_or(i64::MAX);
    store.atomically(|store| {
        let sql = format!(
            "SELECT {ROW_COLS} FROM reconcile_judgement
             WHERE id = ?1 AND applied IN ('merged', 'superseded', 'contradiction')"
        );
        let raw = store
            .conn()
            .query_row(&sql, [id], row)
            .optional()?
            .ok_or_else(|| Refused::NotFound(format!("no change {change_id}")))?;
        let r = decode(raw)?;
        if r.undone_at.is_some() {
            return Err(Refused::Invalid(format!(
                "change {change_id} is undone already"
            )));
        }
        let cannot = || Refused::Invalid(format!("change {change_id} cannot be undone"));
        let closed_by = |store: &Store, closed: &FactId, by: &FactId| -> Result<bool, Refused> {
            Ok(store.fact(closed)?.is_some_and(|f| {
                f.superseded_at == Some(r.at) && f.superseded_by.as_ref() == Some(by)
            }))
        };
        let mut restored = None;
        match r.applied {
            c7::ReconcileApplied::Merged => {
                if !closed_by(store, &r.fact, &r.hint)? {
                    return Err(cannot());
                }
                let copy = store.restore(&r.fact, now)?;
                if let Some(p) = r.proposal {
                    store.conn().execute(
                        "UPDATE edge_proposal SET status = 'undone' WHERE id = ?1",
                        [p],
                    )?;
                }
                keep(store.conn(), &copy, &r.hint, now)?;
                restored = Some(copy);
            }
            c7::ReconcileApplied::Superseded => {
                if !closed_by(store, &r.hint, &r.fact)? {
                    return Err(cannot());
                }
                let copy = store.restore(&r.hint, now)?;
                let base = format!("{}~{}", r.hint.0, r.at);
                let slices: Vec<String> = store
                    .conn()
                    .prepare(
                        "SELECT id FROM facts WHERE superseded_at IS NULL
                           AND (id = ?1 OR (id > ?1 || '.' AND id < ?1 || '/'))",
                    )?
                    .query_map([&base], |row| row.get(0))?
                    .collect::<Result<_, _>>()?;
                for s in slices {
                    store.supersede_by(&FactId(s), now, &copy, &[])?;
                }
                keep(store.conn(), &copy, &r.fact, now)?;
                restored = Some(copy);
            }
            c7::ReconcileApplied::Contradiction => {
                let open = store.conn().execute(
                    "UPDATE edge_proposal SET status = 'withdrawn', candidate = NULL,
                     entity_candidates = NULL, resolved_at = ?2
                     WHERE id = ?1 AND status = 'queued'",
                    params![r.proposal, now],
                )?;
                if open == 0 {
                    return Err(cannot());
                }
            }
            c7::ReconcileApplied::NotApplied | c7::ReconcileApplied::Stale => {
                return Err(cannot());
            }
        }
        keep(store.conn(), &r.fact, &r.hint, now)?;
        store.conn().execute(
            "UPDATE reconcile_judgement SET undone_at = ?2, restored = ?3 WHERE id = ?1",
            params![id, now, restored.as_ref().map(|f| f.0.as_str())],
        )?;
        bump(store.conn(), &format!("undone_{}", r.relation.as_str()), 1)?;
        let raw = store.conn().query_row(&sql, [id], row)?;
        Ok(change(store, decode(raw)?)?)
    })
}

/// The anonymous counters of the hints and the judge for `health`.
///
/// # Errors
///
/// SQLite errors.
pub fn stats(store: &Store) -> Result<c7::ReconcileStats, StoreError> {
    let c = store.conn();
    let n: HashMap<String, i64> = c
        .prepare("SELECT name, n FROM reconcile_stats")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let get = |name: &str| n.get(name).copied().unwrap_or(0).unsigned_abs();
    let queued = |state: &str| -> Result<u64, StoreError> {
        let k: i64 = c.query_row(
            "SELECT count(*) FROM reconcile_queue WHERE state = ?1",
            [state],
            |r| r.get(0),
        )?;
        Ok(k.unsigned_abs())
    };
    let counts = |prefix: &str| c7::RelationCounts {
        duplicate: get(&format!("{prefix}_duplicate")),
        supersedes: get(&format!("{prefix}_supersedes")),
        contradiction: get(&format!("{prefix}_contradiction")),
    };
    let mut latency: Vec<c7::LatencyBucket> = LATENCY_BUCKETS
        .iter()
        .map(|&le| c7::LatencyBucket {
            le_seconds: Some(le.unsigned_abs()),
            count: get(&format!("latency_le_{le}")),
        })
        .collect();
    latency.push(c7::LatencyBucket {
        le_seconds: None,
        count: get("latency_more"),
    });
    Ok(c7::ReconcileStats {
        waiting: queued("waiting")?,
        hinted: queued("hinted")?,
        hinted_dense: get("hinted_dense"),
        hinted_lexical: get("hinted_lexical"),
        without_hints: get("without_hints"),
        hints_offered: get("hints_offered"),
        judged: get("judged"),
        judged_lexical: get("judged_lexical"),
        verdicts: counts("verdict"),
        compatible: get("compatible"),
        applied: counts("applied"),
        stale: get("stale"),
        not_applied: get("not_applied"),
        undone: counts("undone"),
        judge_calls: get("judge_calls"),
        judge_failures: get("judge_failures"),
        judge_timeouts: get("judge_timeouts"),
        judge_tokens_in: get("judge_tokens_in"),
        judge_tokens_out: get("judge_tokens_out"),
        latency,
    })
}
