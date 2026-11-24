//! Matrix kernels and their dispatch: the one place that knows which implementation runs.
//!
//! Models call [`linear`] and [`dot`] only; the functions here pick the implementation. Today
//! there is one, the portable fp32 code of [`portable`] (milestone E1 of ADR-0087). The int8
//! GEMM kernels per instruction set (ADR-0087 section 5) go into this module as siblings of
//! [`portable`], picked once at start by run-time feature detection behind these same
//! functions, so a kernel is swapped without touching a model. This module and the weight
//! file's `mmap` in [`crate::weights`] are the only places where ADR-0087 section 5 lifts the
//! `unsafe` denial, from E5 on.

pub mod portable;

/// Dot product of two equal-length slices.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    portable::dot(a, b)
}

/// A linear layer on `x` (rows of `w_in` values): `y = x W^T + b` with `w` of shape
/// `[out, w_in]` row-major (the `PyTorch` layout) and `b` of `out` values.
pub fn linear(x: &[f32], w: &[f32], b: &[f32], w_in: usize) -> Vec<f32> {
    portable::linear(x, w, b, w_in)
}
