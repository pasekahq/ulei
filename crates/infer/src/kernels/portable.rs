//! The portable fp32 kernels: plain Rust, no SIMD and no `unsafe`. They are the reference every
//! later kernel is checked against (ADR-0087 section 5).

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
}
