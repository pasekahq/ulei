//! In-process C7 facade over the store, retrieval and the edge: `garden_search` (active tier),
//! `garden_pointers` (passive tier), `garden_propose`, `garden_digest`, `garden_resolve`. A
//! search miss is queued in the miss outbox ([`crate::miss`]); a pointers miss is not.
//!
//! The pinned core profile (tier a) is not served here yet.

use std::fmt;

use crate::edge::{Edge, EdgeConfig, EdgeError, proof_count, wire_kind};
use crate::miss::{self, ProducerKey, Queued};
use crate::model::{Fact, FactId, Sensitivity, SourceKind, UnixTime};
use crate::search::is_trivial_prompt;
use crate::store::{Store, StoreError};
use crate::wire::c7;

/// Ranked retrieval of current facts: ids with scores, best first, higher is better.
pub trait Retriever {
    /// Up to `k` facts that hold at (`valid_at`, `known_at`); `private` admits private facts.
    ///
    /// # Errors
    ///
    /// Store failures.
    fn search(
        &self,
        store: &Store,
        q: &str,
        k: usize,
        valid_at: UnixTime,
        known_at: UnixTime,
        private: bool,
    ) -> Result<Vec<(FactId, f32)>, StoreError>;
}

/// Lexical-only retrieval over the store's FTS5 index.
#[derive(Clone, Copy, Debug, Default)]
pub struct Lexical;

impl Retriever for Lexical {
    fn search(
        &self,
        store: &Store,
        q: &str,
        k: usize,
        valid_at: UnixTime,
        known_at: UnixTime,
        private: bool,
    ) -> Result<Vec<(FactId, f32)>, StoreError> {
        store.lexical(q, k, valid_at, known_at, private)
    }
}

/// Tunables of the API.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApiConfig {
    /// Pointers need a score above this; tuned per retriever.
    pub pointer_min_score: f32,
    /// Facts returned by `garden_search` without `k`.
    pub search_k: usize,
    /// The edge's tunables.
    pub edge: EdgeConfig,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            pointer_min_score: 0.0,
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

/// A failure of the garden (not a decision about a proposal).
#[derive(Debug)]
pub enum ApiError {
    /// The store failed.
    Store(StoreError),
    /// The edge failed.
    Edge(EdgeError),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Edge(e) => write!(f, "{e}"),
        }
    }
}

impl ApiError {
    /// A short name of the kind of failure, free of any stored or proposed text.
    pub fn class(&self) -> &'static str {
        match self {
            Self::Store(e) => e.class(),
            Self::Edge(e) => e.class(),
        }
    }
}

impl std::error::Error for ApiError {}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
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

/// The garden behind its C7 API.
pub struct Garden<R = Lexical> {
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

    /// Active tier: full facts for `q` with their proof counts, best first, as the garden knew
    /// them at `as_of` (else `now`); a fact of a kind with `recheck_before_acting` carries it.
    /// Secret-pointer and private facts only with `private`. A non-trivial query that finds
    /// nothing is queued as a miss ([`miss::queue`]); a trivial prompt (a greeting, an
    /// acknowledgement, a slash command) gets nothing and no miss.
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
        let hits = self.hits(&req.q, k, req.as_of, req.private, now)?;
        let mut facts = Vec::with_capacity(hits.len());
        let ttl = self.store.ttl_table();
        for (fact, score) in hits {
            let entity = self.store.entity(&fact.entity)?;
            let proofs = proof_count(&self.store, &fact.id)?;
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
                pinned: None,
                proof_count: Some(proofs),
                recheck_before_acting: ttl.rule(fact.ttl).recheck_before_acting.then_some(true),
            });
        }
        let miss = if facts.is_empty() && !is_trivial_prompt(&req.q) {
            Some(miss::queue(&mut self.store, &self.miss_key, &req.q, now)?)
        } else {
            None
        };
        Ok(Answer {
            response: c7::SearchResponse {
                facts,
                lexical_only: None,
            },
            miss,
        })
    }

    /// Passive tier: at most three one-line pointers to facts scoring above the threshold;
    /// never fact text. Finding nothing is no miss; a trivial prompt gets nothing.
    ///
    /// # Errors
    ///
    /// Store failures.
    pub fn garden_pointers(
        &self,
        req: &c7::PointersRequest,
        now: UnixTime,
    ) -> Result<c7::PointersResponse, ApiError> {
        let k = req
            .k
            .map_or(MAX_POINTERS, |k| usize::try_from(k).unwrap_or(MAX_POINTERS))
            .clamp(1, MAX_POINTERS);
        let hits = self.hits(&req.q, k, req.as_of, req.private, now)?;
        let mut pointers = Vec::with_capacity(hits.len());
        for (fact, score) in hits {
            if score <= self.config.pointer_min_score {
                continue;
            }
            let Some(entity) = self.store.entity(&fact.entity)? else {
                continue;
            };
            let kind = wire_kind(entity.kind);
            pointers.push(c7::Pointer {
                hint: format!("there is a fact about {} ({})", entity.name, kind.as_str()),
                fact_id: fact.id.0,
                entity: entity.name,
                kind,
                score: f64::from(score),
            });
        }
        Ok(c7::PointersResponse {
            pointers,
            lexical_only: None,
        })
    }

    /// Hands a candidate to the edge.
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
        Ok(edge.propose(req, now)?.to_wire())
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

    /// Facts for `q` with their scores, visible to the caller, best first.
    fn hits(
        &self,
        q: &str,
        k: usize,
        as_of: Option<f64>,
        private: Option<bool>,
        now: UnixTime,
    ) -> Result<Vec<(Fact, f32)>, ApiError> {
        if is_trivial_prompt(q) {
            return Ok(Vec::new());
        }
        #[allow(clippy::cast_possible_truncation)]
        let at = as_of.map_or(now, |t| t.floor() as UnixTime);
        let private = private.unwrap_or(false);
        let ids = self.retriever.search(&self.store, q, k, at, at, private)?;
        let mut out = Vec::with_capacity(ids.len());
        for (id, score) in ids {
            let Some(fact) = self.store.fact(&id)? else {
                continue;
            };
            let visible = match fact.sensitivity {
                Sensitivity::Public => true,
                Sensitivity::Private | Sensitivity::Secret { .. } => private,
            };
            if visible && fact.holds(at, at) {
                out.push((fact, score));
            }
        }
        out.truncate(k);
        Ok(out)
    }
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
