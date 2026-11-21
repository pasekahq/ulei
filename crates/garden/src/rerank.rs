//! The reranker seam (ADR-0047 section 4): it reorders the fused candidates of a search before
//! they are cut to `k` or gated into pointers. This build has only `none`.

use crate::search::{Hit, SearchError};
use crate::store::Store;

/// Reorders the fused hits of one query; it can neither add nor drop a hit.
pub trait Reranker {
    /// Puts `hits`, the fused candidates for `query`, into this reranker's order, best first.
    ///
    /// # Errors
    ///
    /// [`SearchError`] when the store or the reranker's model fails.
    fn rerank(&self, store: &Store, query: &str, hits: &mut [Hit]) -> Result<(), SearchError>;
}

/// `none`: the fused order stays.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoReranker;

impl Reranker for NoReranker {
    fn rerank(&self, _: &Store, _: &str, _: &mut [Hit]) -> Result<(), SearchError> {
        Ok(())
    }
}

/// A value of `garden.reranker`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// `none`: [`NoReranker`].
    None,
}

impl Choice {
    /// The reranker named `name`, if this build has it.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// The reranker of this choice.
    pub fn reranker(self) -> Box<dyn Reranker> {
        match self {
            Self::None => Box::new(NoReranker),
        }
    }
}
