//! The second level (ADR-0047 sections 5 and 6): where the worker looks for an answer to a
//! search miss before it publishes the miss, and for a new version of an expired fact. A source
//! only proposes; its candidates are written into the seedbed like any proposal.

use std::fmt;

use crate::model::UnixTime;
use crate::wire::c7;

/// A place the garden can look up what it does not know.
pub trait Source {
    /// Candidate facts for the query `q`, a search or the text of an expired fact, at `now`,
    /// each with `source_kind`, `source_ref`, `observed_at` and `ttl_kind`; empty when nothing
    /// is found.
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

/// A value of `garden.second_level`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// `none`: no source, so every miss is published and no recheck is made.
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

    /// The source of this provider, none for `none`.
    pub fn source(self) -> Option<Box<dyn Source + Send>> {
        match self {
            Self::None => None,
        }
    }
}
