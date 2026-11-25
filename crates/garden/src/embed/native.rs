//! The encoder engine of `ulei-infer` (ADR-0086 E6) behind [`Embedder`]: the int8 weight file
//! [`super::ENGINE_FILE`] written by `ulei-garden-embed convert`, mapped read-only, with the
//! tokenizer, pooling and the query and passage prefixes of its header. One text at a time on
//! the calling thread; no thread, timer or file is touched between calls.

use std::fs::File;
use std::path::Path;

use ulei_infer::bert::{Encoder, Prefixes};
use ulei_infer::weights::file::WeightFile;

use super::pipe::sha256_of;
use super::{EmbedError, Embedder};

/// A model of the encoder engine bound to its weight file for the life of the process.
pub struct NativeEmbedder {
    encoder: Encoder<'static>,
    prefixes: Prefixes,
    name: String,
    dim: usize,
    sha256: String,
}

impl NativeEmbedder {
    /// Maps the weight file at `path` and binds the encoder to it, then hashes the same open
    /// file: the identity is of the bytes the engine reads, even when a new file is renamed
    /// over `path` meanwhile (the supported way to replace it; writing the file in place while
    /// it is mapped is not). The mapping lives until the process ends.
    ///
    /// # Errors
    ///
    /// [`EmbedError::Load`] naming the file when it cannot be opened, is not a weight file of the
    /// engine's current format (a file of an older one asks to convert the model again), or
    /// holds no BERT model with a tokenizer.
    pub fn load(path: &Path) -> Result<Self, EmbedError> {
        let named =
            |e: &dyn std::fmt::Display| EmbedError::Load(format!("{}: {e}", path.display()));
        let file = File::open(path).map_err(|e| named(&e))?;
        let weights = WeightFile::from_file(&file).map_err(|e| named(&e))?;
        // One model per process: the encoder borrows the mapping until the process exits.
        let weights: &'static WeightFile = Box::leak(Box::new(weights));
        let encoder = Encoder::from_file(weights).map_err(|e| named(&e))?;
        let max_tokens = encoder
            .tokenizer()
            .map_or(0, ulei_infer::text::Tokenizer::max_tokens);
        let config = encoder.config();
        let name = format!(
            "ulei-infer-bert-int8-{}-{max_tokens}",
            config.pooling.name()
        );
        let dim = config.hidden_size;
        let prefixes = encoder.prefixes().clone();
        let sha256 = sha256_of(&file).map_err(|e| named(&e))?;
        Ok(Self {
            encoder,
            prefixes,
            name,
            dim,
            sha256,
        })
    }

    /// Lowercase hex SHA-256 of the weight file as mapped: the model and its tokenizer.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    fn embed(&self, prefix: &str, text: &str) -> Result<Vec<f32>, EmbedError> {
        let run = |e: ulei_infer::Error| EmbedError::Run(e.to_string());
        if prefix.is_empty() {
            self.encoder.embed_text(text).map_err(run)
        } else {
            self.encoder
                .embed_text(&format!("{prefix}{text}"))
                .map_err(run)
        }
    }
}

impl Embedder for NativeEmbedder {
    fn model_id(&self) -> &str {
        &self.name
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.embed(&self.prefixes.query, text)
    }

    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        texts
            .iter()
            .map(|t| self.embed(&self.prefixes.passage, t))
            .collect()
    }
}
