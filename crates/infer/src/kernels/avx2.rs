//! The AVX2 int8 kernel (ADR-0086 section 5): the block sums of a whole linear layer, eight
//! output channels at a time, one output channel per `i32` lane so no lane is ever summed
//! across.
//!
//! - **Pairs.** Inputs and weights are sign-extended to `i16` (`VPMOVSXBW`) and kept as pairs
//!   of neighbouring channels, one pair per `i32`. The token rows are widened once per layer
//!   call; a tile of eight weight rows is widened and transposed once per tile, so that one
//!   256-bit load holds the same pair of eight output channels.
//! - **Products.** For each pair, the token row's pair is broadcast to all lanes and
//!   `VPMADDWD` multiplies it with the eight channels' pairs and adds the two products: lane `j`
//!   gains `x[2p] * w_j[2p] + x[2p + 1] * w_j[2p + 1]`. Each product is at most `2^14` in
//!   magnitude and their sum `2^15`, so `VPMADDWD` neither saturates nor rounds. Summing the
//!   pairs of a block in `i32` gives exactly that block's sum: far below `2^31` for any block
//!   in use, and integer addition does not depend on order.
//! - **Blocks.** Pairs never straddle a block because the dispatcher sends only even block
//!   sizes here. An odd last channel of a row is paired with a zero, which adds nothing.
//! - Four token rows share each weight load. Lanes of channels past the layer's last output
//!   hold zero weights and are ignored by the caller.
//!
//! So every block sum equals [`super::portable::block_sums`] exactly.
//!
//! The only `unsafe` here: calling the `#[target_feature]` function (sound once
//! [`Avx2::detect`] has seen the feature) and unaligned loads and stores on slices of the
//! right length.

use std::arch::x86_64::{
    __m128i, __m256i, _mm_loadu_si128, _mm256_add_epi32, _mm256_cvtepi8_epi16, _mm256_loadu_si256,
    _mm256_madd_epi16, _mm256_permute2x128_si256, _mm256_set1_epi32, _mm256_setzero_si256,
    _mm256_storeu_si256, _mm256_unpackhi_epi32, _mm256_unpackhi_epi64, _mm256_unpacklo_epi32,
    _mm256_unpacklo_epi64,
};

use super::{QuantRows, TILE};

/// Token rows that share one load of the weights.
const ROWS: usize = 4;

/// Proof that the CPU has AVX2: only [`Avx2::detect`] makes one.
#[derive(Debug, Clone, Copy)]
pub(super) struct Avx2(());

impl Avx2 {
    /// `Some` if this CPU has AVX2.
    pub(super) fn detect() -> Option<Self> {
        std::arch::is_x86_feature_detected!("avx2").then_some(Self(()))
    }

    /// The block sums of the linear layer `x W^T` (`w` of shape `[out, x.width]`): for each
    /// tile of [`TILE`] output channels from `o0` and each token row `r`, calls `emit(o0, r,
    /// sums)` with `sums[b * TILE + j]` the sum of block `b` of row `r` times weight row
    /// `o0 + j` (0 for `o0 + j >= out`).
    ///
    /// # Panics
    /// If `x.block` is odd or `w` is not `out` rows of `x.width`.
    // `self` is unused but required: it is the proof that the feature was detected.
    #[allow(clippy::unused_self)]
    pub(super) fn linear(
        self,
        x: &QuantRows,
        w: &[i8],
        out: usize,
        emit: &mut impl FnMut(usize, usize, &[i32]),
    ) {
        assert!(x.block.is_multiple_of(2), "avx2: odd block size");
        assert_eq!(w.len(), out * x.width, "avx2: weight is not [out, in]");
        // SAFETY: `self` exists only if `detect` found AVX2 on this CPU, which is all the
        // `#[target_feature]` function requires; the shapes are checked above.
        unsafe { linear(x, w, out, emit) }
    }
}

/// [`super::portable::quantize_rows`] compiled with AVX2 enabled: the same code, so the same
/// result (no fused multiply-add is formed; `round` and `max` have one exact result), with the
/// rounding done in registers instead of by a call to `roundf`.
// `token` is unused but required: it is the proof that the feature was detected.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn quantize_rows(token: Avx2, x: &[f32], width: usize, block: usize) -> QuantRows {
    let _ = token;
    // SAFETY: `token` exists only if `detect` found AVX2 on this CPU, which is all the
    // `#[target_feature]` function requires.
    unsafe { quantize_rows_avx2(x, width, block) }
}

#[target_feature(enable = "avx2")]
fn quantize_rows_avx2(x: &[f32], width: usize, block: usize) -> QuantRows {
    super::portable::quantize_rows(x, width, block)
}

/// [`super::portable::scores`] compiled with AVX2 enabled: the same code and the same order of
/// operations, so the same result (no fused multiply-add is formed), with the eight running
/// sums of [`super::portable::dot`] in one register.
// `token` is unused but required: it is the proof that the feature was detected.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn scores(token: Avx2, q: &[f32], keys: &[f32], stride: usize, s: f32, out: &mut [f32]) {
    let _ = token;
    // SAFETY: `token` exists only if `detect` found AVX2 on this CPU, which is all the
    // `#[target_feature]` function requires.
    unsafe { scores_avx2(q, keys, stride, s, out) }
}

#[target_feature(enable = "avx2")]
fn scores_avx2(q: &[f32], keys: &[f32], stride: usize, scale: f32, out: &mut [f32]) {
    super::portable::scores(q, keys, stride, scale, out);
}

/// [`super::portable::weighted_sum`] compiled with AVX2 enabled: element-wise products and
/// sums in the same order, so the same result.
// `token` is unused but required: it is the proof that the feature was detected.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn weighted_sum(token: Avx2, p: &[f32], values: &[f32], stride: usize, out: &mut [f32]) {
    let _ = token;
    // SAFETY: `token` exists only if `detect` found AVX2 on this CPU, which is all the
    // `#[target_feature]` function requires.
    unsafe { weighted_sum_avx2(p, values, stride, out) }
}

#[target_feature(enable = "avx2")]
fn weighted_sum_avx2(p: &[f32], values: &[f32], stride: usize, out: &mut [f32]) {
    super::portable::weighted_sum(p, values, stride, out);
}

/// [`super::portable::gelu`] compiled with AVX2 enabled: the same `f32` operations in the same
/// order (no fused multiply-add is formed; the branches become selects), eight values at a time.
// `token` is unused but required: it is the proof that the feature was detected.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn gelu(token: Avx2, x: &mut [f32]) {
    let _ = token;
    // SAFETY: `token` exists only if `detect` found AVX2 on this CPU, which is all the
    // `#[target_feature]` function requires.
    unsafe { gelu_avx2(x) }
}

#[target_feature(enable = "avx2")]
fn gelu_avx2(x: &mut [f32]) {
    super::portable::gelu(x);
}

/// Loads 16 bytes.
#[inline]
#[target_feature(enable = "avx2")]
fn load16(s: &[i8; 16]) -> __m128i {
    // SAFETY: `s` is 16 readable bytes; `loadu` has no alignment requirement.
    unsafe { _mm_loadu_si128(s.as_ptr().cast()) }
}

/// Loads eight `i32`.
#[inline]
#[target_feature(enable = "avx2")]
fn load8(s: &[i32; 8]) -> __m256i {
    // SAFETY: `s` is 32 readable bytes; `loadu` has no alignment requirement.
    unsafe { _mm256_loadu_si256(s.as_ptr().cast()) }
}

/// Stores eight `i32`.
#[inline]
#[target_feature(enable = "avx2")]
fn store8(d: &mut [i32; 8], v: __m256i) {
    // SAFETY: `d` is 32 writable bytes; `storeu` has no alignment requirement.
    unsafe { _mm256_storeu_si256(d.as_mut_ptr().cast(), v) }
}

/// Two `i8` sign-extended to `i16` and packed into one `i32`, the first in the low half: the
/// layout `VPMOVSXBW` gives a pair of neighbouring bytes.
fn pair(lo: i8, hi: i8) -> i32 {
    let [a, b] = i16::from(lo).to_le_bytes();
    let [c, d] = i16::from(hi).to_le_bytes();
    i32::from_le_bytes([a, b, c, d])
}

/// The pairs of `src` into `dst` (`src.len().div_ceil(2)` values), the odd last one paired
/// with zero.
#[target_feature(enable = "avx2")]
fn widen(src: &[i8], dst: &mut [i32]) {
    let (chunks, tail) = src.as_chunks::<16>();
    let (dst_full, dst_tail) = dst.split_at_mut(chunks.len() * 8);
    for (c, d) in chunks.iter().zip(dst_full.as_chunks_mut::<8>().0) {
        store8(d, _mm256_cvtepi8_epi16(load16(c)));
    }
    for (d, p) in dst_tail.iter_mut().zip(tail.chunks(2)) {
        *d = pair(p[0], p.get(1).copied().unwrap_or(0));
    }
}

/// Eight vectors of eight `i32` transposed: lane `j` of output `p` is lane `p` of input `j`.
#[inline]
#[target_feature(enable = "avx2")]
fn transpose(v: &[__m256i; 8]) -> [__m256i; 8] {
    let t = [
        _mm256_unpacklo_epi32(v[0], v[1]),
        _mm256_unpackhi_epi32(v[0], v[1]),
        _mm256_unpacklo_epi32(v[2], v[3]),
        _mm256_unpackhi_epi32(v[2], v[3]),
        _mm256_unpacklo_epi32(v[4], v[5]),
        _mm256_unpackhi_epi32(v[4], v[5]),
        _mm256_unpacklo_epi32(v[6], v[7]),
        _mm256_unpackhi_epi32(v[6], v[7]),
    ];
    let u = [
        _mm256_unpacklo_epi64(t[0], t[2]),
        _mm256_unpackhi_epi64(t[0], t[2]),
        _mm256_unpacklo_epi64(t[1], t[3]),
        _mm256_unpackhi_epi64(t[1], t[3]),
        _mm256_unpacklo_epi64(t[4], t[6]),
        _mm256_unpackhi_epi64(t[4], t[6]),
        _mm256_unpacklo_epi64(t[5], t[7]),
        _mm256_unpackhi_epi64(t[5], t[7]),
    ];
    [
        _mm256_permute2x128_si256::<0x20>(u[0], u[4]),
        _mm256_permute2x128_si256::<0x20>(u[1], u[5]),
        _mm256_permute2x128_si256::<0x20>(u[2], u[6]),
        _mm256_permute2x128_si256::<0x20>(u[3], u[7]),
        _mm256_permute2x128_si256::<0x31>(u[0], u[4]),
        _mm256_permute2x128_si256::<0x31>(u[1], u[5]),
        _mm256_permute2x128_si256::<0x31>(u[2], u[6]),
        _mm256_permute2x128_si256::<0x31>(u[3], u[7]),
    ]
}

/// The weight rows `rows` (up to [`TILE`] rows of `k`) as pairs, pair-major: `dst[p][j]` is
/// pair `p` of row `j`, zero for `j >= rows.len()`.
#[target_feature(enable = "avx2")]
fn pack(rows: &[&[i8]], k: usize, dst: &mut [[i32; 8]]) {
    let full = k / 16;
    for (c, d) in dst.as_chunks_mut::<8>().0.iter_mut().take(full).enumerate() {
        let v: [__m256i; 8] = std::array::from_fn(|j| {
            rows.get(j).map_or(_mm256_setzero_si256(), |r| {
                _mm256_cvtepi8_epi16(load16(&r.as_chunks::<16>().0[c]))
            })
        });
        for (d, v) in d.iter_mut().zip(transpose(&v)) {
            store8(d, v);
        }
    }
    for (p, d) in dst.iter_mut().enumerate().skip(full * 8) {
        for (j, d) in d.iter_mut().enumerate() {
            *d = rows.get(j).map_or(0, |r| {
                pair(r[2 * p], r.get(2 * p + 1).copied().unwrap_or(0))
            });
        }
    }
}

/// The block sums of `R` token rows (pairs `xs[i]`) against the packed tile `wp` (`half`
/// pairs per block), into `sums[i]` (`blocks * TILE` each).
#[inline]
#[target_feature(enable = "avx2")]
fn rows_tile<const R: usize>(
    xs: [&[i32]; R],
    wp: &[[i32; 8]],
    half: usize,
    sums: &mut [&mut [i32]; R],
) {
    for (b, block) in wp.chunks(half).enumerate() {
        let start = b * half;
        let mut acc = [_mm256_setzero_si256(); R];
        for (p, wv) in block.iter().enumerate() {
            let wv = load8(wv);
            for (a, x) in acc.iter_mut().zip(&xs) {
                let xv = _mm256_set1_epi32(x[start + p]);
                *a = _mm256_add_epi32(*a, _mm256_madd_epi16(xv, wv));
            }
        }
        for (s, a) in sums.iter_mut().zip(acc) {
            let (d, _) = s[b * TILE..].as_chunks_mut::<8>();
            store8(&mut d[0], a);
        }
    }
}

#[allow(clippy::many_single_char_names)]
#[target_feature(enable = "avx2")]
fn linear(x: &QuantRows, w: &[i8], out: usize, emit: &mut impl FnMut(usize, usize, &[i32])) {
    let (k, rows, blocks) = (x.width, x.rows(), x.blocks());
    let kp = k.div_ceil(2);
    let half = x.block / 2;
    let mut xw = vec![0i32; rows * kp];
    for (src, dst) in x.values.chunks_exact(k).zip(xw.chunks_exact_mut(kp)) {
        widen(src, dst);
    }
    let mut wp = vec![[0i32; 8]; kp];
    let mut sums: [Vec<i32>; ROWS] = std::array::from_fn(|_| vec![0i32; blocks * TILE]);
    for o0 in (0..out).step_by(TILE) {
        let tile: Vec<&[i8]> = w[o0 * k..out.min(o0 + TILE) * k].chunks_exact(k).collect();
        pack(&tile, k, &mut wp);
        let mut r = 0;
        while r + ROWS <= rows {
            let xs: [&[i32]; ROWS] = std::array::from_fn(|i| &xw[(r + i) * kp..(r + i + 1) * kp]);
            let mut s = sums.each_mut().map(Vec::as_mut_slice);
            rows_tile::<ROWS>(xs, &wp, half, &mut s);
            for (i, s) in s.iter().enumerate() {
                emit(o0, r + i, s);
            }
            r += ROWS;
        }
        for r in r..rows {
            let s = sums[0].as_mut_slice();
            rows_tile::<1>([&xw[r * kp..(r + 1) * kp]], &wp, half, &mut [&mut *s]);
            emit(o0, r, s);
        }
    }
}
