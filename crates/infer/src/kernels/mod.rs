//! Matrix kernels and their dispatch: the one place that knows which implementation runs.
//!
//! Models call the functions of this module only; they pick the implementation. [`portable`]
//! holds the reference code: the fp32 product of milestone E1 and the int8 linear layer of E3
//! (ADR-0086 sections 2 and 3). The int8 linear layer also has one kernel per instruction set
//! (ADR-0086 section 5), picked once by run-time feature detection behind [`linear_i8`]:
//!
//! - `avx2` (`x86_64`): `VPMOVSXBW` + `VPMADDWD`, eight output channels at a time, one per
//!   lane;
//! - `neon` (`aarch64`): `SDOT` with the `dotprod` feature, an exact widening multiply
//!   otherwise. Compiled and fuzz-tested where an `aarch64` host runs the tests; not picked by
//!   [`Kernel::best`] until it has run on such a host (ADR-0086 section 5, "no hardware, no
//!   kernel").
//!
//! **What a kernel computes.** Only the integer part: for each token row and tile of [`TILE`]
//! output channels, the `i32` sum of each block of B input channels. The fp32 combination of those
//! block sums (each times its activation scale, summed over the blocks in order, times the
//! output channel's weight scale, plus the bias) is [`combine`], shared by every kernel. The
//! integer sums are exact in every kernel, so every kernel's output is bit-identical to
//! [`portable::linear_i8`]; the fuzz tests of `fuzz` check both halves.
//!
//! **The fp32 operations around it** ([`quantize_rows`], [`scores`], [`weighted_sum`],
//! [`gelu`]) have
//! one source, in [`portable`]. Where the AVX2 kernel runs, the same source is compiled a
//! second time with AVX2 enabled. Rust never fuses a multiply and an add, and `round`, `max`
//! and the order of every sum are fixed by the source, so that build gives the same bits with
//! wider registers.
//!
//! These modules (`avx2`, `neon`) and the weight file's mapping in `weights::map` are the
//! only places where ADR-0086 section 5 lifts the `unsafe` denial,
//! each by one `#[allow(unsafe_code)]` on its `mod` line. Each exposes a safe API behind a
//! token that exists only once its CPU feature is detected.

use std::sync::atomic::{AtomicU8, Ordering};

pub mod portable;

// Intrinsics: ADR-0086 section 5 lifts the workspace's `unsafe_code = "deny"` for the kernel
// modules (and `weights::map`) only.
#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod avx2;
#[cfg(target_arch = "aarch64")]
#[allow(unsafe_code)]
mod neon;

#[cfg(test)]
mod fuzz;

/// Output channels per call of an instruction-set kernel.
const TILE: usize = 8;

/// Activations quantised to int8 for one linear layer: one scale per token row and per block
/// of `block` consecutive input channels (ADR-0086 section 3). The last block of a row is
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

/// An implementation of the int8 linear layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kernel {
    /// Plain Rust: [`portable`], on every CPU.
    Portable,
    /// AVX2 (`x86_64`).
    Avx2,
    /// NEON, `SDOT` with `dotprod`, widening multiplies otherwise (`aarch64`).
    Neon,
}

impl Kernel {
    /// Every kernel, on any CPU.
    pub const ALL: [Self; 3] = [Self::Portable, Self::Avx2, Self::Neon];

    /// Its name: `portable`, `avx2`, `neon`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Portable => "portable",
            Self::Avx2 => "avx2",
            Self::Neon => "neon",
        }
    }

    /// Whether this CPU can run it.
    pub fn available(self) -> bool {
        match self {
            Self::Portable => true,
            #[cfg(target_arch = "x86_64")]
            Self::Avx2 => avx2::Avx2::detect().is_some(),
            #[cfg(target_arch = "aarch64")]
            Self::Neon => true,
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }

    /// The kernels this CPU can run, portable first.
    pub fn all_available() -> Vec<Self> {
        Self::ALL.into_iter().filter(|k| k.available()).collect()
    }

    /// The kernel picked by default on this CPU: the fastest one measured on its instruction
    /// set (ADR-0086 E5) among those verified on real hardware.
    pub fn best() -> Self {
        if Self::Avx2.available() {
            Self::Avx2
        } else {
            Self::Portable
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Portable => 1,
            Self::Avx2 => 2,
            Self::Neon => 3,
        }
    }
}

/// The selected kernel's [`Kernel::code`]; 0 until the first use picks [`Kernel::best`].
static ACTIVE: AtomicU8 = AtomicU8::new(0);

/// The kernel [`linear_i8`] runs: [`Kernel::best`] unless [`select`] chose another.
pub fn active() -> Kernel {
    let code = ACTIVE.load(Ordering::Relaxed);
    if let Some(k) = Kernel::ALL.into_iter().find(|k| k.code() == code) {
        return k;
    }
    let best = Kernel::best();
    ACTIVE.store(best.code(), Ordering::Relaxed);
    best
}

/// Makes [`linear_i8`] run `kernel` from now on, process-wide, if this CPU can run it; returns
/// whether it did. For measurements and tests: every kernel gives bit-identical outputs, so
/// switching changes only the speed.
pub fn select(kernel: Kernel) -> bool {
    let ok = kernel.available();
    if ok {
        ACTIVE.store(kernel.code(), Ordering::Relaxed);
    }
    ok
}

/// Dot product of two equal-length slices.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    portable::dot(a, b)
}

/// Attention scores of one query: `out[j] = dot(q, key j) * scale`, key `j` being
/// `keys[j * stride..j * stride + q.len()]`, each [`dot`] exactly as it computes it.
///
/// # Panics
/// If `keys` is shorter than `out.len()` keys.
pub fn scores(q: &[f32], keys: &[f32], stride: usize, scale: f32, out: &mut [f32]) {
    match Tile::of(active()) {
        #[cfg(target_arch = "x86_64")]
        Tile::Avx2(t) => avx2::scores(t, q, keys, stride, scale, out),
        _ => portable::scores(q, keys, stride, scale, out),
    }
}

/// Adds `p[j] * value j` to `out` for `j` in order, element by element, value `j` being
/// `values[j * stride..j * stride + out.len()]`: the context of one query in one head.
///
/// # Panics
/// If `values` is shorter than `p.len()` values.
pub fn weighted_sum(p: &[f32], values: &[f32], stride: usize, out: &mut [f32]) {
    match Tile::of(active()) {
        #[cfg(target_arch = "x86_64")]
        Tile::Avx2(t) => avx2::weighted_sum(t, p, values, stride, out),
        _ => portable::weighted_sum(p, values, stride, out),
    }
}

/// [`crate::ops::gelu_f32`] on every value of `x`, in place: the int8 path's activation. The
/// same bits whichever kernel runs (one `f32` formula of plain operations, compiled a second
/// time with AVX2 where it runs).
pub fn gelu(x: &mut [f32]) {
    match Tile::of(active()) {
        #[cfg(target_arch = "x86_64")]
        Tile::Avx2(t) => avx2::gelu(t, x),
        _ => portable::gelu(x),
    }
}

/// A linear layer on `x` (rows of `w_in` values): `y = x W^T + b` with `w` of shape
/// `[out, w_in]` row-major (the `PyTorch` layout) and `b` of `out` values.
pub fn linear(x: &[f32], w: &[f32], b: &[f32], w_in: usize) -> Vec<f32> {
    portable::linear(x, w, b, w_in)
}

/// Quantises `x` to int8 with one symmetric scale, `max |x| / 127` (1 when that is 0), each
/// value `x / scale` rounded half away from zero and clamped to `[-127, 127]`; returns the
/// scale. The rule of every int8 tensor of ADR-0086: weight rows, token table rows and
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
    match Tile::of(active()) {
        #[cfg(target_arch = "x86_64")]
        Tile::Avx2(t) => avx2::quantize_rows(t, x, width, block),
        _ => portable::quantize_rows(x, width, block),
    }
}

/// The int8 linear layer: `y = x W^T + b` with `x` quantised by [`quantize_rows`] and `w`
/// int8 of shape `[out, x.width]` with one scale per output channel in `w_scale`. Inside a
/// block the products are summed in `i32`; each block sum is scaled by its activation scale
/// and the blocks are summed in `f32`, then scaled by the output channel's weight scale and the
/// bias added. Runs the [`active`] kernel; the result is bit-identical to
/// [`portable::linear_i8`] whichever kernel runs.
///
/// # Panics
/// If `w`, `w_scale` and `b` disagree with `x.width` and `b.len()`.
pub fn linear_i8(x: &QuantRows, w: &[i8], w_scale: &[f32], b: &[f32]) -> Vec<f32> {
    linear_i8_on(active(), x, w, w_scale, b)
}

/// [`linear_i8`] on `kernel` ([`Kernel::Portable`] if this CPU cannot run it).
#[allow(clippy::many_single_char_names)]
fn linear_i8_on(kernel: Kernel, x: &QuantRows, w: &[i8], w_scale: &[f32], b: &[f32]) -> Vec<f32> {
    let (k, out) = (x.width, b.len());
    assert_eq!(w.len(), out * k, "linear_i8: weight is not [out, in]");
    assert_eq!(w_scale.len(), out, "linear_i8: one weight scale per output");
    let (rows, blocks) = (x.rows(), x.blocks());
    let mut y = vec![0.0f32; rows * out];
    let mut emit = |o0: usize, r: usize, sums: &[i32]| {
        let o1 = out.min(o0 + TILE);
        combine(
            sums,
            &x.scales[r * blocks..(r + 1) * blocks],
            &w_scale[o0..o1],
            &b[o0..o1],
            &mut y[r * out + o0..r * out + o1],
        );
    };
    match Tile::of(kernel) {
        #[cfg(target_arch = "x86_64")]
        Tile::Avx2(t) if x.block.is_multiple_of(2) => t.linear(x, w, out, &mut emit),
        #[cfg(target_arch = "aarch64")]
        Tile::Neon(t) => per_row(x, w, out, &mut emit, |xr, wt, sums| {
            if wt.len() == TILE * xr.len() {
                t.block_sums(xr, wt, x.block, sums);
            } else {
                portable::block_sums(xr, wt, x.block, sums);
            }
        }),
        _ => per_row(x, w, out, &mut emit, |xr, wt, sums| {
            portable::block_sums(xr, wt, x.block, sums);
        }),
    }
    y
}

/// The block sums of `x W^T` tile by tile and row by row with `sums_of(row, weight rows,
/// sums)` (block-major, `blocks x n` for `n` weight rows), passed to `emit` as the kernels do:
/// `emit(o0, r, sums)` with [`TILE`] lanes per block.
fn per_row(
    x: &QuantRows,
    w: &[i8],
    out: usize,
    emit: &mut impl FnMut(usize, usize, &[i32]),
    sums_of: impl Fn(&[i8], &[i8], &mut [i32]),
) {
    let (k, blocks) = (x.width, x.blocks());
    let mut sums = vec![0i32; blocks * TILE];
    let mut lanes = vec![0i32; blocks * TILE];
    for o0 in (0..out).step_by(TILE) {
        let n = TILE.min(out - o0);
        let wt = &w[o0 * k..(o0 + n) * k];
        for (r, xr) in x.values.chunks_exact(k).enumerate() {
            sums_of(xr, wt, &mut sums[..blocks * n]);
            if n == TILE {
                emit(o0, r, &sums);
            } else {
                for (dst, src) in lanes
                    .as_chunks_mut::<TILE>()
                    .0
                    .iter_mut()
                    .zip(sums.chunks_exact(n))
                {
                    dst[..n].copy_from_slice(src);
                }
                emit(o0, r, &lanes);
            }
        }
    }
}

/// The fp32 half of the int8 linear layer, shared by every kernel: for `n = ws.len()` output
/// channels, `sums` holds the block sums block-major with [`TILE`] lanes per block (lanes past
/// `n` are ignored), `xs` the row's activation scales, one per block. Each output is `(sum over
/// blocks in order of xs[b] * sums[b][o]) * ws[o] + bias[o]`, every operation rounded to `f32`
/// in this order, as in [`portable::linear_i8`].
#[allow(clippy::cast_precision_loss)]
#[inline]
fn combine(sums: &[i32], xs: &[f32], ws: &[f32], bias: &[f32], y: &mut [f32]) {
    let mut acc = [0.0f32; TILE];
    for (s, block) in xs.iter().zip(sums.as_chunks::<TILE>().0) {
        // Exact: a block sum is below 2^24 in magnitude for blocks up to 1040 channels.
        for (a, &v) in acc.iter_mut().zip(block) {
            *a += s * v as f32;
        }
    }
    for (((o, a), w), b) in y.iter_mut().zip(&acc).zip(ws).zip(bias) {
        *o = a * w + b;
    }
}

/// The kernel resolved once per layer call, with the proof of its CPU feature.
#[derive(Clone, Copy)]
enum Tile {
    Portable,
    #[cfg(target_arch = "x86_64")]
    Avx2(avx2::Avx2),
    #[cfg(target_arch = "aarch64")]
    Neon(neon::Neon),
}

impl Tile {
    fn of(kernel: Kernel) -> Self {
        match kernel {
            #[cfg(target_arch = "x86_64")]
            Kernel::Avx2 => avx2::Avx2::detect().map_or(Self::Portable, Self::Avx2),
            #[cfg(target_arch = "aarch64")]
            Kernel::Neon => Self::Neon(neon::Neon::detect()),
            _ => Self::Portable,
        }
    }
}
