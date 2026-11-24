//! Encoder engine for embedders (ADR-0086): one parameterised BERT / XLM-RoBERTa encoder.
//!
//! Milestone E1 is the naive fp32 path: a [`weights::Checkpoint`] read from a `safetensors`
//! file onto the heap, a [`config::Config`] read from the checkpoint's `config.json`, and
//! [`forward::Encoder`], which runs the embeddings, the post-LayerNorm layers, pooling and L2
//! normalisation with the portable operations of [`ops`].

pub mod config;
pub mod forward;
pub mod ops;
pub mod weights;

use std::fmt;

/// An error of the encoder: a bad config, a bad or incomplete checkpoint, or a bad input.
#[derive(Debug)]
pub enum Error {
    /// Reading a file failed.
    Io(std::io::Error),
    /// `config.json` is malformed or describes an unsupported model.
    Config(String),
    /// The weight file is malformed or lacks a tensor the model needs.
    Weights(String),
    /// The token ids cannot be encoded (empty, too long, out of the vocabulary).
    Input(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Config(m) => write!(f, "config: {m}"),
            Self::Weights(m) => write!(f, "weights: {m}"),
            Self::Input(m) => write!(f, "input: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;
