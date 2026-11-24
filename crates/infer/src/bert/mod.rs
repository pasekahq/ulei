//! The BERT / XLM-RoBERTa encoder family (cargo feature `bert`): the embedders of the garden
//! (ADR-0086).
//!
//! A model is a [`Config`], read from the checkpoint's `config.json` plus the pooling, and a
//! [`crate::weights::Checkpoint`]. [`Encoder`] binds the two and runs the embeddings, the
//! post-LayerNorm encoder layers, pooling and L2 normalisation, for one sequence without
//! padding (the attention mask is all ones).
//!
//! Tensor names follow Hugging Face `BertModel` / `XLMRobertaModel`
//! (`embeddings.word_embeddings.weight`, `encoder.layer.N.attention.self.query.weight`, ...),
//! under whatever prefix the checkpoint puts before `embeddings.` (`bert.`, `roberta.`, none).

mod config;
mod embeddings;
mod encoder;
mod layer;
mod params;
mod pooling;

pub use config::{Config, Pooling};
pub use encoder::{Encoder, Forward};
