//! Pooling of the final token states into one L2-normalised vector.

use super::Pooling;
use crate::ops;

/// Pools `hidden` (`tokens x h`, row-major) by `pooling` and L2-normalises the result.
pub(super) fn pool(pooling: Pooling, hidden: &[f32], tokens: usize, h: usize) -> Vec<f32> {
    let mut out = match pooling {
        Pooling::Cls => hidden[..h].to_vec(),
        Pooling::Mean => mean(hidden, tokens, h),
    };
    ops::l2_normalize(&mut out);
    out
}

/// The mean of the `tokens` rows of `hidden` (`tokens x h`), summed in `f64` in row order,
/// before normalisation.
#[allow(clippy::cast_precision_loss)]
fn mean(hidden: &[f32], tokens: usize, h: usize) -> Vec<f32> {
    let mut sum = vec![0.0f64; h];
    for row in hidden.chunks_exact(h) {
        for (s, v) in sum.iter_mut().zip(row) {
            *s += f64::from(*v);
        }
    }
    #[allow(clippy::cast_possible_truncation)]
    sum.iter().map(|s| (s / tokens as f64) as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_divides_by_the_token_count() {
        // L2 normalisation cancels a wrong divisor but for one rounding, so the mean is
        // checked before it (E13r: `tokens + 1` survived every default test).
        let hidden = [1.0, -2.0, 3.0, 4.0, 0.5, -1.0];
        assert_eq!(mean(&hidden, 3, 2), [1.5, 1.0 / 3.0]);
        assert_eq!(mean(&hidden[..2], 1, 2), [1.0, -2.0]);
        let pooled = pool(Pooling::Mean, &hidden, 3, 2);
        let mut want = mean(&hidden, 3, 2);
        ops::l2_normalize(&mut want);
        assert_eq!(pooled, want);
        let cls = pool(Pooling::Cls, &hidden, 3, 2);
        assert!((cls[0] - 0.447_213_6).abs() < 1e-6 && (cls[1] + 0.894_427_2).abs() < 1e-6);
    }
}
