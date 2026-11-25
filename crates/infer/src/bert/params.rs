//! Binding the tensors of a weight source by name and shape, and the parameter groups built
//! from them. Two sources: an fp32 `safetensors` checkpoint (E1) and the engine's int8 weight
//! file (E3, written by [`super::convert`]).

use crate::kernels;
use crate::ops;
use crate::weights::file::WeightFile;
use crate::weights::{Checkpoint, View};
use crate::{Error, Result};

/// A linear layer `y = x W^T + b`, `W` of shape `[out, in]`, `b` of `out` values.
#[derive(Debug, Clone, Copy)]
pub(super) enum Linear<'w> {
    /// fp32 weights.
    F32 {
        weight: &'w [f32],
        bias: &'w [f32],
        inputs: usize,
    },
    /// int8 weights with one scale per output channel; the input is quantised per row and per
    /// block of `block` channels (ADR-0086 section 3).
    I8 {
        weight: &'w [i8],
        scale: &'w [f32],
        bias: &'w [f32],
        inputs: usize,
        block: usize,
    },
}

impl Linear<'_> {
    pub(super) fn apply(&self, x: &[f32]) -> Vec<f32> {
        match *self {
            Self::F32 {
                weight,
                bias,
                inputs,
            } => kernels::linear(x, weight, bias, inputs),
            Self::I8 {
                weight,
                scale,
                bias,
                inputs,
                block,
            } => kernels::linear_i8(
                &kernels::quantize_rows(x, inputs, block),
                weight,
                scale,
                bias,
            ),
        }
    }
}

/// Applies the linear layers `layers` to the same input `x`. When all of them are int8 with
/// the same input width and block size, `x` is quantised once for all of them: the outputs are
/// the same as from [`Linear::apply`] one by one, since quantisation is deterministic.
pub(super) fn apply_all<const N: usize>(layers: [&Linear<'_>; N], x: &[f32]) -> [Vec<f32>; N] {
    let shared = layers
        .iter()
        .try_fold(None, |shared, l| match (**l, shared) {
            (Linear::I8 { inputs, block, .. }, None) => Some(Some((inputs, block))),
            (Linear::I8 { inputs, block, .. }, Some(s)) if s == (inputs, block) => Some(Some(s)),
            _ => None,
        });
    match shared {
        Some(Some((inputs, block))) => {
            let q = kernels::quantize_rows(x, inputs, block);
            layers.map(|l| match *l {
                Linear::I8 {
                    weight,
                    scale,
                    bias,
                    ..
                } => kernels::linear_i8(&q, weight, scale, bias),
                Linear::F32 { .. } => l.apply(x),
            })
        }
        _ => layers.map(|l| l.apply(x)),
    }
}

/// `LayerNorm` parameters.
#[derive(Debug, Clone, Copy)]
pub(super) struct Norm<'w> {
    gamma: &'w [f32],
    beta: &'w [f32],
}

impl Norm<'_> {
    /// Normalises each row of `x` in place.
    pub(super) fn apply(&self, x: &mut [f32], eps: f32) {
        ops::layer_norm(x, self.gamma, self.beta, eps);
    }
}

/// The token embedding table.
#[derive(Debug, Clone, Copy)]
pub(super) enum Table<'w> {
    /// fp32 rows in token id order.
    F32(&'w [f32]),
    /// int8 rows in the file's row order with one scale per row, and the row of each id.
    I8 {
        rows: &'w [i8],
        scales: &'w [f32],
        row_of_id: &'w [u32],
    },
}

/// Where the tensors come from.
pub(super) enum Tensors<'w> {
    /// A checkpoint, its names under the prefix it puts before `embeddings.`.
    Checkpoint {
        checkpoint: &'w Checkpoint,
        prefix: String,
    },
    /// The engine's weight file, its activations quantised in blocks of `block` channels.
    File { file: &'w WeightFile, block: usize },
}

/// Suffixes of the int8 token table's tensors in the weight file.
pub(super) const TABLE: &str = "embeddings.word_embeddings";

impl<'w> Tensors<'w> {
    /// Finds the prefix of `checkpoint`'s tensor names.
    ///
    /// # Errors
    /// [`Error::Weights`] if the checkpoint has no `embeddings.word_embeddings.weight`.
    pub(super) fn checkpoint(checkpoint: &'w Checkpoint) -> Result<Self> {
        let prefix = checkpoint
            .names()
            .into_iter()
            .find_map(|n| n.strip_suffix("embeddings.word_embeddings.weight"))
            .ok_or_else(|| Error::Weights("no `embeddings.word_embeddings.weight`".into()))?
            .to_string();
        Ok(Self::Checkpoint { checkpoint, prefix })
    }

    /// The fp32 data of tensor `name`, which must have `shape`.
    pub(super) fn get(&self, name: &str, shape: &[usize]) -> Result<&'w [f32]> {
        match self {
            Self::Checkpoint { checkpoint, prefix } => {
                let full = format!("{prefix}{name}");
                shaped(&full, checkpoint.get(&full)?, shape)
            }
            Self::File { file, .. } => shaped(name, file.f32(name)?, shape),
        }
    }

    /// The linear layer `name` (`name.weight`, `name.bias`, and `name.scale` in the file).
    pub(super) fn linear(&self, name: &str, out: usize, inputs: usize) -> Result<Linear<'w>> {
        let bias = self.get(&format!("{name}.bias"), &[out])?;
        match self {
            Self::Checkpoint { .. } => Ok(Linear::F32 {
                weight: self.get(&format!("{name}.weight"), &[out, inputs])?,
                bias,
                inputs,
            }),
            Self::File { file, block } => {
                let w = format!("{name}.weight");
                Ok(Linear::I8 {
                    weight: shaped(&w, file.i8(&w)?, &[out, inputs])?,
                    scale: self.get(&format!("{name}.scale"), &[out])?,
                    bias,
                    inputs,
                    block: *block,
                })
            }
        }
    }

    /// The `LayerNorm` `name` over rows of `width` values.
    pub(super) fn norm(&self, name: &str, width: usize) -> Result<Norm<'w>> {
        Ok(Norm {
            gamma: self.get(&format!("{name}.weight"), &[width])?,
            beta: self.get(&format!("{name}.bias"), &[width])?,
        })
    }

    /// The token embedding table of `vocab` rows of `width` values.
    pub(super) fn table(&self, vocab: usize, width: usize) -> Result<Table<'w>> {
        match self {
            Self::Checkpoint { .. } => Ok(Table::F32(
                self.get(&format!("{TABLE}.weight"), &[vocab, width])?,
            )),
            Self::File { file, .. } => {
                let rows = format!("{TABLE}.weight");
                let map = format!("{TABLE}.row_of_id");
                let row_of_id = shaped(&map, file.u32(&map)?, &[vocab])?;
                if let Some(bad) = row_of_id
                    .iter()
                    .find(|&&r| usize::try_from(r).map_or(true, |r| r >= vocab))
                {
                    return Err(Error::Weights(format!("`{map}` names row {bad}")));
                }
                Ok(Table::I8 {
                    rows: shaped(&rows, file.i8(&rows)?, &[vocab, width])?,
                    scales: self.get(&format!("{TABLE}.scale"), &[vocab])?,
                    row_of_id,
                })
            }
        }
    }
}

fn shaped<'w, T: Copy>(name: &str, v: View<'w, T>, shape: &[usize]) -> Result<&'w [T]> {
    if v.shape == shape {
        Ok(v.data)
    } else {
        Err(Error::Weights(format!(
            "`{name}` has shape {:?}, the config expects {shape:?}",
            v.shape
        )))
    }
}
