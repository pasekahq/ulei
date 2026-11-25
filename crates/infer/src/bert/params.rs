//! Binding the tensors of a weight source by name and shape, and the parameter groups built
//! from them. Two sources: an fp32 `safetensors` checkpoint (E1) and the engine's int8 weight
//! file (E3, written by [`super::convert`]).

use super::workspace::LinearBuffers;
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
    /// `y = x W^T + b` into `y` (every value written), with `buffers` for the quantised input
    /// and the kernels.
    pub(super) fn apply_into(&self, x: &[f32], y: &mut [f32], buffers: &mut LinearBuffers) {
        match *self {
            Self::F32 {
                weight,
                bias,
                inputs,
            } => kernels::linear_into(x, weight, bias, inputs, y),
            Self::I8 {
                weight,
                scale,
                bias,
                inputs,
                block,
            } => {
                let (q, scratch) = buffers.quantise(x, inputs, block);
                kernels::linear_i8_into(&q, weight, scale, bias, y, scratch);
            }
        }
    }

    /// [`Linear::apply_into`] into a new vector (tests).
    #[cfg(test)]
    pub(super) fn apply(&self, x: &[f32]) -> Vec<f32> {
        let out = match *self {
            Self::F32 { bias, inputs, .. } | Self::I8 { bias, inputs, .. } => {
                x.len() / inputs * bias.len()
            }
        };
        let mut y = vec![0.0; out];
        self.apply_into(x, &mut y, &mut LinearBuffers::default());
        y
    }
}

/// Applies the linear layers `layers` to the same input `x`, each into its output in `outs`.
/// When all of them are int8 with the same input width and block size, `x` is quantised once
/// for all of them: the outputs are the same as from [`Linear::apply_into`] one by one, since
/// quantisation is deterministic.
pub(super) fn apply_all_into<const N: usize>(
    layers: [&Linear<'_>; N],
    x: &[f32],
    outs: [&mut [f32]; N],
    buffers: &mut LinearBuffers,
) {
    let shared = layers
        .iter()
        .try_fold(None, |shared, l| match (**l, shared) {
            (Linear::I8 { inputs, block, .. }, None) => Some(Some((inputs, block))),
            (Linear::I8 { inputs, block, .. }, Some(s)) if s == (inputs, block) => Some(Some(s)),
            _ => None,
        });
    match shared {
        Some(Some((inputs, block))) => {
            let (q, scratch) = buffers.quantise(x, inputs, block);
            for (l, y) in layers.into_iter().zip(outs) {
                if let Linear::I8 {
                    weight,
                    scale,
                    bias,
                    ..
                } = *l
                {
                    kernels::linear_i8_into(&q, weight, scale, bias, y, scratch);
                }
            }
        }
        _ => {
            for (l, y) in layers.into_iter().zip(outs) {
                l.apply_into(x, y, buffers);
            }
        }
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
