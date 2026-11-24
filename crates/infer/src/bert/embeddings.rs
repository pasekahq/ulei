//! The embedding block: word, position and token type embeddings summed, then `LayerNorm`.

use super::Config;
use super::params::{Norm, Tensors};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub(super) struct Embeddings<'w> {
    word: &'w [f32],
    position: &'w [f32],
    token_type: &'w [f32],
    norm: Norm<'w>,
}

impl<'w> Embeddings<'w> {
    /// Binds the `embeddings.*` tensors, checking their shapes against `c`.
    pub(super) fn bind(t: &Tensors<'w>, c: &Config) -> Result<Self> {
        let h = c.hidden_size;
        Ok(Self {
            word: t.get("embeddings.word_embeddings.weight", &[c.vocab_size, h])?,
            position: t.get(
                "embeddings.position_embeddings.weight",
                &[c.max_positions, h],
            )?,
            token_type: t.get(
                "embeddings.token_type_embeddings.weight",
                &[c.type_vocab_size, h],
            )?,
            norm: t.norm("embeddings.LayerNorm", h)?,
        })
    }

    /// The block's output for `ids`, `ids.len() x hidden_size`; the caller has checked the
    /// length against [`Config::max_tokens`].
    ///
    /// # Errors
    /// [`Error::Input`] for an id outside the vocabulary.
    pub(super) fn apply(&self, ids: &[u32], c: &Config) -> Result<Vec<f32>> {
        let h = c.hidden_size;
        let mut x = Vec::with_capacity(ids.len() * h);
        for (pos, &id) in ids.iter().enumerate() {
            let id = usize::try_from(id)
                .ok()
                .filter(|&i| i < c.vocab_size)
                .ok_or_else(|| Error::Input(format!("token id {id} outside the vocabulary")))?;
            let word = &self.word[id * h..(id + 1) * h];
            let p = pos + c.position_offset;
            let position = &self.position[p * h..(p + 1) * h];
            let token_type = &self.token_type[..h];
            x.extend(
                word.iter()
                    .zip(position)
                    .zip(token_type)
                    .map(|((w, p), t)| w + p + t),
            );
        }
        self.norm.apply(&mut x, c.layer_norm_eps);
        Ok(x)
    }
}
