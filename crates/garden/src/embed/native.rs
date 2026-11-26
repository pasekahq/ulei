//! The encoder engine of `ulei-infer` (ADR-0086 E6) behind [`Embedder`]: the int8 weight file
//! [`super::ENGINE_FILE`] written by `ulei-garden-embed convert`, mapped read-only, with the
//! tokenizer, pooling and the query and passage prefixes of its header. One text at a time on
//! the calling thread; no thread, timer or file is touched between calls. Queries and passages
//! have an activation workspace ([`Workspace`]) each, so the helper's query lane and indexing
//! lane (ADR-0086 section 9) run at once over the one mapped encoder. The passages' buffers are
//! kept from one call to the next, so a forward pass allocates nothing once they have grown to
//! the longest passage seen, and released after a call that left them above [`KEEP_BYTES`]; the
//! queries' buffers are released after a call that left them above [`QUERY_KEEP_BYTES`]. Both
//! keep the idle helper's dirty memory within ADR-0086 section 6.

use std::path::Path;
use std::sync::Mutex;

use ulei_infer::bert::{Encoder, Prefixes, Workspace};
use ulei_infer::weights::file::WeightFile;

use super::digest::{VerifyError, open_sealed};
use super::{EmbedError, Embedder, lock};
use crate::miss::lower_hex;

/// The most the engine's buffers keep between calls: 16 MiB, under the 20 MB of idle dirty
/// memory of ADR-0086 section 6 with the helper's own few MiB. e5-small's buffers at its 512
/// tokens stay below it; bge-m3's after a long input do not and are released after that call.
pub const KEEP_BYTES: usize = 16 << 20;

/// The most the query lane's buffers keep between calls: 2 MiB, so that with the passages'
/// [`KEEP_BYTES`] and the helper's own memory the idle helper stays under the 20 MB of ADR-0086
/// section 6. The buffers grow linearly with the tokens (about 18.9 KB per token for e5-small,
/// 50.2 KB for bge-m3, section 9): a query of up to about 110 tokens (e5-small) or 41 tokens
/// (bge-m3) keeps them, a longer one allocates them again and gives them back.
pub const QUERY_KEEP_BYTES: usize = 2 << 20;

/// A model of the encoder engine bound to its weight file for the life of the process.
pub struct NativeEmbedder {
    encoder: Encoder<'static>,
    passages: Mutex<Workspace>,
    queries: Mutex<Workspace>,
    prefixes: Prefixes,
    name: String,
    dim: usize,
    sha256: String,
}

impl NativeEmbedder {
    /// Maps the weight file at `path` and binds the encoder to it. Its identity is the content
    /// digest the converter sealed into the file's preamble (ADR-0086 E6c), taken as it is: the
    /// start checks only what reads no blob (the preamble, the file's length against the one it
    /// records, the header and the tensor table), so a changed blob byte is found by
    /// `ulei-garden-embed verify`, not here. The mapping is of the file opened here, so a new
    /// file renamed over `path` meanwhile (the supported way to replace it; writing the file in
    /// place while it is mapped is not) takes effect at the next start. The mapping lives until
    /// the process ends.
    ///
    /// # Errors
    ///
    /// [`EmbedError::Corrupt`] naming the file when it is corrupted (truncated, too long, a
    /// malformed preamble, header or tensor table, no sealed digest); [`EmbedError::Load`]
    /// naming it when it cannot be opened, is not a weight file of the engine's current format
    /// (a file of an older one asks to convert the model again), or holds no BERT model with a
    /// tokenizer.
    pub fn load(path: &Path) -> Result<Self, EmbedError> {
        let named =
            |e: &dyn std::fmt::Display| EmbedError::Load(format!("{}: {e}", path.display()));
        let (_file, weights, digest) = open_sealed(path).map_err(|e| match e {
            VerifyError::Corrupt(m) => EmbedError::Corrupt(format!("{}: {m}", path.display())),
            other => named(&other),
        })?;
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
        let sha256 = lower_hex(&digest);
        Ok(Self {
            encoder,
            passages: Mutex::new(Workspace::new()),
            queries: Mutex::new(Workspace::new()),
            prefixes,
            name,
            dim,
            sha256,
        })
    }

    /// Lowercase hex content digest sealed in the weight file (ADR-0086 E6c), which covers the
    /// model and its tokenizer.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Bytes the engine's buffers hold now, the passages' and the queries' (between calls: at
    /// most [`KEEP_BYTES`] and [`QUERY_KEEP_BYTES`]).
    pub fn workspace_bytes(&self) -> (usize, usize) {
        (lock(&self.passages).bytes(), lock(&self.queries).bytes())
    }

    /// Embeds `texts`, each after `prefix`, in `workspace`, then releases it if it has grown
    /// above `keep` bytes.
    fn embed(
        &self,
        workspace: &Mutex<Workspace>,
        keep: usize,
        prefix: &str,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let run = |e: ulei_infer::Error| EmbedError::Run(e.to_string());
        let mut ws = lock(workspace);
        let vectors = texts
            .iter()
            .map(|text| {
                if prefix.is_empty() {
                    self.encoder.embed_text_with(&mut ws, text)
                } else {
                    self.encoder
                        .embed_text_with(&mut ws, &format!("{prefix}{text}"))
                }
                .map_err(run)
            })
            .collect();
        if ws.bytes() > keep {
            ws.release();
        }
        vectors
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
        let mut v = self.embed(
            &self.queries,
            QUERY_KEEP_BYTES,
            &self.prefixes.query,
            &[text],
        )?;
        v.pop()
            .ok_or_else(|| EmbedError::Run("no vector for the query".into()))
    }

    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.embed(&self.passages, KEEP_BYTES, &self.prefixes.passage, texts)
    }
}
