//! Matrix kernels and their dispatch: the one place that knows which implementation runs.
//!
//! Models call the functions of this module only; they pick the implementation. Today there is
//! one, the portable code of [`portable`]: the fp32 product of milestone E1 and the int8 linear
//! layer of E3 (ADR-0087 sections 2 and 3). The int8 GEMM kernels per instruction set
//! (ADR-0087 section 5) go into this module as siblings of [`portable`], picked once at start
//! by run-time feature detection behind these same functions, so a kernel is swapped without
//! touching a model. This module and the weight file's `mmap` in [`crate::weights`] are the
//! only places where ADR-0087 section 5 lifts the `unsafe` denial (the mapping since E4, the
//! kernels from E5 on).

pub mod portable;

/// Activations quantised to int8 for one linear layer: one scale per token row and per block
/// of `block` consecutive input channels (ADR-0087 section 3). The last block of a row is
/// shorter when `block` does not divide `width`.
#[derive(Debug, Clone, PartialEq)]
pub struct QuantRows {
    /// `rows x width` int8 values, row-major.
    pub values: Vec<i8>,
    /// `rows x width.div_ceil(block)` scales, row-major: value = `q * scale` of its block.
    pub scales: Vec<f32>,
    /// Input channels per row.
    pub width: usize,
    /// Input channels per block.
    pub block: usize,
}

impl QuantRows {
    /// Number of token rows.
    pub fn rows(&self) -> usize {
        self.values.len().checked_div(self.width).unwrap_or(0)
    }

    /// Blocks per row.
    pub fn blocks(&self) -> usize {
        self.width.div_ceil(self.block)
    }
}

/// Dot product of two equal-length slices.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    portable::dot(a, b)
}

/// A linear layer on `x` (rows of `w_in` values): `y = x W^T + b` with `w` of shape
/// `[out, w_in]` row-major (the `PyTorch` layout) and `b` of `out` values.
pub fn linear(x: &[f32], w: &[f32], b: &[f32], w_in: usize) -> Vec<f32> {
    portable::linear(x, w, b, w_in)
}

/// Quantises `x` to int8 with one symmetric scale, `max |x| / 127` (1 when that is 0), each
/// value `x / scale` rounded half away from zero and clamped to `[-127, 127]`; returns the
/// scale. The rule of every int8 tensor of ADR-0087: weight rows, token table rows and
/// activation blocks.
///
/// # Panics
/// If `q` and `x` differ in length.
pub fn quantize(x: &[f32], q: &mut [i8]) -> f32 {
    portable::quantize(x, q)
}

/// Quantises `x` (rows of `width` values) per row and per block of `block` channels.
///
/// # Panics
/// If `width` or `block` is 0 or `x` is not a whole number of rows.
pub fn quantize_rows(x: &[f32], width: usize, block: usize) -> QuantRows {
    portable::quantize_rows(x, width, block)
}

/// The int8 linear layer: `y = x W^T + b` with `x` quantised by [`quantize_rows`] and `w`
/// int8 of shape `[out, x.width]` with one scale per output channel in `w_scale`. Inside a
/// block the products are summed in `i32`; each block sum is scaled by its activation scale
/// and the blocks are summed in `f32`, then scaled by the output channel's weight scale and the
/// bias added.
///
/// # Panics
/// If `w`, `w_scale` and `b` disagree with `x.width` and `b.len()`.
pub fn linear_i8(x: &QuantRows, w: &[i8], w_scale: &[f32], b: &[f32]) -> Vec<f32> {
    portable::linear_i8(x, w, w_scale, b)
}
