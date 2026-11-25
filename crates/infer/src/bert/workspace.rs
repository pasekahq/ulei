//! The buffers of the forward pass, reused across layers and inputs (ADR-0086 section 3).

use crate::kernels::{self, Quant};

/// The activation buffers of [`super::Encoder`]'s forward pass, kept by the caller across
/// inputs (ADR-0086 section 3, the buffer pool).
///
/// Every buffer is sized on first use and grows only when a longer input (or a wider model)
/// needs more; it is never shrunk, so once it has seen the longest input of a process a
/// forward pass allocates nothing in its layers, and no buffer is zeroed again. Its contents
/// between calls mean nothing: the forward pass writes every value before it reads it, so a
/// reused workspace gives the same bits as a new one. [`Workspace::release`] frees the memory
/// (a caller that must keep its idle memory low calls it after a long input).
///
/// One workspace serves one forward pass at a time; an [`super::Encoder`] is shared, so
/// threads each keep their own.
#[derive(Debug, Default)]
pub struct Workspace {
    /// The token states, `tokens x hidden`: the embedding block's output, then each layer's
    /// input and output in place.
    pub(super) hidden: Vec<f32>,
    /// The attention block's output after its residual and `LayerNorm`, `tokens x hidden`.
    pub(super) attended: Vec<f32>,
    /// The attention context, `tokens x hidden`.
    pub(super) context: Vec<f32>,
    /// The query, key and value projections, `tokens x hidden` each.
    pub(super) query: Vec<f32>,
    pub(super) key: Vec<f32>,
    pub(super) value: Vec<f32>,
    /// The feed-forward block's inner activations, `tokens x intermediate`.
    pub(super) inner: Vec<f32>,
    /// One dequantised row of the int8 token table.
    pub(super) word: Vec<f32>,
    /// The buffers of the linear layers and the kernels.
    pub(super) linear: LinearBuffers,
}

impl Workspace {
    /// An empty workspace: it allocates on first use.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes held by its buffers (their capacity).
    pub fn bytes(&self) -> usize {
        let f32s = [
            &self.hidden,
            &self.attended,
            &self.context,
            &self.query,
            &self.key,
            &self.value,
            &self.inner,
            &self.word,
            &self.linear.scales,
        ]
        .iter()
        .map(|v| v.capacity() * std::mem::size_of::<f32>())
        .sum::<usize>();
        f32s + self.linear.values.capacity() + self.linear.kernels.bytes()
    }

    /// Frees every buffer; the next forward pass allocates them again.
    pub fn release(&mut self) {
        *self = Self::default();
    }
}

/// The quantised input of a linear layer and the kernels' working buffers.
#[derive(Debug, Default)]
pub(super) struct LinearBuffers {
    values: Vec<i8>,
    scales: Vec<f32>,
    pub(super) kernels: kernels::Scratch,
}

impl LinearBuffers {
    /// Quantises `x` (rows of `width`) per row and per block of `block` channels into the
    /// buffers, and lends them out with the kernels' scratch.
    pub(super) fn quantise(
        &mut self,
        x: &[f32],
        width: usize,
        block: usize,
    ) -> (Quant<'_>, &mut kernels::Scratch) {
        let rows = x.len().checked_div(width).unwrap_or(0);
        let values = kernels::grown(&mut self.values, x.len(), 0);
        let scales = kernels::grown(&mut self.scales, rows * width.div_ceil(block.max(1)), 0.0);
        kernels::quantize_rows_into(x, width, block, values, scales);
        let q = Quant {
            values,
            scales,
            width,
            block,
        };
        (q, &mut self.kernels)
    }
}
