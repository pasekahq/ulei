//! Pooling of the final token states into one L2-normalised vector.

use super::Pooling;
use crate::ops;

/// Pools `hidden` (`tokens x h`, row-major) by `pooling` and L2-normalises the result.
#[allow(clippy::cast_precision_loss)]
pub(super) fn pool(pooling: Pooling, hidden: &[f32], tokens: usize, h: usize) -> Vec<f32> {
    let mut out = match pooling {
        Pooling::Cls => hidden[..h].to_vec(),
        Pooling::Mean => {
            let mut sum = vec![0.0f64; h];
            for row in hidden.chunks_exact(h) {
                for (s, v) in sum.iter_mut().zip(row) {
                    *s += f64::from(*v);
                }
            }
            #[allow(clippy::cast_possible_truncation)]
            sum.iter().map(|s| (s / tokens as f64) as f32).collect()
        }
    };
    ops::l2_normalize(&mut out);
    out
}
