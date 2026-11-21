//! The second level (ADR-0047 section 5): where the worker looks for an answer to a search
//! miss before it publishes the miss. A source only proposes; its candidates pass the edge
//! like any proposal.

use std::fmt;

use crate::model::UnixTime;
use crate::wire::c7;

/// A place the garden can look up what it does not know.
pub trait Source {
    /// Candidate facts for the query `q` at `now`, each with `source_kind`, `source_ref`,
    /// `observed_at` and `ttl_kind`; empty when nothing is found.
    ///
    /// # Errors
    ///
    /// The source could not be asked or its answer could not be read.
    fn find(&self, q: &str, now: UnixTime) -> Result<Vec<c7::ProposeRequest>, SourceError>;
}

/// A failed lookup, named by its class alone: never the query or the source's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceError {
    class: &'static str,
}

impl SourceError {
    /// A failure of class `class`, a short name such as `unreachable`.
    pub fn new(class: &'static str) -> Self {
        Self { class }
    }

    /// The class of the failure.
    pub fn class(&self) -> &'static str {
        self.class
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "second level failed: {}", self.class)
    }
}

impl std::error::Error for SourceError {}

/// The `none` provider: finds nothing, so every miss is published.
#[derive(Clone, Copy, Debug, Default)]
pub struct Nothing;

impl Source for Nothing {
    fn find(&self, _q: &str, _now: UnixTime) -> Result<Vec<c7::ProposeRequest>, SourceError> {
        Ok(Vec::new())
    }
}

/// A value of `garden.second_level`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// `none`: [`Nothing`].
    None,
}

impl Provider {
    /// The provider named `name`, if this build has it.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// The source of this provider.
    pub fn source(self) -> Box<dyn Source + Send> {
        match self {
            Self::None => Box::new(Nothing),
        }
    }
}
