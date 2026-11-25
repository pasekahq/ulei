//! The AVX2 int8 kernel (ADR-0087 section 5): the block sums of a whole linear layer, eight
//! output channels at a time, one output channel per `i32` lane so no lane is ever summed
//! across.
//!
//! - **Quads.** Four neighbouring input channels are one `i32`. A tile of eight weight rows is
//!   transposed once per tile (8 x 8 `i32` transpose), so one 256-bit load holds the same quad of
//!   eight output channels; the token rows are cut into quads once per layer call.
//! - **Products.** For each quad, `VPABSB` of the weights gives the unsigned operand (|w| up to
//!   128, exact as an unsigned byte), and the token row's quad, broadcast to all lanes, takes
//!   the weights' signs with `VPSIGNB` (`x` negated where `w < 0`, zero where `w = 0`): the
//!   signed operand. `VPMADDUBSW` multiplies the bytes and adds neighbouring pairs into `i16`;
//!   `VPMADDWD` by ones adds the two pairs of each lane into `i32`; `VPADDD` accumulates.
//! - **Exactness.** `|w| * sign(x, w) = w * x` exactly when `x` is in `[-127, 127]`: negating
//!   `x` cannot wrap. Each product is at most `128 * 127 = 16 256` in magnitude, a pair sum at
//!   most `32 512 < 32 767`, so `VPMADDUBSW` never saturates; no two `i16` results are ever
//!   added in `i16`. `VPMADDWD` by ones is exact, and summing a block's quads in `i32` gives
//!   exactly that block's sum (far below `2^31`; integer addition does not depend on order).
//!   The weights may hold any `i8`, `-128` included; the activations must not hold `-128`,
//!   which [`super::portable::quantize`] never produces. [`Avx2::takes`] checks it per call
//!   (one pass over the activation bytes), and the dispatcher sends anything else to the
//!   portable kernel.
//! - **Blocks.** Quads never straddle a block because only blocks that are a multiple of 4 come
//!   here. A row's last quad is padded with zeros, which add nothing.
//! - Four token rows share each weight load. Lanes of channels past the layer's last output
//!   hold zero weights and are ignored by the caller.
//!
//! So every block sum equals [`super::portable::block_sums`] exactly.
//!
//! The only `unsafe` here: calling the `#[target_feature]` function (sound once
//! [`Avx2::detect`] has seen the feature) and unaligned loads and stores on slices of the
//! right length.

use std::arch::x86_64::{
    __m256i, _mm256_abs_epi8, _mm256_add_epi32, _mm256_loadu_si256, _mm256_madd_epi16,
    _mm256_maddubs_epi16, _mm256_permute2x128_si256, _mm256_set1_epi16, _mm256_set1_epi32,
    _mm256_setzero_si256, _mm256_sign_epi8, _mm256_storeu_si256, _mm256_unpackhi_epi32,
    _mm256_unpackhi_epi64, _mm256_unpacklo_epi32, _mm256_unpacklo_epi64,
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

    /// Whether [`Avx2::linear`] takes `x`: its block size is a multiple of 4 and no activation
    /// is `-128` (the exactness condition of the module documentation).
    // `self` is unused but required: it is the proof that the feature was detected.
    #[allow(clippy::unused_self)]
    pub(super) fn takes(self, x: &QuantRows) -> bool {
        x.block.is_multiple_of(4) && !x.values.contains(&i8::MIN)
    }

    /// The block sums of the linear layer `x W^T` (`w` of shape `[out, x.width]`): for each
    /// tile of [`TILE`] output channels from `o0` and each token row `r`, calls `emit(o0, r,
    /// sums)` with `sums[b * TILE + j]` the sum of block `b` of row `r` times weight row
    /// `o0 + j` (0 for `o0 + j >= out`).
    ///
    /// # Panics
    /// If [`Avx2::takes`] refuses `x` or `w` is not `out` rows of `x.width`.
    pub(super) fn linear(
        self,
        x: &QuantRows,
        w: &[i8],
        out: usize,
        emit: &mut impl FnMut(usize, usize, &[i32]),
    ) {
        assert!(self.takes(x), "avx2: block not a multiple of 4, or -128");
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

/// Loads eight `i32`.
#[inline]
#[target_feature(enable = "avx2")]
fn load8(s: &[i32; 8]) -> __m256i {
    // SAFETY: `s` is 32 readable bytes; `loadu` has no alignment requirement.
    unsafe { _mm256_loadu_si256(s.as_ptr().cast()) }
}

/// Loads 32 bytes.
#[inline]
#[target_feature(enable = "avx2")]
fn load32(s: &[i8; 32]) -> __m256i {
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

/// Up to four `i8` as one `i32`, the first in the low byte (the order of the bytes in memory),
/// missing ones zero.
fn quad(s: &[i8]) -> i32 {
    let mut b = [0u8; 4];
    for (d, v) in b.iter_mut().zip(s) {
        *d = v.to_le_bytes()[0];
    }
    i32::from_le_bytes(b)
}

/// The quads of `src` into `dst` (`src.len().div_ceil(4)` values), the last one padded with
/// zeros.
fn quads(src: &[i8], dst: &mut [i32]) {
    for (d, q) in dst.iter_mut().zip(src.chunks(4)) {
        *d = quad(q);
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

/// The weight rows `rows` (up to [`TILE`] rows of `k`) as quads, quad-major: `dst[p][j]` is
/// quad `p` of row `j`, zero for `j >= rows.len()`.
#[target_feature(enable = "avx2")]
fn pack(rows: &[&[i8]], k: usize, dst: &mut [[i32; 8]]) {
    let full = k / 32;
    for (c, d) in dst.as_chunks_mut::<8>().0.iter_mut().take(full).enumerate() {
        let v: [__m256i; 8] = std::array::from_fn(|j| {
            rows.get(j).map_or(_mm256_setzero_si256(), |r| {
                load32(&r.as_chunks::<32>().0[c])
            })
        });
        for (d, v) in d.iter_mut().zip(transpose(&v)) {
            store8(d, v);
        }
    }
    for (p, d) in dst.iter_mut().enumerate().skip(full * 8) {
        for (j, d) in d.iter_mut().enumerate() {
            *d = rows.get(j).map_or(0, |r| quad(&r[4 * p..k.min(4 * p + 4)]));
        }
    }
}

/// The block sums of `R` token rows (quads `xs[i]`) against the packed tile `wp` (`qb`
/// quads per block), into `sums[i]` (`blocks * TILE` each).
#[inline]
#[target_feature(enable = "avx2")]
fn rows_tile<const R: usize>(
    xs: [&[i32]; R],
    wp: &[[i32; 8]],
    qb: usize,
    sums: &mut [&mut [i32]; R],
) {
    let ones = _mm256_set1_epi16(1);
    for (b, block) in wp.chunks(qb).enumerate() {
        let xb: [&[i32]; R] = xs.map(|x| &x[b * qb..b * qb + block.len()]);
        let mut acc = [_mm256_setzero_si256(); R];
        for (p, wv) in block.iter().enumerate() {
            let wv = load8(wv);
            let aw = _mm256_abs_epi8(wv);
            for (a, x) in acc.iter_mut().zip(&xb) {
                let sx = _mm256_sign_epi8(_mm256_set1_epi32(x[p]), wv);
                let pairs = _mm256_maddubs_epi16(aw, sx);
                *a = _mm256_add_epi32(*a, _mm256_madd_epi16(pairs, ones));
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
    let kq = k.div_ceil(4);
    let qb = x.block / 4;
    let mut xq = vec![0i32; rows * kq];
    for (src, dst) in x.values.chunks_exact(k).zip(xq.chunks_exact_mut(kq)) {
        quads(src, dst);
    }
    let mut wp = vec![[0i32; 8]; kq];
    let mut sums: [Vec<i32>; ROWS] = std::array::from_fn(|_| vec![0i32; blocks * TILE]);
    for o0 in (0..out).step_by(TILE) {
        let tile: Vec<&[i8]> = w[o0 * k..out.min(o0 + TILE) * k].chunks_exact(k).collect();
        pack(&tile, k, &mut wp);
        let mut r = 0;
        while r + ROWS <= rows {
            let xs: [&[i32]; ROWS] = std::array::from_fn(|i| &xq[(r + i) * kq..(r + i + 1) * kq]);
            let mut s = sums.each_mut().map(Vec::as_mut_slice);
            rows_tile::<ROWS>(xs, &wp, qb, &mut s);
            for (i, s) in s.iter().enumerate() {
                emit(o0, r + i, s);
            }
            r += ROWS;
        }
        for r in r..rows {
            let s = sums[0].as_mut_slice();
            rows_tile::<1>([&xq[r * kq..(r + 1) * kq]], &wp, qb, &mut [&mut *s]);
            emit(o0, r, s);
        }
    }
}
