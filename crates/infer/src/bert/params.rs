//! Binding a checkpoint's tensors by name and shape, and the parameter groups built from them.

use crate::weights::{Checkpoint, View};
use crate::{Error, Result, kernels, ops};

/// A linear layer: `weight` of shape `[out, in]` and `bias` of `out` values.
#[derive(Debug, Clone, Copy)]
pub(super) struct Linear<'w> {
    weight: &'w [f32],
    bias: &'w [f32],
    inputs: usize,
}

impl Linear<'_> {
    pub(super) fn apply(&self, x: &[f32]) -> Vec<f32> {
        kernels::linear(x, self.weight, self.bias, self.inputs)
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

/// The tensors of a checkpoint under the prefix it puts before `embeddings.`.
pub(super) struct Tensors<'w> {
    checkpoint: &'w Checkpoint,
    prefix: String,
}

impl<'w> Tensors<'w> {
    /// Finds the prefix of `checkpoint`'s tensor names.
    ///
    /// # Errors
    /// [`Error::Weights`] if the checkpoint has no `embeddings.word_embeddings.weight`.
    pub(super) fn new(checkpoint: &'w Checkpoint) -> Result<Self> {
        let prefix = checkpoint
            .names()
            .into_iter()
            .find_map(|n| n.strip_suffix("embeddings.word_embeddings.weight"))
            .ok_or_else(|| Error::Weights("no `embeddings.word_embeddings.weight`".into()))?
            .to_string();
        Ok(Self { checkpoint, prefix })
    }

    /// The data of tensor `name`, which must have `shape`.
    pub(super) fn get(&self, name: &str, shape: &[usize]) -> Result<&'w [f32]> {
        let full = format!("{}{name}", self.prefix);
        let View { shape: s, data } = self.checkpoint.get(&full)?;
        if s == shape {
            Ok(data)
        } else {
            Err(Error::Weights(format!(
                "`{full}` has shape {s:?}, the config expects {shape:?}"
            )))
        }
    }

    /// The linear layer `name` (`name.weight`, `name.bias`).
    pub(super) fn linear(&self, name: &str, out: usize, inputs: usize) -> Result<Linear<'w>> {
        Ok(Linear {
            weight: self.get(&format!("{name}.weight"), &[out, inputs])?,
            bias: self.get(&format!("{name}.bias"), &[out])?,
            inputs,
        })
    }

    /// The `LayerNorm` `name` over rows of `width` values.
    pub(super) fn norm(&self, name: &str, width: usize) -> Result<Norm<'w>> {
        Ok(Norm {
            gamma: self.get(&format!("{name}.weight"), &[width])?,
            beta: self.get(&format!("{name}.bias"), &[width])?,
        })
    }
}
