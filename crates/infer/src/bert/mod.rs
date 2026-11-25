//! The BERT / XLM-RoBERTa encoder family (cargo feature `bert`): the embedders of the garden
//! (ADR-0087).
//!
//! A model is a [`Config`], read from the checkpoint's `config.json` plus the pooling, and its
//! weights: an fp32 [`crate::weights::Checkpoint`] (E1) or the engine's int8 weight file
//! [`crate::weights::file::WeightFile`] that [`convert::convert`] writes from the checkpoint
//! (E3; the config, pooling and input prefixes then come from the file). [`Encoder`] binds them and runs the
//! embeddings, the post-LayerNorm encoder layers, pooling and L2 normalisation, for one
//! sequence without padding (the attention mask is all ones). With int8 weights every linear
//! layer runs on int8 activations quantised per row and per block (ADR-0087 section 3);
//! `LayerNorm`, softmax, GELU and the attention products stay fp32.
//!
//! Tensor names follow Hugging Face `BertModel` / `XLMRobertaModel`
//! (`embeddings.word_embeddings.weight`, `encoder.layer.N.attention.self.query.weight`, ...),
//! under whatever prefix the checkpoint puts before `embeddings.` (`bert.`, `roberta.`, none).

mod config;
pub mod convert;
mod embeddings;
mod encoder;
mod layer;
mod params;
mod pooling;
mod workspace;

pub use config::{Config, Pooling, Prefixes};
pub use encoder::{Encoder, Forward};
pub use workspace::Workspace;
