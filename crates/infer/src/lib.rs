//! The inference stack (ADR-0086): shared lower layers written once per CPU architecture, the
//! models on top as feature-gated modules.
//!
//! - [`kernels`]: the matrix kernels and their run-time dispatch: the portable fp32 matrix
//!   product and int8 linear layer (the reference), and the int8 linear layer per instruction
//!   set (AVX2; NEON compiled, not yet enabled), bit-identical to the reference;
//! - [`weights`]: the tensor containers: a `safetensors` checkpoint read onto the heap and the
//!   engine's own int8 weight file ([`weights::file`], mapped read-only and read in place);
//! - [`ops`]: model-agnostic fp32 operations (GELU, `LayerNorm`, softmax, L2 normalisation);
//! - `bert` (feature `bert`): the BERT / XLM-RoBERTa encoder family of the garden's embedders.
//!
//! A model never reaches past these layers' public functions, so a new kernel or weight backing
//! changes no model.

#[cfg(feature = "bert")]
pub mod bert;
pub mod kernels;
pub mod ops;
pub mod weights;

use std::fmt;

/// An error of the inference stack: a bad config, a bad or incomplete checkpoint, or a bad input.
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
