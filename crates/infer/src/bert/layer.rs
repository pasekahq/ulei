//! One post-LayerNorm encoder layer: multi-head self-attention and the GELU feed-forward block,
//! each with its residual and `LayerNorm`.

use super::Config;
use super::params::{Linear, Norm, Tensors};
use crate::{Result, kernels, ops};

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
        for value in &mut inner {
            *value = ops::gelu(*value);
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
        let query = self.query.apply(input);
        let key = self.key.apply(input);
        let value = self.value.apply(input);
        let scale = 1.0 / (width as f32).sqrt();
        // The slice of token `t` in head `head` of a `tokens x hidden` matrix.
        let span =
            |t: usize, head: usize| t * hidden + head * width..t * hidden + (head + 1) * width;
        let mut context = vec![0.0f32; tokens * hidden];
        let mut scores = vec![0.0f32; tokens];
        for head in 0..c.num_heads {
            for i in 0..tokens {
                let q = &query[span(i, head)];
                for (j, score) in scores.iter_mut().enumerate() {
                    *score = kernels::dot(q, &key[span(j, head)]) * scale;
                }
                ops::softmax(&mut scores);
                let out = &mut context[span(i, head)];
                for (j, &p) in scores.iter().enumerate() {
                    for (o, v) in out.iter_mut().zip(&value[span(j, head)]) {
                        *o += p * v;
                    }
                }
            }
        }
        context
    }
}
