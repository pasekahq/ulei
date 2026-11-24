//! The portable kernels: plain Rust, no SIMD and no `unsafe`. They are the reference every
//! later kernel is checked against (ADR-0086 section 5): the fp32 product of E1 and the int8
//! linear layer with block-wise activation scales of E3.

use super::QuantRows;

/// Dot product of two equal-length slices, with eight running sums.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let (ca, ra) = a.as_chunks::<8>();
    let (cb, rb) = b.as_chunks::<8>();
    for (x, y) in ca.iter().zip(cb) {
        for ((s, xi), yi) in acc.iter_mut().zip(x).zip(y) {
            *s += xi * yi;
        }
    }
    let mut tail = 0.0f32;
    for (x, y) in ra.iter().zip(rb) {
        tail += x * y;
    }
    ((acc[0] + acc[4]) + (acc[1] + acc[5])) + ((acc[2] + acc[6]) + (acc[3] + acc[7])) + tail
}

/// A linear layer on `x` (rows of `w_in` values): `y = x W^T + b` with `w` of shape
/// `[out, w_in]` row-major (the `PyTorch` layout) and `b` of `out` values.
pub fn linear(x: &[f32], w: &[f32], b: &[f32], w_in: usize) -> Vec<f32> {
    let out = b.len();
    let mut y = Vec::with_capacity(x.len() / w_in * out);
    for row in x.chunks_exact(w_in) {
        for (w_row, bias) in w.chunks_exact(w_in).zip(b) {
            y.push(dot(row, w_row) + bias);
        }
    }
    y
}

/// See [`super::quantize`].
///
/// # Panics
/// If `q` and `x` differ in length.
#[allow(clippy::cast_possible_truncation)]
pub fn quantize(x: &[f32], q: &mut [i8]) -> f32 {
    assert_eq!(x.len(), q.len(), "quantize: lengths differ");
    let max = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let mut scale = max / 127.0;
    if scale == 0.0 {
        scale = 1.0;
    }
    for (o, v) in q.iter_mut().zip(x) {
        // In range after the clamp, so the cast is exact.
        *o = (v / scale).round().clamp(-127.0, 127.0) as i8;
    }
    scale
}

/// See [`super::quantize_rows`].
///
/// # Panics
/// If `width` or `block` is 0 or `x` is not a whole number of rows.
pub fn quantize_rows(x: &[f32], width: usize, block: usize) -> QuantRows {
    assert!(width > 0 && block > 0, "quantize_rows: zero width or block");
    assert!(
        x.len().is_multiple_of(width),
        "quantize_rows: {} values are not rows of {width}",
        x.len()
    );
    let mut values = vec![0i8; x.len()];
    let mut scales = Vec::with_capacity(x.len() / width * width.div_ceil(block));
    for (row, q_row) in x.chunks_exact(width).zip(values.chunks_exact_mut(width)) {
        for (xb, qb) in row.chunks(block).zip(q_row.chunks_mut(block)) {
            scales.push(quantize(xb, qb));
        }
    }
    QuantRows {
        values,
        scales,
        width,
        block,
    }
}

/// Dot product of two equal-length int8 slices, summed exactly in `i32` (exact for up to
/// 133 000 products of values in `[-127, 127]`).
pub fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| i32::from(x) * i32::from(y))
        .sum()
}

/// See [`super::linear_i8`].
///
/// # Panics
/// If `w`, `w_scale` and `b` disagree with `x.width` and `b.len()`.
#[allow(clippy::cast_precision_loss, clippy::many_single_char_names)]
pub fn linear_i8(x: &QuantRows, w: &[i8], w_scale: &[f32], b: &[f32]) -> Vec<f32> {
    let (k, out) = (x.width, b.len());
    assert_eq!(w.len(), out * k, "linear_i8: weight is not [out, in]");
    assert_eq!(w_scale.len(), out, "linear_i8: one weight scale per output");
    let blocks = x.blocks();
    let mut y = Vec::with_capacity(x.rows() * out);
    for (row, xs) in x.values.chunks_exact(k).zip(x.scales.chunks_exact(blocks)) {
        for ((w_row, ws), bias) in w.chunks_exact(k).zip(w_scale).zip(b) {
            let mut acc = 0.0f32;
            for ((xb, wb), s) in row.chunks(x.block).zip(w_row.chunks(x.block)).zip(xs) {
                // A block sum is below 2^24 in magnitude for blocks up to 1040 channels, so the
                // conversion is exact for every block size in use.
                acc += s * dot_i8(xb, wb) as f32;
            }
            y.push(acc * ws + bias);
        }
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_is_x_w_transposed_plus_b() {
        // x: 2x3, w: 4x3 (out x in).
        let x = [1.0, 2.0, 3.0, -1.0, 0.0, 1.0];
        let w = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0];
        let b = [0.5, 0.0, 0.0, -1.0];
        let y = linear(&x, &w, &b, 3);
        assert_eq!(y, vec![1.5, 2.0, 3.0, 5.0, -0.5, 0.0, 1.0, -1.0]);
    }

    #[test]
    fn dot_handles_tails() {
        for n in 0..40u16 {
            let a: Vec<f32> = (0..n).map(|i| f32::from(i) * 0.5).collect();
            let b: Vec<f32> = (0..n).map(|i| 1.0 - f32::from(i) * 0.25).collect();
            let want: f64 = a
                .iter()
                .zip(&b)
                .map(|(x, y)| f64::from(*x) * f64::from(*y))
                .sum();
            assert!((f64::from(dot(&a, &b)) - want).abs() < 1e-3, "n = {n}");
        }
    }

    /// A deterministic pseudo-random fill in [-1, 1).
    #[allow(clippy::cast_precision_loss)]
    fn fill(seed: u64, n: usize) -> Vec<f32> {
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 40) as f32 / (1u64 << 23) as f32 - 1.0
            })
            .collect()
    }

    #[test]
    fn quantize_rounds_half_away_from_zero_and_clamps() {
        // max 127 -> scale 1: values are rounded as they are.
        let x = [127.0, 0.5, -0.5, 1.5, -2.5, 0.49, -126.6];
        let mut q = [0i8; 7];
        assert!((quantize(&x, &mut q) - 1.0).abs() < f32::EPSILON);
        assert_eq!(q, [127, 1, -1, 2, -3, 0, -127]);
        // All zeros: scale 1, all zero.
        let mut q = [5i8; 3];
        assert!((quantize(&[0.0; 3], &mut q) - 1.0).abs() < f32::EPSILON);
        assert_eq!(q, [0; 3]);
        // The extreme maps to +-127 exactly, whatever the sign.
        let mut q = [0i8; 3];
        let s = quantize(&[-3.0, 1.0, 0.0], &mut q);
        assert_eq!(q[0], -127);
        assert!((s - 3.0 / 127.0).abs() < 1e-9);
        assert_eq!(q[1], 42); // 1 / (3/127) = 42.33
    }

    #[test]
    fn quantize_rows_has_one_scale_per_row_and_block() {
        // Two rows of 5 with blocks of 2: three blocks per row, the last one short.
        let x = [1.0, -2.0, 4.0, 0.0, 8.0, 0.0, 0.0, 0.5, -0.25, 0.0];
        let q = quantize_rows(&x, 5, 2);
        assert_eq!((q.rows(), q.blocks()), (2, 3));
        // An all-zero block gets scale 1.
        let want = [2.0 / 127.0, 4.0 / 127.0, 8.0 / 127.0, 1.0, 0.5 / 127.0, 1.0];
        for (s, w) in q.scales.iter().zip(want) {
            assert!((s - w).abs() < 1e-9, "{s} vs {w}");
        }
        assert_eq!(q.values, vec![64, -127, 127, 0, 127, 0, 0, 127, -64, 0]);
        // Dequantised values are within half a step of the input.
        let x = fill(3, 7 * 100);
        let q = quantize_rows(&x, 100, 64);
        for (r, row) in x.as_chunks::<100>().0.iter().enumerate() {
            for (k, v) in row.iter().enumerate() {
                let s = q.scales[r * 2 + k / 64];
                let d = f32::from(q.values[r * 100 + k]) * s;
                assert!((d - v).abs() <= s * 0.5 + 1e-7);
            }
        }
    }

    /// `y = x W^T + b` from the dequantised operands in `f64`: the naive reference.
    #[allow(clippy::many_single_char_names)]
    fn reference(x: &QuantRows, w: &[i8], ws: &[f32], b: &[f32]) -> Vec<f64> {
        let (k, blocks) = (x.width, x.blocks());
        let mut y = Vec::new();
        for r in 0..x.rows() {
            for (o, bias) in b.iter().enumerate() {
                let mut acc = 0.0f64;
                for i in 0..k {
                    let xv = f64::from(x.values[r * k + i])
                        * f64::from(x.scales[r * blocks + i / x.block]);
                    let wv = f64::from(w[o * k + i]) * f64::from(ws[o]);
                    acc += xv * wv;
                }
                y.push(acc + f64::from(*bias));
            }
        }
        y
    }

    #[test]
    fn linear_i8_with_exact_scales_equals_the_f32_product() {
        // Scales that are powers of two make every dequantised operand and product exact, so
        // the kernel must agree with the plain f32 linear layer on the dequantised values.
        let x = QuantRows {
            values: vec![1, -2, 3, 127, -127, 0, 5, 6],
            scales: vec![0.5, 0.25, 2.0, 0.125],
            width: 4,
            block: 2,
        };
        let w = [1i8, 2, 3, 4, -5, 6, -7, 8, 127, -127, 127, -127];
        let ws = [1.0f32, 0.5, 0.0625];
        let b = [0.25f32, -1.0, 0.0];
        let got = linear_i8(&x, &w, &ws, &b);
        let xd: Vec<f32> = (0..8)
            .map(|i| f32::from(x.values[i]) * x.scales[i / 2])
            .collect();
        let wd: Vec<f32> = (0..12).map(|i| f32::from(w[i]) * ws[i / 4]).collect();
        assert_eq!(got, linear(&xd, &wd, &b, 4));
    }

    #[test]
    fn linear_i8_matches_the_dequantised_reference() {
        // Widths with and without a short last block, blocks larger than the row, and the
        // FFN-out width of e5-small.
        for (seed, (rows, k, out, block)) in [
            (1, 5, 3, 1),
            (3, 64, 4, 64),
            (2, 70, 5, 64),
            (4, 129, 3, 32),
            (1, 10, 2, 64),
            (3, 1536, 6, 64),
        ]
        .into_iter()
        .enumerate()
        {
            let seed = seed as u64 * 10;
            let x = quantize_rows(&fill(seed + 1, rows * k), k, block);
            let mut w = vec![0i8; out * k];
            let mut ws = Vec::new();
            for (wr, src) in w
                .chunks_exact_mut(k)
                .zip(fill(seed + 2, out * k).chunks_exact(k))
            {
                ws.push(quantize(src, wr));
            }
            let b = fill(seed + 3, out);
            let got = linear_i8(&x, &w, &ws, &b);
            let want = reference(&x, &w, &ws, &b);
            assert_eq!(got.len(), rows * out);
            for (g, r) in got.iter().zip(&want) {
                assert!(
                    (f64::from(*g) - r).abs() <= 1e-5 * (1.0 + r.abs()),
                    "k {k} block {block}: {g} vs {r}"
                );
            }
        }
    }

    #[test]
    fn linear_i8_extreme_values_do_not_overflow() {
        // Every product is 127 * 127 or its negative, over the widest input in use.
        let k = 4096;
        let x = QuantRows {
            values: vec![-127; k],
            scales: vec![1.0; k / 64],
            width: k,
            block: 64,
        };
        let w = vec![-127i8; 2 * k];
        let got = linear_i8(&x, &w, &[1.0, -1.0], &[0.0, 0.0]);
        #[allow(clippy::cast_precision_loss)]
        let want = (127 * 127 * k) as f32;
        assert_eq!(got, vec![want, -want]);
        assert_eq!(dot_i8(&[-127; 64], &[127; 64]), -127 * 127 * 64);
    }
}
