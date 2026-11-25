//! The AVX2 int8 kernel (ADR-0087 section 5): a whole linear layer, eight output channels at a
//! time, one output channel per `i32` lane so no lane is ever summed across.
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
//! - **Rows per tile (E7k).** [`ROWS`] token rows share each load of a weight quad; the packed
//!   tile (`k / 4` quads of 32 bytes) is read once per group of rows. Lanes of channels past
//!   the layer's last output hold zero weights and are never stored.
//!
//! So every block sum equals [`super::portable::block_sums`] exactly.
//!
//! - **The `f32` half in registers (E7k).** Each block sum is converted (exact: below `2^24`),
//!   times its row's block scale, and added to the output's `f32` accumulator as soon as the
//!   block is done, from `0.0` in block order; at the end `acc * w_scale + bias`. These are the
//!   operations of [`super::combine`] in its order, one output per lane, with no fused
//!   multiply-add, so the outputs are those of [`super::portable::linear_i8`] bit for bit; the
//!   block sums are never stored.
//!
//! The only `unsafe` here: calling the `#[target_feature]` function (sound once
//! [`Avx2::detect`] has seen the feature) and unaligned loads and stores on slices of the
//! right length.

use std::arch::x86_64::{
    __m256, __m256i, _mm256_abs_epi8, _mm256_add_epi32, _mm256_add_ps, _mm256_cvtepi32_ps,
    _mm256_loadu_ps, _mm256_loadu_si256, _mm256_madd_epi16, _mm256_maddubs_epi16, _mm256_mul_ps,
    _mm256_permute2x128_si256, _mm256_set1_epi16, _mm256_set1_epi32, _mm256_set1_ps,
    _mm256_setzero_ps, _mm256_setzero_si256, _mm256_sign_epi8, _mm256_storeu_ps,
    _mm256_storeu_si256, _mm256_unpackhi_epi32, _mm256_unpackhi_epi64, _mm256_unpacklo_epi32,
    _mm256_unpacklo_epi64,
};

use super::{Quant, ROWS, Scratch, TILE, grown};
use crate::ops::Softmax;

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
    pub(super) fn takes(self, x: &Quant<'_>) -> bool {
        x.block.is_multiple_of(4) && !x.values.contains(&i8::MIN)
    }

    /// The int8 linear layer `y = x W^T + b` (`w` of shape `[out, x.width]`, one weight scale
    /// per output in `w_scale`, `out = b.len()`) into `y` (`x.rows() x out`, every value
    /// written), bit-identical to [`super::portable::linear_i8`]. Its working buffers are those of
    /// `scratch`.
    ///
    /// # Panics
    /// If [`Avx2::takes`] refuses `x` or the shapes disagree.
    pub(super) fn linear(
        self,
        x: &Quant<'_>,
        w: &[i8],
        w_scale: &[f32],
        b: &[f32],
        y: &mut [f32],
        scratch: &mut Scratch,
    ) {
        let out = b.len();
        assert!(self.takes(x), "avx2: block not a multiple of 4, or -128");
        assert_eq!(w.len(), out * x.width, "avx2: weight is not [out, in]");
        assert_eq!(w_scale.len(), out, "avx2: one weight scale per output");
        assert_eq!(y.len(), x.rows() * out, "avx2: output is not [rows, out]");
        assert_eq!(
            x.scales.len(),
            x.rows() * x.blocks(),
            "avx2: one scale per block"
        );
        // SAFETY: `self` exists only if `detect` found AVX2 on this CPU, which is all the
        // `#[target_feature]` function requires; the shapes are checked above.
        unsafe { linear(x, w, w_scale, b, y, scratch) }
    }

    /// The integer block sums of [`Avx2::linear`], as the portable kernel's `emit` gives them:
    /// `emit(o0, r, sums)` for each tile of [`TILE`] outputs from `o0` and token row `r`, with
    /// `sums[b * TILE + j]` the sum of block `b` of row `r` times weight row `o0 + j` (0 for
    /// `o0 + j >= out`). For the fuzz tests.
    ///
    /// # Panics
    /// If [`Avx2::takes`] refuses `x` or `w` is not `out` rows of `x.width`.
    #[cfg(test)]
    pub(super) fn block_sums(
        self,
        x: &Quant<'_>,
        w: &[i8],
        out: usize,
        emit: &mut impl FnMut(usize, usize, &[i32]),
        scratch: &mut Scratch,
    ) {
        assert!(self.takes(x), "avx2: block not a multiple of 4, or -128");
        assert_eq!(w.len(), out * x.width, "avx2: weight is not [out, in]");
        // SAFETY: as in `linear`.
        unsafe { block_sums_of(x, w, out, emit, scratch) }
    }
}

/// [`super::portable::quantize_rows_into`] compiled with AVX2 enabled: the same code, so the
/// same result (no fused multiply-add is formed; `round` and `max` have one exact result), with
/// the rounding done in registers instead of by a call to `roundf`.
// `token` is unused but required: it is the proof that the feature was detected.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn quantize_rows_into(
    token: Avx2,
    x: &[f32],
    width: usize,
    block: usize,
    values: &mut [i8],
    scales: &mut [f32],
) {
    let _ = token;
    // SAFETY: `token` exists only if `detect` found AVX2 on this CPU, which is all the
    // `#[target_feature]` function requires.
    unsafe { quantize_rows_avx2(x, width, block, values, scales) }
}

#[target_feature(enable = "avx2")]
fn quantize_rows_avx2(
    x: &[f32],
    width: usize,
    block: usize,
    values: &mut [i8],
    scales: &mut [f32],
) {
    super::portable::quantize_rows_into(x, width, block, values, scales);
}

/// [`super::portable::attention`] with AVX2: per head, the keys transposed once, so one
/// register holds one channel of eight keys and the scores of eight keys come out of one pass,
/// each in the order of operations of [`super::portable::dot`]; the probabilities of four
/// queries at a time; their weighted sums of the values in registers. The same `f32`
/// operations in the same order per value, so the same result (no fused multiply-add);
/// `softmax` is the shared source of [`crate::ops`], compiled here with AVX2. The transposed
/// keys and the probabilities live in `scratch`.
// `token` is unused but required: it is the proof that the feature was detected.
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
pub(super) fn attention(
    token: Avx2,
    query: &[f32],
    key: &[f32],
    value: &[f32],
    tokens: usize,
    heads: usize,
    scale: f32,
    softmax: Softmax,
    context: &mut [f32],
    scratch: &mut Scratch,
) {
    let _ = token;
    // SAFETY: `token` exists only if `detect` found AVX2 on this CPU, which is all the
    // `#[target_feature]` function requires.
    unsafe {
        attention_avx2(
            query,
            key,
            value,
            tokens,
            heads,
            scale,
            softmax,
            context,
            &mut scratch.keys_t,
            &mut scratch.probs,
        );
    }
}

/// Queries whose probabilities [`attention_avx2`] holds at once.
const QUERIES: usize = 4;

/// Keys per vector of scores.
const KEYS: usize = 8;

#[allow(clippy::many_single_char_names, clippy::too_many_arguments)]
#[target_feature(enable = "avx2")]
fn attention_avx2(
    query: &[f32],
    key: &[f32],
    value: &[f32],
    tokens: usize,
    heads: usize,
    scale: f32,
    softmax: Softmax,
    context: &mut [f32],
    keys_t: &mut Vec<f32>,
    p: &mut Vec<f32>,
) {
    let (hidden, width) =
        super::portable::attention_shape(query, key, value, tokens, heads, context);
    let padded = tokens.next_multiple_of(KEYS);
    // This head's keys transposed: `keys_t[d * padded + j]` is channel `d` of key `j`. The
    // columns past the last key are set to 0 here and never written by the heads: their
    // scores are computed and not used.
    let keys_t = grown(keys_t, width * padded, 0.0);
    for row in keys_t.chunks_exact_mut(padded) {
        row[tokens..].fill(0.0);
    }
    // The probabilities of up to `QUERIES` queries, rows of `padded`; `scores_t` writes them all.
    let p = grown(p, QUERIES * padded, 0.0);
    for off in (0..hidden).step_by(width) {
        for (j, k) in key.chunks_exact(hidden).enumerate() {
            for (d, &v) in k[off..off + width].iter().enumerate() {
                keys_t[d * padded + j] = v;
            }
        }
        for i0 in (0..tokens).step_by(QUERIES) {
            let n = QUERIES.min(tokens - i0);
            for (r, row) in p.chunks_exact_mut(padded).take(n).enumerate() {
                let at = (i0 + r) * hidden + off;
                scores_t(&query[at..at + width], keys_t, padded, scale, row);
                softmax.apply(&mut row[..tokens]);
            }
            let rows = &p[..n * padded];
            let ctx = &mut context[i0 * hidden..(i0 + n) * hidden];
            match width {
                32 => weighted_sum_n::<4, 2>(rows, padded, tokens, value, hidden, off, ctx),
                64 => weighted_sum_n::<8, 1>(rows, padded, tokens, value, hidden, off, ctx),
                _ => {
                    for (r, row) in rows.chunks_exact(padded).enumerate() {
                        let out = &mut ctx[r * hidden + off..r * hidden + off + width];
                        out.fill(0.0);
                        super::portable::weighted_sum(&row[..tokens], &value[off..], hidden, out);
                    }
                }
            }
        }
    }
}

/// Loads eight `f32`.
#[inline]
#[target_feature(enable = "avx2")]
fn loadf(s: &[f32]) -> __m256 {
    let s: &[f32; 8] = &s[..8].as_chunks::<8>().0[0];
    // SAFETY: `s` is 32 readable bytes; `loadu` has no alignment requirement.
    unsafe { _mm256_loadu_ps(s.as_ptr()) }
}

/// Stores eight `f32`.
#[inline]
#[target_feature(enable = "avx2")]
fn storef(d: &mut [f32], v: __m256) {
    let d: &mut [f32; 8] = &mut d[..8].as_chunks_mut::<8>().0[0];
    // SAFETY: `d` is 32 writable bytes; `storeu` has no alignment requirement.
    unsafe { _mm256_storeu_ps(d.as_mut_ptr(), v) }
}

/// The scores of query `q` against every key of `keys_t` (rows of `padded`), [`KEYS`] keys
/// per vector, one per lane, each exactly as [`super::portable::scores`] computes it: the eight
/// running sums of [`super::portable::dot`] over the channels in chunks of eight, the running
/// sum of the tail, `((s0 + s4) + (s1 + s5)) + ((s2 + s6) + (s3 + s7)) + tail`, times `scale`.
/// Writes all `padded` values of `out`; those past the last key are unused.
#[target_feature(enable = "avx2")]
fn scores_t(q: &[f32], keys_t: &[f32], padded: usize, scale: f32, out: &mut [f32]) {
    let (chunks, tail_q) = q.as_chunks::<8>();
    let full = chunks.len() * 8;
    let scale = _mm256_set1_ps(scale);
    for col in (0..padded).step_by(KEYS) {
        let lanes = |d: usize| loadf(&keys_t[d * padded + col..]);
        let mut acc = [_mm256_setzero_ps(); 8];
        for (c, qc) in chunks.iter().enumerate() {
            for (l, (a, &qv)) in acc.iter_mut().zip(qc).enumerate() {
                *a = _mm256_add_ps(*a, _mm256_mul_ps(_mm256_set1_ps(qv), lanes(c * 8 + l)));
            }
        }
        let mut tail = _mm256_setzero_ps();
        for (d, &qv) in tail_q.iter().enumerate() {
            tail = _mm256_add_ps(tail, _mm256_mul_ps(_mm256_set1_ps(qv), lanes(full + d)));
        }
        let left = _mm256_add_ps(_mm256_add_ps(acc[0], acc[4]), _mm256_add_ps(acc[1], acc[5]));
        let right = _mm256_add_ps(_mm256_add_ps(acc[2], acc[6]), _mm256_add_ps(acc[3], acc[7]));
        let sum = _mm256_add_ps(_mm256_add_ps(left, right), tail);
        storef(&mut out[col..], _mm256_mul_ps(sum, scale));
    }
}

/// The context of the queries whose probabilities are the rows of `p` (rows of `padded`) in one
/// head of `V * 8` channels from `off`, written into their rows of `ctx` (rows of `hidden`):
/// for each query and channel, `p[j] * value j` added up for `j` in order from zero, exactly as
/// [`super::portable::weighted_sum`] adds them into a zeroed output; `R` queries per pass over
/// the values, their sums held in registers.
#[target_feature(enable = "avx2")]
fn weighted_sum_n<const V: usize, const R: usize>(
    p: &[f32],
    padded: usize,
    tokens: usize,
    value: &[f32],
    hidden: usize,
    off: usize,
    ctx: &mut [f32],
) {
    let queries = p.len() / padded;
    for q0 in (0..queries).step_by(R) {
        let n = R.min(queries - q0);
        let mut acc = [[_mm256_setzero_ps(); V]; R];
        for j in 0..tokens {
            let v: [__m256; V] = std::array::from_fn(|c| loadf(&value[j * hidden + off + c * 8..]));
            for (r, a) in acc.iter_mut().enumerate().take(n) {
                let pj = _mm256_set1_ps(p[(q0 + r) * padded + j]);
                for (a, v) in a.iter_mut().zip(&v) {
                    *a = _mm256_add_ps(*a, _mm256_mul_ps(pj, *v));
                }
            }
        }
        for (r, a) in acc.iter().take(n).enumerate() {
            for (c, a) in a.iter().enumerate() {
                storef(&mut ctx[(q0 + r) * hidden + off + c * 8..], *a);
            }
        }
    }
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

/// [`crate::ops::exp_f32`] on every value of `x`, in place, compiled with AVX2: for the fuzz
/// tests of the softmax exponential.
#[cfg(test)]
#[allow(clippy::needless_pass_by_value)]
pub(super) fn exp(token: Avx2, x: &mut [f32]) {
    let _ = token;
    // SAFETY: `token` exists only if `detect` found AVX2 on this CPU, which is all the
    // `#[target_feature]` function requires.
    unsafe { exp_avx2(x) }
}

#[cfg(test)]
#[target_feature(enable = "avx2")]
fn exp_avx2(x: &mut [f32]) {
    for v in x {
        *v = crate::ops::exp_f32(*v);
    }
}

/// [`crate::ops::softmax_f32`] of each row of `len` values of `x`, compiled with AVX2: for the
/// fuzz tests.
#[cfg(test)]
#[allow(clippy::needless_pass_by_value)]
pub(super) fn softmax_rows(token: Avx2, x: &mut [f32], len: usize) {
    let _ = token;
    // SAFETY: `token` exists only if `detect` found AVX2 on this CPU, which is all the
    // `#[target_feature]` function requires.
    unsafe { softmax_rows_avx2(x, len) }
}

#[cfg(test)]
#[target_feature(enable = "avx2")]
fn softmax_rows_avx2(x: &mut [f32], len: usize) {
    for row in x.chunks_mut(len) {
        crate::ops::softmax_f32(row);
    }
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
    // Four bytes are one fixed-size load; only a row's short last quad takes the loop, which
    // the compiler makes a call to `memcpy` of a variable length (89 million calls per round
    // of the 116 probe inputs when every quad took it, ADR-0087 E6b).
    if let Ok(four) = <[i8; 4]>::try_from(s) {
        return i32::from_le_bytes(four.map(|v| v.to_le_bytes()[0]));
    }
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

/// The integer block sums of `R` token rows over one block against the packed tile: the
/// quads `xs[i]` of row `i` and `ws` of the tile (equal lengths, the block's quads). Lane `j` of
/// `[i]` is the block's sum of row `i` times weight row `j` of the tile.
#[inline]
#[target_feature(enable = "avx2")]
fn block_sums<const R: usize>(xs: [&[i32]; R], ws: &[[i32; 8]]) -> [__m256i; R] {
    let ones = _mm256_set1_epi16(1);
    let mut acc = [_mm256_setzero_si256(); R];
    for (p, wv) in ws.iter().enumerate() {
        let wv = load8(wv);
        let aw = _mm256_abs_epi8(wv);
        for (a, x) in acc.iter_mut().zip(&xs) {
            let sx = _mm256_sign_epi8(_mm256_set1_epi32(x[p]), wv);
            let pairs = _mm256_maddubs_epi16(aw, sx);
            *a = _mm256_add_epi32(*a, _mm256_madd_epi16(pairs, ones));
        }
    }
    acc
}

/// One layer call's operands for [`rows_out`]: the quads of every token row and the packed
/// tile.
#[derive(Clone, Copy)]
struct Layout<'a> {
    /// The token rows as quads, rows of `kq`.
    xq: &'a [i32],
    /// The packed tile, `kq` quads.
    wp: &'a [[i32; 8]],
    /// Quads per row.
    kq: usize,
    /// Quads per block.
    qb: usize,
    /// Blocks per row.
    blocks: usize,
}

impl<'a> Layout<'a> {
    /// The quads of block `b` of rows `r..r + R` and of the tile.
    #[inline]
    fn block<const R: usize>(&self, r: usize, b: usize) -> ([&'a [i32]; R], &'a [[i32; 8]]) {
        let q0 = b * self.qb;
        let q1 = self.kq.min(q0 + self.qb);
        (
            std::array::from_fn(|i| &self.xq[(r + i) * self.kq..][q0..q1]),
            &self.wp[q0..q1],
        )
    }
}

/// The outputs of the tile from `o0` for the `R` token rows from `r`, into `y` (rows of `out`),
/// in the order of [`super::combine`]: per row and output, from `0.0`, `acc + scale[b] * (sum
/// as f32)` for each block `b` in order, then `acc * w_scale + bias`. The tile's block sums are
/// combined in registers as they come, eight outputs per register.
#[allow(clippy::many_single_char_names)]
#[inline]
#[target_feature(enable = "avx2")]
fn rows_out<const R: usize>(
    l: &Layout<'_>,
    scales: &[f32],
    r: usize,
    (ws, bias): (__m256, __m256),
    y: &mut [f32],
    (out, o0): (usize, usize),
) {
    let mut f = [_mm256_setzero_ps(); R];
    for b in 0..l.blocks {
        let (xs, wt) = l.block::<R>(r, b);
        for (i, (f, a)) in f.iter_mut().zip(block_sums::<R>(xs, wt)).enumerate() {
            let s = _mm256_set1_ps(scales[(r + i) * l.blocks + b]);
            *f = _mm256_add_ps(*f, _mm256_mul_ps(s, _mm256_cvtepi32_ps(a)));
        }
    }
    for (i, f) in f.iter().enumerate() {
        let row = &mut y[(r + i) * out..(r + i + 1) * out];
        let v = _mm256_add_ps(_mm256_mul_ps(*f, ws), bias);
        if o0 + TILE <= out {
            storef(&mut row[o0..], v);
        } else {
            let mut tmp = [0.0f32; TILE];
            storef(&mut tmp, v);
            row[o0..].copy_from_slice(&tmp[..out - o0]);
        }
    }
}

/// Eight `f32` of `s` from `at`, zeros past its end.
#[inline]
#[target_feature(enable = "avx2")]
fn load_padded(s: &[f32], at: usize) -> __m256 {
    let mut tmp = [0.0f32; TILE];
    for (d, v) in tmp.iter_mut().zip(s.get(at..).unwrap_or(&[])) {
        *d = *v;
    }
    loadf(&tmp)
}

/// The token rows as quads into `scratch.quads` and a packer of weight tiles into
/// `scratch.packed`; calls `tile(layout, o0)` for each tile of [`TILE`] outputs from `o0`, once
/// its weights are packed.
#[target_feature(enable = "avx2")]
fn tiles(
    x: &Quant<'_>,
    w: &[i8],
    out: usize,
    scratch: &mut Scratch,
    mut tile: impl FnMut(&Layout<'_>, usize),
) {
    let (k, rows) = (x.width, x.rows());
    let kq = k.div_ceil(4);
    let xq = grown(&mut scratch.quads, rows * kq, 0);
    for (src, dst) in x.values.chunks_exact(k).zip(xq.chunks_exact_mut(kq)) {
        quads(src, dst);
    }
    let wp = grown(&mut scratch.packed, kq, [0; 8]);
    let mut rows_of: [&[i8]; TILE] = [&[]; TILE];
    for o0 in (0..out).step_by(TILE) {
        let o1 = out.min(o0 + TILE);
        for (d, row) in rows_of.iter_mut().zip(w[o0 * k..o1 * k].chunks_exact(k)) {
            *d = row;
        }
        pack(&rows_of[..o1 - o0], k, wp);
        let l = Layout {
            xq,
            wp,
            kq,
            qb: x.block / 4,
            blocks: x.blocks(),
        };
        tile(&l, o0);
    }
}

/// The whole int8 linear layer: per tile of [`TILE`] outputs, packed once, the token rows in
/// groups of [`ROWS`] (the last group shorter), each group's block sums and their `f32`
/// combination in registers. The kernel's buffers are those of `scratch`, every value written
/// before it is read: the quads of each token row (`quads` writes all `kq` of a row) and the
/// packed tile (`pack` writes all `kq` quads).
#[target_feature(enable = "avx2")]
fn linear(
    x: &Quant<'_>,
    w: &[i8],
    w_scale: &[f32],
    bias: &[f32],
    y: &mut [f32],
    scratch: &mut Scratch,
) {
    let (rows, out) = (x.rows(), bias.len());
    tiles(x, w, out, scratch, |l, o0| {
        let wb = (load_padded(w_scale, o0), load_padded(bias, o0));
        let at = (out, o0);
        let mut r = 0;
        while r + ROWS <= rows {
            rows_out::<ROWS>(l, x.scales, r, wb, y, at);
            r += ROWS;
        }
        match rows - r {
            1 => rows_out::<1>(l, x.scales, r, wb, y, at),
            2 => rows_out::<2>(l, x.scales, r, wb, y, at),
            3 => rows_out::<3>(l, x.scales, r, wb, y, at),
            _ => {}
        }
    });
}

/// The block sums of [`linear`] as the portable kernel's `emit` gives them (tests: [`linear`]
/// combines them in registers and never stores them). The same tiles and the same groups of
/// rows, through [`block_sums`].
#[cfg(test)]
#[target_feature(enable = "avx2")]
fn block_sums_of(
    x: &Quant<'_>,
    w: &[i8],
    out: usize,
    emit: &mut impl FnMut(usize, usize, &[i32]),
    scratch: &mut Scratch,
) {
    fn group<const R: usize>(
        l: &Layout<'_>,
        r: usize,
        o0: usize,
        emit: &mut impl FnMut(usize, usize, &[i32]),
    ) {
        let mut sums = vec![vec![0i32; l.blocks * TILE]; R];
        for b in 0..l.blocks {
            let (xs, wt) = l.block::<R>(r, b);
            // SAFETY: called only from `block_sums_of`, which runs with AVX2 detected.
            let acc = unsafe { block_sums::<R>(xs, wt) };
            for (s, a) in sums.iter_mut().zip(acc) {
                // SAFETY: as above.
                unsafe { store8(&mut s[b * TILE..].as_chunks_mut::<8>().0[0], a) };
            }
        }
        for (i, s) in sums.iter().enumerate() {
            emit(o0, r + i, s);
        }
    }
    let rows = x.rows();
    tiles(x, w, out, scratch, |l, o0| {
        let mut r = 0;
        while r + ROWS <= rows {
            group::<ROWS>(l, r, o0, emit);
            r += ROWS;
        }
        match rows - r {
            1 => group::<1>(l, r, o0, emit),
            2 => group::<2>(l, r, o0, emit),
            3 => group::<3>(l, r, o0, emit),
            _ => {}
        }
    });
}
