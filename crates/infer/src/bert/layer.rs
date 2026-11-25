//! One post-LayerNorm encoder layer: multi-head self-attention and the GELU feed-forward block,
//! each with its residual and `LayerNorm`.

use super::Config;
use super::params::{self, Linear, Norm, Tensors};
use super::workspace::{LinearBuffers, Workspace};
use crate::kernels::{self, grown};
use crate::{Result, ops};

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

    /// The layer on the token states in `ws.hidden` (`tokens x hidden`), in place, with the
    /// other buffers of `ws`.
    pub(super) fn apply(&self, ws: &mut Workspace, tokens: usize, c: &Config) {
        let eps = c.layer_norm_eps;
        let n = tokens * c.hidden_size;
        let Workspace {
            hidden,
            attended,
            context,
            query,
            key,
            value,
            inner,
            linear,
            ..
        } = ws;
        let x = &mut hidden[..n];
        let context = grown(context, n, 0.0);
        self.attention(
            x,
            tokens,
            c,
            [
                grown(query, n, 0.0),
                grown(key, n, 0.0),
                grown(value, n, 0.0),
            ],
            context,
            linear,
        );
        let attended = grown(attended, n, 0.0);
        self.attn_out.apply_into(context, attended, linear);
        for (value, residual) in attended.iter_mut().zip(&*x) {
            *value += residual;
        }
        self.attn_norm.apply(attended, eps);
        let inner = grown(inner, tokens * c.intermediate_size, 0.0);
        self.ffn_in.apply_into(attended, inner, linear);
        match self.gelu {
            Gelu::Exact => {
                for value in &mut *inner {
                    *value = ops::gelu(*value);
                }
            }
            Gelu::F32 => kernels::gelu(inner),
        }
        self.ffn_out.apply_into(inner, x, linear);
        for (value, residual) in x.iter_mut().zip(&*attended) {
            *value += residual;
        }
        self.ffn_norm.apply(x, eps);
    }

    /// Multi-head scaled dot-product self-attention of `input`: its query, key and value
    /// projections into `qkv`, the context into `context` (`tokens x hidden` each).
    #[allow(clippy::cast_precision_loss)]
    fn attention(
        &self,
        input: &[f32],
        tokens: usize,
        c: &Config,
        qkv: [&mut [f32]; 3],
        context: &mut [f32],
        linear: &mut LinearBuffers,
    ) {
        let [query, key, value] = qkv;
        params::apply_all_into(
            [&self.query, &self.key, &self.value],
            input,
            [&mut *query, &mut *key, &mut *value],
            linear,
        );
        let scale = 1.0 / (c.head_size() as f32).sqrt();
        kernels::attention(
            query,
            key,
            value,
            tokens,
            c.num_heads,
            scale,
            match self.gelu {
                Gelu::Exact => kernels::Softmax::Exact,
                Gelu::F32 => kernels::Softmax::F32,
            },
            context,
            &mut linear.kernels,
        );
    }

    /// The attention context of `input` into a new vector (tests).
    #[cfg(test)]
    pub(super) fn attention_of(&self, input: &[f32], tokens: usize, c: &Config) -> Vec<f32> {
        let mut qkv = vec![vec![0.0; input.len()]; 3];
        let mut context = vec![0.0; input.len()];
        let [query, key, value] = &mut qkv[..] else {
            unreachable!()
        };
        self.attention(
            input,
            tokens,
            c,
            [query, key, value],
            &mut context,
            &mut LinearBuffers::default(),
        );
        context
    }
}
