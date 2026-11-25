//! One post-LayerNorm encoder layer: multi-head self-attention and the GELU feed-forward block,
//! each with its residual and `LayerNorm`.

use super::Config;
use super::params::{self, Linear, Norm, Tensors};
use crate::{Result, kernels, ops};

/// Which GELU a layer applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gelu {
    /// The exact form, [`ops::gelu`]: the fp32 reference path (E1).
    Exact,
    /// The `f32` formula [`ops::gelu_f32`] through [`kernels::gelu`]: the int8 path.
    F32,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Layer<'w> {
    query: Linear<'w>,
    key: Linear<'w>,
    pub(super) value: Linear<'w>,
    attn_out: Linear<'w>,
    attn_norm: Norm<'w>,
    ffn_in: Linear<'w>,
    ffn_out: Linear<'w>,
    ffn_norm: Norm<'w>,
    gelu: Gelu,
}

impl<'w> Layer<'w> {
    /// Binds the tensors of `encoder.layer.{index}`, checking their shapes against `c`.
    pub(super) fn bind(t: &Tensors<'w>, c: &Config, index: usize) -> Result<Self> {
        let h = c.hidden_size;
        let ffn = c.intermediate_size;
        let p = format!("encoder.layer.{index}");
        Ok(Self {
            query: t.linear(&format!("{p}.attention.self.query"), h, h)?,
            key: t.linear(&format!("{p}.attention.self.key"), h, h)?,
            value: t.linear(&format!("{p}.attention.self.value"), h, h)?,
            attn_out: t.linear(&format!("{p}.attention.output.dense"), h, h)?,
            attn_norm: t.norm(&format!("{p}.attention.output.LayerNorm"), h)?,
            ffn_in: t.linear(&format!("{p}.intermediate.dense"), ffn, h)?,
            ffn_out: t.linear(&format!("{p}.output.dense"), h, ffn)?,
            ffn_norm: t.norm(&format!("{p}.output.LayerNorm"), h)?,
            gelu: match t {
                Tensors::Checkpoint { .. } => Gelu::Exact,
                Tensors::File { .. } => Gelu::F32,
            },
        })
    }

    /// The layer's output for `input`, `tokens x hidden`.
    pub(super) fn apply(&self, input: &[f32], tokens: usize, c: &Config) -> Vec<f32> {
        let eps = c.layer_norm_eps;
        let context = self.attention(input, tokens, c);
        let mut attended = self.attn_out.apply(&context);
        for (value, residual) in attended.iter_mut().zip(input) {
            *value += residual;
        }
        self.attn_norm.apply(&mut attended, eps);
        let mut inner = self.ffn_in.apply(&attended);
        match self.gelu {
            Gelu::Exact => {
                for value in &mut inner {
                    *value = ops::gelu(*value);
                }
            }
            Gelu::F32 => kernels::gelu(&mut inner),
        }
        let mut out = self.ffn_out.apply(&inner);
        for (value, residual) in out.iter_mut().zip(&attended) {
            *value += residual;
        }
        self.ffn_norm.apply(&mut out, eps);
        out
    }

    /// Multi-head scaled dot-product self-attention; returns the context, `tokens x hidden`.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn attention(&self, input: &[f32], tokens: usize, c: &Config) -> Vec<f32> {
        let hidden = c.hidden_size;
        let width = c.head_size();
        let [query, key, value] = params::apply_all([&self.query, &self.key, &self.value], input);
        let scale = 1.0 / (width as f32).sqrt();
        let mut context = vec![0.0f32; tokens * hidden];
        kernels::attention(
            &query,
            &key,
            &value,
            tokens,
            c.num_heads,
            scale,
            match self.gelu {
                Gelu::Exact => kernels::Softmax::Exact,
                Gelu::F32 => kernels::Softmax::F32,
            },
            &mut context,
        );
        context
    }
}
