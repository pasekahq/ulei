//! In-process C7 facade over the store, retrieval and the edge: `garden_search` (active tier),
//! `garden_pointers` (passive tier), `garden_propose` (the seedbed write), `garden_digest`,
//! `garden_resolve`, the night stage's `garden_seedbed_take` and `garden_seedbed_done`, and the
//! owner's `garden_forget`. Search and pointers answer with facts and with the seedbed records
//! not yet processed and not forgotten, marked `unprocessed`. A search miss is queued in the
//! miss outbox ([`crate::miss`]); a pointers miss is not. The pinned core profile (tier a) is
//! a file, rendered by [`crate::profile`].

use std::fmt;
use std::sync::Arc;

use crate::edge::{Edge, EdgeConfig, EdgeError, proof_count, seed_ref, wire_kind};
use crate::embed::Embedder;
use crate::embed::supervisor::Supervisor;
use crate::miss::{self, ProducerKey, Queued};
use crate::model::{EntityKind, Fact, Sensitivity, SourceKind, UnixTime};
use crate::rerank::Reranker;
use crate::search::{self, PointerGate, SearchError, SearchParams, Searcher, is_trivial_prompt};
use crate::store::{ForgetRef, Indexed, SeedRecord, Store, StoreError};
use crate::wire::c7;

/// What a retriever found, and whether it comes from the lexical side alone because the dense
/// side is down or failed this query.
#[derive(Clone, Debug, PartialEq)]
pub struct Found<T> {
    /// The hits.
    pub hits: T,
    /// True when the dense side took no part.
    pub lexical_only: bool,
}

/// Ranked retrieval of current facts and seedbed records that hold at (`valid_at`, `known_at`);
/// `private` admits private ones.
pub trait Retriever {
    /// Up to `k` facts and records with their scores, best first, higher is better.
    ///
    /// # Errors
    ///
    /// [`SearchError`] when the store or the reranker fails.
    fn search(
        &self,
        store: &Store,
        q: &str,
        k: usize,
        valid_at: UnixTime,
        known_at: UnixTime,
        private: bool,
    ) -> Result<Found<Vec<(Indexed, f32)>>, SearchError>;

    /// Up to `gate.n` passive pointers that pass `gate`, best first ([`search::pointers`]).
    ///
    /// # Errors
    ///
    /// [`SearchError`] when the store or the reranker fails.
    fn pointers(
        &self,
        store: &Store,
        q: &str,
        gate: &PointerGate,
        valid_at: UnixTime,
        known_at: UnixTime,
        private: bool,
    ) -> Result<Found<Vec<search::Pointer>>, SearchError>;
}

/// The hybrid retriever of [`search`] with the default [`SearchParams`]: RRF of the lexical and,
/// with an embedder, the dense candidates, reordered by a reranker.
pub struct Hybrid {
    dense: Dense,
    reranker: Box<dyn Reranker>,
}

enum Dense {
    Off,
    InProcess(Box<dyn Embedder>),
    Helper(Arc<Supervisor>),
}

impl Hybrid {
    /// Over the in-process `embedder`, lexical only when it is `None`.
    pub fn new(embedder: Option<Box<dyn Embedder>>, reranker: Box<dyn Reranker>) -> Self {
        let dense = embedder.map_or(Dense::Off, Dense::InProcess);
        Self { dense, reranker }
    }

    /// Over the daemon's helper and its resident matrix: lexical only, and marked so, while it
    /// is down or when it fails a query, which never fails the search.
    pub fn with_helper(helper: Arc<Supervisor>, reranker: Box<dyn Reranker>) -> Self {
        Self {
            dense: Dense::Helper(helper),
            reranker,
        }
    }

    /// The `health` state of the dense side, and why it is down.
    pub fn dense_state(&self) -> (c7::DenseState, Option<String>) {
        match &self.dense {
            Dense::Off => (c7::DenseState::Off, None),
            Dense::InProcess(_) => (c7::DenseState::Ready, None),
            Dense::Helper(helper) => match helper.helper() {
                Ok(_) => (c7::DenseState::Ready, None),
                Err(reason) => (c7::DenseState::Down, Some(reason)),
            },
        }
    }

    /// Records a query arriving at `now`, which the worker's passages give way to.
    pub fn queried(&self, now: UnixTime) {
        if let Dense::Helper(helper) = &self.dense {
            helper.queried(now);
        }
    }

    /// `run` with the dense side when it is there, the helper's over its resident matrix, else,
    /// or when the helper fails the query, lexical only.
    fn retrieve<T>(
        &self,
        store: &Store,
        run: impl Fn(&Searcher<'_>) -> Result<T, SearchError>,
    ) -> Result<Found<T>, SearchError> {
        let found = |searcher: Searcher<'_>, lexical_only| {
            run(&searcher).map(|hits| Found { hits, lexical_only })
        };
        let reranker = self.reranker.as_ref();
        let lexical = || Searcher::new(store, None, reranker);
        let supervisor = match &self.dense {
            Dense::Off => return found(lexical(), false),
            Dense::InProcess(embedder) => {
                return found(
                    Searcher::new(store, Some(embedder.as_ref()), reranker),
                    false,
                );
            }
            Dense::Helper(supervisor) => supervisor,
        };
        let Ok(helper) = supervisor.helper() else {
            return found(lexical(), true);
        };
        let dense =
            Searcher::new(store, Some(helper.as_ref()), reranker).with_matrix(supervisor.matrix());
        match found(dense, false) {
            Err(SearchError::Embed(_)) => {
                // The look turns the dense side down when this query broke the helper's pipe.
                drop(supervisor.helper());
                found(lexical(), true)
            }
            other => other,
        }
    }
}

fn params(k: usize, valid_at: UnixTime, known_at: UnixTime, private: bool) -> SearchParams {
    SearchParams {
        k,
        valid_at,
        known_at,
        include_private: private,
        ..SearchParams::at(valid_at)
    }
}

impl Retriever for Hybrid {
    fn search(
        &self,
        store: &Store,
        q: &str,
        k: usize,
        valid_at: UnixTime,
        known_at: UnixTime,
        private: bool,
    ) -> Result<Found<Vec<(Indexed, f32)>>, SearchError> {
        let p = params(k, valid_at, known_at, private);
        self.retrieve(store, |searcher| {
            Ok(searcher
                .search(q, &p)?
                .into_iter()
                .map(|h| (h.id, h.score))
                .collect())
        })
    }

    fn pointers(
        &self,
        store: &Store,
        q: &str,
        gate: &PointerGate,
        valid_at: UnixTime,
        known_at: UnixTime,
        private: bool,
    ) -> Result<Found<Vec<search::Pointer>>, SearchError> {
        let p = params(gate.n, valid_at, known_at, private);
        self.retrieve(store, |searcher| search::pointers(searcher, q, &p, gate))
    }
}

/// Tunables of the API.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApiConfig {
    /// The evidence a pointer needs; its `n` is replaced by the request's count.
    pub pointers: PointerGate,
    /// Facts returned by `garden_search` without `k`.
    pub search_k: usize,
    /// The edge's tunables.
    pub edge: EdgeConfig,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            pointers: PointerGate::default(),
            search_k: 5,
            edge: EdgeConfig::default(),
        }
    }
}

/// Pointers returned at most, and without `k`.
pub const MAX_POINTERS: usize = 3;
/// Facts returned by `garden_search` at most.
pub const MAX_SEARCH_K: usize = 50;
const MAX_DIGEST: usize = 200;
/// Seedbed records taken or processed in one call at most.
pub const MAX_SEEDBED_BATCH: usize = 100;
const SEEDBED_TAKE: usize = 50;
/// Refs of one `forget` at most.
pub const MAX_FORGET_REFS: usize = 100;
/// Bytes of a `forget` provenance at most.
const MAX_PROVENANCE: usize = 64;

/// A failure of the garden (not a decision about a proposal).
#[derive(Debug)]
pub enum ApiError {
    /// The store failed.
    Store(StoreError),
    /// The edge failed.
    Edge(EdgeError),
    /// Retrieval failed outside the store: the embedder, the reranker or a search parameter.
    Search(SearchError),
    /// The request breaks its method's input rules; the text says which.
    Invalid(String),
    /// The request names a seedbed record the garden does not hold.
    UnknownSeed(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Edge(e) => write!(f, "{e}"),
            Self::Search(e) => write!(f, "{e}"),
            Self::Invalid(e) => f.write_str(e),
            Self::UnknownSeed(r) => write!(f, "no seedbed record {r:?}"),
        }
    }
}

impl ApiError {
    /// A short name of the kind of failure, free of any stored or proposed text.
    pub fn class(&self) -> &'static str {
        match self {
            Self::Store(e) | Self::Search(SearchError::Store(e)) => e.class(),
            Self::Edge(e) => e.class(),
            Self::Search(SearchError::Embed(_)) => "embed",
            Self::Search(SearchError::Invalid(_)) => "invalid_search",
            Self::Invalid(_) => "invalid_request",
            Self::UnknownSeed(_) => "unknown_seed",
        }
    }
}

impl std::error::Error for ApiError {}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<SearchError> for ApiError {
    fn from(e: SearchError) -> Self {
        match e {
            SearchError::Store(e) => Self::Store(e),
            e => Self::Search(e),
        }
    }
}

impl From<EdgeError> for ApiError {
    fn from(e: EdgeError) -> Self {
        Self::Edge(e)
    }
}

/// A search answer and what its miss did to the outbox.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer<T> {
    /// The C7 response.
    pub response: T,
    /// Present when the response is empty for a non-trivial query.
    pub miss: Option<Queued>,
}

/// A hit of the active search: a fact, or a seedbed record not yet processed.
enum Hit {
    Fact(Fact),
    Seed(SeedRecord),
}

/// The garden behind its C7 API.
pub struct Garden<R = Hybrid> {
    store: Store,
    retriever: R,
    config: ApiConfig,
    miss_key: ProducerKey,
}

impl<R: Retriever> Garden<R> {
    /// The garden over `store`, retrieving with `retriever`; search misses are keyed with
    /// `miss_key` ([`miss::queue`]).
    pub fn new(store: Store, retriever: R, config: ApiConfig, miss_key: ProducerKey) -> Self {
        Self {
            store,
            retriever,
            config,
            miss_key,
        }
    }

    /// The store, for seeding domains and entities and for maintenance.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The store, mutable.
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// The retriever.
    pub fn retriever(&self) -> &R {
        &self.retriever
    }

    /// Active tier: full facts for `q` with their proof counts, best first, as the garden knew
    /// them at `as_of` (else `now`); a pinned fact and a fact of a kind with
    /// `recheck_before_acting` carry the flag. A seedbed record is a fact `seed:<seq>` marked
    /// `unprocessed`, and `disputed` with `conflicts_with` when it disputes a current fact.
    /// Secret-pointer and private facts only with `private`. A non-trivial query that finds
    /// neither facts nor records is queued as a miss ([`miss::queue`]) unless `no_miss` is set;
    /// a trivial prompt (a greeting, an acknowledgement, a slash command) gets nothing and no
    /// miss.
    ///
    /// # Errors
    ///
    /// Failures of the store or the edge table.
    pub fn garden_search(
        &mut self,
        req: &c7::SearchRequest,
        now: UnixTime,
    ) -> Result<Answer<c7::SearchResponse>, ApiError> {
        let k = req
            .k
            .map_or(self.config.search_k, |k| {
                usize::try_from(k).unwrap_or(MAX_SEARCH_K)
            })
            .clamp(1, MAX_SEARCH_K);
        let found = self.hits(&req.q, k, req.as_of, req.private, now)?;
        let mut facts = Vec::with_capacity(found.hits.len());
        let ttl = self.store.ttl_table();
        for (hit, score) in found.hits {
            let fact = match hit {
                Hit::Fact(fact) => fact,
                Hit::Seed(r) => {
                    facts.push(record_fact(r, score));
                    continue;
                }
            };
            let entity = self.store.entity(&fact.entity)?;
            let proofs = proof_count(&self.store, &fact.id)?;
            let pinned = self.store.pinned_at(&fact.id)?.map(|_| true);
            facts.push(c7::Fact {
                fact: fact.text,
                entity: fact.entity.0,
                valid_from: as_f64(fact.valid_from),
                valid_to: fact.valid_to.map(as_f64),
                source_ref: fact.provenance.source_ref,
                score: f64::from(score.max(0.0)),
                fact_id: Some(fact.id.0),
                kind: entity.map(|e| wire_kind(e.kind)),
                domain: fact.domain,
                source_kind: Some(wire_source(fact.provenance.kind)),
                expires_at: fact.expires_at.map(as_f64),
                pinned,
                proof_count: Some(proofs),
                recheck_before_acting: ttl.rule(fact.ttl).recheck_before_acting.then_some(true),
                unprocessed: None,
                disputed: None,
                conflicts_with: None,
            });
        }
        let miss = if facts.is_empty() && req.no_miss != Some(true) && !is_trivial_prompt(&req.q) {
            Some(miss::queue(&mut self.store, &self.miss_key, &req.q, now)?)
        } else {
            None
        };
        Ok(Answer {
            response: c7::SearchResponse {
                facts,
                lexical_only: found.lexical_only.then_some(true),
            },
            miss,
        })
    }

    /// Passive tier: at most three one-line pointers to facts and seedbed records that pass the
    /// [`PointerGate`] of the config, one per entity; never their text. Finding nothing is no
    /// miss; a trivial prompt gets nothing.
    ///
    /// # Errors
    ///
    /// Failures of the store or the retriever.
    pub fn garden_pointers(
        &self,
        req: &c7::PointersRequest,
        now: UnixTime,
    ) -> Result<c7::PointersResponse, ApiError> {
        let n = req
            .k
            .map_or(MAX_POINTERS, |k| usize::try_from(k).unwrap_or(MAX_POINTERS))
            .clamp(1, MAX_POINTERS);
        let gate = PointerGate {
            n,
            ..self.config.pointers
        };
        let at = at(req.as_of, now);
        let found = self.retriever.pointers(
            &self.store,
            &req.q,
            &gate,
            at,
            at,
            req.private.unwrap_or(false),
        )?;
        let mut pointers = Vec::with_capacity(found.hits.len());
        for p in found.hits {
            let score = f64::from(p.score);
            let seq = match p.id {
                Indexed::Fact(id) => {
                    let Some(fact) = self.store.fact(&id)? else {
                        continue;
                    };
                    let Some(entity) = self.store.entity(&fact.entity)? else {
                        continue;
                    };
                    let kind = wire_kind(entity.kind);
                    pointers.push(c7::Pointer {
                        hint: format!("there is a fact about {} ({})", entity.name, kind.as_str()),
                        fact_id: fact.id.0,
                        entity: entity.name,
                        kind,
                        score,
                        unprocessed: None,
                        disputed: None,
                    });
                    continue;
                }
                Indexed::Seed(seq) => seq,
            };
            let r = self.record(seq)?;
            let resolved = r
                .entity
                .as_ref()
                .map(|e| self.store.entity(e))
                .transpose()?
                .flatten();
            let Some(name) = resolved
                .as_ref()
                .map(|e| e.name.clone())
                .or_else(|| r.candidate.entity.clone())
            else {
                continue;
            };
            let kind = r.candidate.kind.unwrap_or_else(|| {
                wire_kind(resolved.as_ref().map_or(EntityKind::Item, |e| e.kind))
            });
            let disputed = r.conflicts_with.is_some();
            let flag = if disputed { ", disputed" } else { "" };
            pointers.push(c7::Pointer {
                hint: format!(
                    "there is an unprocessed record about {name} ({}{flag})",
                    kind.as_str()
                ),
                fact_id: seed_ref(seq),
                entity: name,
                kind,
                score,
                unprocessed: Some(true),
                disputed: disputed.then_some(true),
            });
        }
        Ok(c7::PointersResponse {
            pointers,
            lexical_only: found.lexical_only.then_some(true),
        })
    }

    /// Writes a candidate into the seedbed ([`Edge::seed`]): `seeded` with the record's ref and
    /// the fact it disputes, or `rejected` by the edge's screen.
    ///
    /// # Errors
    ///
    /// Failures of the store or the edge tables; refusals are answers.
    pub fn garden_propose(
        &mut self,
        req: &c7::ProposeRequest,
        now: UnixTime,
    ) -> Result<c7::ProposeResponse, ApiError> {
        let mut edge = Edge::new(&mut self.store, self.config.edge);
        Ok(edge.seed(req, now)?.to_wire())
    }

    /// Open digest items, oldest first.
    ///
    /// # Errors
    ///
    /// Failures of the store or the edge tables.
    pub fn garden_digest(
        &mut self,
        req: &c7::DigestRequest,
    ) -> Result<c7::DigestResponse, ApiError> {
        let limit = req
            .limit
            .map_or(MAX_DIGEST, |l| usize::try_from(l).unwrap_or(MAX_DIGEST))
            .clamp(1, MAX_DIGEST);
        let edge = Edge::new(&mut self.store, self.config.edge);
        Ok(c7::DigestResponse {
            items: edge.digest(limit)?,
        })
    }

    /// Applies the owner's decision on a digest item.
    ///
    /// # Errors
    ///
    /// An unknown or closed item, `replace` without a replacement, a missing or wrong
    /// `entity`, store failures.
    pub fn garden_resolve(
        &mut self,
        req: &c7::ResolveRequest,
        now: UnixTime,
    ) -> Result<c7::ProposeResponse, ApiError> {
        let mut edge = Edge::new(&mut self.store, self.config.edge);
        Ok(edge
            .resolve(
                &req.proposal_id,
                req.action,
                req.replacement.as_ref(),
                req.entity.as_deref(),
                now,
            )?
            .to_wire())
    }

    /// Night stage: the first `limit` (default 50, at most 100) unprocessed seedbed records not
    /// forgotten written at or before `until` (else `now`), oldest first. Reads only.
    ///
    /// # Errors
    ///
    /// Failures of the store.
    pub fn garden_seedbed_take(
        &self,
        req: &c7::SeedbedTakeRequest,
        now: UnixTime,
    ) -> Result<c7::SeedbedTakeResponse, ApiError> {
        let limit = req
            .limit
            .map_or(SEEDBED_TAKE, |l| {
                usize::try_from(l).unwrap_or(MAX_SEEDBED_BATCH)
            })
            .clamp(1, MAX_SEEDBED_BATCH);
        let records = self
            .store
            .unprocessed_seeds(at(req.until, now), limit)?
            .into_iter()
            .map(|r| c7::SeedRecord {
                r#ref: seed_ref(r.seq),
                candidate: r.candidate,
                written_at: as_f64(r.written_at),
                conflicts_with: r.conflicts_with.map(|f| f.0),
            })
            .collect();
        Ok(c7::SeedbedTakeResponse { records })
    }

    /// Night stage: hands each item's record to the edge ([`Edge::process_seed`]), each in its
    /// own unit of work, in the request's order.
    ///
    /// # Errors
    ///
    /// [`ApiError::Invalid`] for no items, more than 100 or a malformed ref, and
    /// [`ApiError::UnknownSeed`] for a ref the garden does not hold or a forgotten record, both
    /// before any item is processed; failures of the store or the edge, which leave the items
    /// before the failing one processed.
    pub fn garden_seedbed_done(
        &mut self,
        req: &c7::SeedbedDoneRequest,
        now: UnixTime,
    ) -> Result<c7::SeedbedDoneResponse, ApiError> {
        if req.items.is_empty() || req.items.len() > MAX_SEEDBED_BATCH {
            return Err(ApiError::Invalid(format!(
                "`items` holds 1 to {MAX_SEEDBED_BATCH} records"
            )));
        }
        let mut seqs = Vec::with_capacity(req.items.len());
        for item in &req.items {
            let seq = seed_seq(&item.r#ref)
                .ok_or_else(|| ApiError::Invalid(format!("{:?} is no seedbed ref", item.r#ref)))?;
            if self.store.seed(seq)?.is_none() || self.store.seed_forgotten(seq)? {
                return Err(ApiError::UnknownSeed(item.r#ref.clone()));
            }
            seqs.push(seq);
        }
        let mut edge = Edge::new(&mut self.store, self.config.edge);
        let mut items = Vec::with_capacity(seqs.len());
        for (item, seq) in req.items.iter().zip(seqs) {
            items.push(c7::SeedbedOutcome {
                r#ref: item.r#ref.clone(),
                outcomes: edge.process_seed(seq, item.candidates.as_deref(), now)?,
            });
        }
        Ok(c7::SeedbedDoneResponse { items })
    }

    /// The owner's forget of `refs`, or with `undo` its undo, at `now` in one unit of work
    /// ([`Store::forget`], [`Store::unforget`]): marks or lifts the seedbed records among them
    /// and queues or unqueues the facts they reach for the night (ADR-0051 section 5).
    ///
    /// # Errors
    ///
    /// [`ApiError::Invalid`] for no refs or more than 100, a ref of another form, an
    /// `archive:<id>` ref, a malformed `provenance` or a missing `reason` without `undo`, and
    /// [`ApiError::UnknownSeed`] for a record the garden does not hold; failures of the store.
    /// Nothing changes then.
    pub fn garden_forget(
        &mut self,
        req: &c7::ForgetRequest,
        now: UnixTime,
    ) -> Result<c7::ForgetResponse, ApiError> {
        if req.refs.is_empty() || req.refs.len() > MAX_FORGET_REFS {
            return Err(ApiError::Invalid(format!(
                "`refs` holds 1 to {MAX_FORGET_REFS} refs"
            )));
        }
        if !is_provenance(&req.provenance) {
            return Err(ApiError::Invalid(format!(
                "{:?} is no provenance s<session>-t<turn>[-n<n>]",
                req.provenance
            )));
        }
        let refs = req
            .refs
            .iter()
            .map(|r| forget_ref(r))
            .collect::<Result<Vec<_>, _>>()?;
        let changed = if req.undo.unwrap_or(false) {
            self.store.unforget(&refs, &req.provenance, now)
        } else {
            let reason = req
                .reason
                .as_deref()
                .filter(|r| !r.is_empty())
                .ok_or_else(|| ApiError::Invalid("`reason` is required without `undo`".into()))?;
            self.store.forget(&refs, reason, &req.provenance, now)
        }
        .map_err(|e| match e {
            StoreError::NotFound(r) => ApiError::UnknownSeed(r),
            e => ApiError::Store(e),
        })?;
        let items = req
            .refs
            .iter()
            .zip(changed)
            .map(|(r, c)| c7::ForgetOutcome {
                r#ref: r.clone(),
                marks: c.marks as u64,
                queue: c.queue as u64,
            })
            .collect();
        Ok(c7::ForgetResponse { items })
    }

    /// The seedbed record `seq` the index found.
    fn record(&self, seq: i64) -> Result<SeedRecord, ApiError> {
        self.store.seed(seq)?.ok_or_else(|| {
            ApiError::Store(StoreError::Corrupt(format!(
                "index row of a missing record {seq}"
            )))
        })
    }

    /// Facts and records for `q` with their scores, visible to the caller, best first.
    fn hits(
        &self,
        q: &str,
        k: usize,
        as_of: Option<f64>,
        private: Option<bool>,
        now: UnixTime,
    ) -> Result<Found<Vec<(Hit, f32)>>, ApiError> {
        if is_trivial_prompt(q) {
            return Ok(Found {
                hits: Vec::new(),
                lexical_only: false,
            });
        }
        let at = at(as_of, now);
        let private = private.unwrap_or(false);
        let found = self.retriever.search(&self.store, q, k, at, at, private)?;
        let mut out = Vec::with_capacity(found.hits.len());
        for (id, score) in found.hits {
            let id = match id {
                Indexed::Fact(id) => id,
                Indexed::Seed(seq) => {
                    out.push((Hit::Seed(self.record(seq)?), score));
                    continue;
                }
            };
            let Some(fact) = self.store.fact(&id)? else {
                continue;
            };
            let visible = match fact.sensitivity {
                Sensitivity::Public => true,
                Sensitivity::Private | Sensitivity::Secret { .. } => private,
            };
            if visible && fact.holds(at, at) {
                out.push((Hit::Fact(fact), score));
            }
        }
        out.truncate(k);
        Ok(Found {
            hits: out,
            lexical_only: found.lexical_only,
        })
    }
}

/// The C7 fact of an unprocessed record: its candidate's fields as written, the resolved entity's
/// id else the name as written, the record's valid interval and expiry, and the dispute.
fn record_fact(r: SeedRecord, score: f32) -> c7::Fact {
    let c = r.candidate;
    let disputed = r.conflicts_with.is_some();
    c7::Fact {
        fact: c.fact,
        entity: r.entity.map(|e| e.0).or(c.entity).unwrap_or_default(),
        valid_from: as_f64(r.valid_from),
        valid_to: r.valid_to.map(as_f64),
        source_ref: c.source_ref,
        score: f64::from(score.max(0.0)),
        fact_id: Some(seed_ref(r.seq)),
        kind: c.kind,
        domain: c.domain,
        source_kind: c.source_kind,
        expires_at: r.expires_at.map(as_f64),
        pinned: None,
        proof_count: None,
        recheck_before_acting: None,
        unprocessed: Some(true),
        disputed: disputed.then_some(true),
        conflicts_with: r.conflicts_with.map(|f| f.0),
    }
}

/// The `seq` of a ref `seed:<seq>`.
fn seed_seq(r: &str) -> Option<i64> {
    positive(r.strip_prefix("seed:")?)
}

/// `digits` as an integer from 1 written without a sign or leading zeros.
fn positive(digits: &str) -> Option<i64> {
    let canonical = digits.bytes().all(|b| b.is_ascii_digit()) && !digits.starts_with('0');
    digits.parse().ok().filter(|_| canonical)
}

/// A ref of `forget`: `seed:<seq>`, `c8:<n>` or `c8:session:<id>`.
fn forget_ref(r: &str) -> Result<ForgetRef, ApiError> {
    if let Some(seq) = seed_seq(r) {
        return Ok(ForgetRef::Seed(seq));
    }
    let journal = r
        .strip_prefix("c8:session:")
        .or_else(|| r.strip_prefix("c8:"));
    if journal.and_then(positive).is_some() {
        return Ok(ForgetRef::Journal(r.to_owned()));
    }
    Err(ApiError::Invalid(if r.starts_with("archive:") {
        format!("{r:?} refused: forget neither marks nor changes the archive")
    } else {
        format!("{r:?} is no seed:<seq>, c8:<n> or c8:session:<id> ref")
    }))
}

/// Whether `p` is a provenance `s<session>-t<turn>` with an optional `-n<n>`.
fn is_provenance(p: &str) -> bool {
    let number =
        |s: Option<&str>| s.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
    let mut parts = p.split('-');
    p.len() <= MAX_PROVENANCE
        && number(parts.next().and_then(|s| s.strip_prefix('s')))
        && number(parts.next().and_then(|s| s.strip_prefix('t')))
        && parts.next().is_none_or(|n| number(n.strip_prefix('n')))
        && parts.next().is_none()
}

/// The time a request asks about: `as_of`, else `now`.
fn at(as_of: Option<f64>, now: UnixTime) -> UnixTime {
    #[allow(clippy::cast_possible_truncation)]
    as_of.map_or(now, |t| t.floor() as UnixTime)
}

fn wire_source(k: SourceKind) -> c7::SourceKind {
    match k {
        SourceKind::Owner => c7::SourceKind::Owner,
        SourceKind::Raw => c7::SourceKind::Raw,
        SourceKind::Internet => c7::SourceKind::Internet,
        SourceKind::Agent => c7::SourceKind::Agent,
    }
}

#[allow(clippy::cast_precision_loss)]
pub(crate) fn as_f64(t: UnixTime) -> f64 {
    t as f64
}
