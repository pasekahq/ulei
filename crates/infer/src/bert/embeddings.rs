//! The embedding block: word, position and token type embeddings summed, then `LayerNorm`.

use super::Config;
use super::params::{Norm, Table, Tensors};
use crate::kernels::grown;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub(super) struct Embeddings<'w> {
    word: Table<'w>,
    position: &'w [f32],
    token_type: &'w [f32],
    norm: Norm<'w>,
}

impl<'w> Embeddings<'w> {
    /// Binds the `embeddings.*` tensors, checking their shapes against `c`.
    pub(super) fn bind(t: &Tensors<'w>, c: &Config) -> Result<Self> {
        let h = c.hidden_size;
        Ok(Self {
            word: t.table(c.vocab_size, h)?,
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

    /// The block's output for `ids` into the first `ids.len() x hidden_size` values of
    /// `hidden` (grown if shorter), with `word` for a dequantised table row; the caller has
    /// checked the length against [`Config::max_tokens`].
    ///
    /// # Errors
    /// [`Error::Input`] for an id outside the vocabulary.
    #[allow(clippy::many_single_char_names)]
    pub(super) fn apply(
        &self,
        ids: &[u32],
        c: &Config,
        hidden: &mut Vec<f32>,
        word: &mut Vec<f32>,
    ) -> Result<()> {
        let h = c.hidden_size;
        let x = grown(hidden, ids.len() * h, 0.0);
        let dequantised = grown(word, h, 0.0);
        for (pos, &id) in ids.iter().enumerate() {
            let id = usize::try_from(id)
                .ok()
                .filter(|&i| i < c.vocab_size)
                .ok_or_else(|| Error::Input(format!("token id {id} outside the vocabulary")))?;
            let word: &[f32] = match self.word {
                Table::F32(table) => &table[id * h..(id + 1) * h],
                Table::I8 {
                    rows,
                    scales,
                    row_of_id,
                } => {
                    // Checked below the vocabulary size when the table was bound.
                    let r = row_of_id[id] as usize;
                    let s = scales[r];
                    for (d, q) in dequantised.iter_mut().zip(&rows[r * h..(r + 1) * h]) {
                        *d = f32::from(*q) * s;
                    }
                    dequantised
                }
            };
            let p = pos + c.position_offset;
            let position = &self.position[p * h..(p + 1) * h];
            let token_type = &self.token_type[..h];
            for (((o, w), p), t) in x[pos * h..(pos + 1) * h]
                .iter_mut()
                .zip(word)
                .zip(position)
                .zip(token_type)
            {
                *o = w + p + t;
            }
        }
        self.norm.apply(x, c.layer_norm_eps);
        Ok(())
    }
}
