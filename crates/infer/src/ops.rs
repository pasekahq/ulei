//! Model-agnostic fp32 operations: `erf` and GELU, `LayerNorm`, softmax and L2 normalisation.
//! Plain portable Rust; the matrix products live in [`crate::kernels`].
//!
//! GELU has two forms. [`gelu`] is exact (an `f64` `erf`) and stays the fp32 reference path's
//! (milestone E1, the checking tool). [`gelu_f32`] is the int8 path's (ADR-0087 section 3): one
//! fixed `f32` formula, close to the exact one, written so that every build computes the same
//! bits.

use std::f64::consts::FRAC_2_SQRT_PI;

/// Below this the power series is used, at or above it the continued fraction.
const ERF_SPLIT: f64 = 3.0;

/// Terms of the continued fraction of `erfc`; enough for 1e-16 from [`ERF_SPLIT`] up.
const ERFC_TERMS: u32 = 80;

/// The error function, accurate to a few units in the last place of `f64`.
///
/// For `|x| < 3` it sums `erf(x) = 2/sqrt(pi) * exp(-x^2) * sum_n 2^n x^(2n+1) / (2n+1)!!`, a
/// series of positive terms (no cancellation); from 3 up it evaluates the continued fraction
/// `erfc(x) = exp(-x^2)/sqrt(pi) / (x + (1/2)/(x + 1/(x + (3/2)/(x + ...))))`.
pub fn erf(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    let a = x.abs();
    let r = if a < ERF_SPLIT {
        erf_series(a)
    } else {
        1.0 - erfc_fraction(a)
    };
    r.copysign(x)
}

fn erf_series(a: f64) -> f64 {
    let a2 = a * a;
    let mut term = a;
    let mut sum = a;
    let mut k = 1.0;
    // At a = 3 the terms fall below 1e-17 of the sum after about 60 steps.
    while term > sum * 1e-17 {
        k += 2.0;
        term *= 2.0 * a2 / k;
        sum += term;
    }
    FRAC_2_SQRT_PI * (-a2).exp() * sum
}

fn erfc_fraction(a: f64) -> f64 {
    let mut f = a;
    for k in (1..=ERFC_TERMS).rev() {
        f = a + f64::from(k) * 0.5 / f;
    }
    FRAC_2_SQRT_PI * 0.5 * (-a * a).exp() / f
}

/// GELU in its exact form, `x/2 * (1 + erf(x / sqrt 2))` (Hugging Face `gelu`).
#[allow(clippy::cast_possible_truncation)]
pub fn gelu(x: f32) -> f32 {
    let x = f64::from(x);
    (0.5 * x * (1.0 + erf(x * std::f64::consts::FRAC_1_SQRT_2))) as f32
}

/// The coefficients of [`gelu_f32`], lowest degree first. Fitted 2026-11-25 (weighted Lawson
/// iteration on the absolute GELU error, then rounded to `f32`), checked over every `f32`.
const GELU_ERF_P: [f32; 4] = [1.128_381_6, 0.147_280_93, 0.040_004_592, 0.000_555_643_23];
const GELU_ERF_Q: [f32; 4] = [1.0, 0.463_877_4, 0.090_016_85, 0.008_015_547];
const GELU_ERFC_P: [f32; 5] = [
    0.004_677_686,
    -0.010_261_694,
    0.008_611_778,
    -0.003_263_998_5,
    0.000_469_588_76,
];
const GELU_ERFC_Q: [f32; 5] = [1.0, 2.222_705_8, 2.870_086_4, 2.202_074_3, 2.281_102_7];

/// GELU in `f32`, the int8 path's activation: one explicit formula of `f32` multiplications,
/// additions, one division per branch and comparisons, no fused multiply-add and no library
/// call, so every build and instruction set computes the same bits (Rust never contracts
/// `a * b + c`). With `z = x * FRAC_1_SQRT_2` (the `f32` constant), `a = |z|`, `t = z * z`,
/// `u = a - 2`, each operation rounded in the order written:
///
/// - `a < 2`: `(0.5 * x) * (1 + (z * P1(t)) / Q1(t))`, `z P1(t) / Q1(t)` approximating
///   `erf z`;
/// - `2 <= a < 4`: with `c = P2(u) / Q2(u)` approximating `erfc a` and `h = (0.5 * x) * c`:
///   `x - h` for `x > 0`, `h` otherwise;
/// - `a >= 4` (`|x| >= 5.66`): `x` for `x > 0`, `0 * x` otherwise (`-0` for finite `x`; `NaN`
///   for `NaN` and `-inf`, as the exact form gives).
///
/// Polynomials by Horner from the highest coefficient: `P(t) = ((p3 * t + p2) * t + p1) * t +
/// p0`. Coefficients: `GELU_ERF_P`, `GELU_ERF_Q`, `GELU_ERFC_P`, `GELU_ERFC_Q`. Largest
/// absolute difference from the exact GELU over all finite `f32` inputs: `6.1e-7`, at `x =
/// 2.77` (the test `gelu_f32_error_over_every_f32`).
// Inlined so that an instruction-set kernel can compile the same code with its features
// (`kernels::gelu`).
#[allow(clippy::inline_always)]
#[inline(always)]
#[allow(clippy::many_single_char_names)]
pub fn gelu_f32(x: f32) -> f32 {
    let [p0, p1, p2, p3] = GELU_ERF_P;
    let [_, q1, q2, q3] = GELU_ERF_Q;
    let [r0, r1, r2, r3, r4] = GELU_ERFC_P;
    let [_, s1, s2, s3, s4] = GELU_ERFC_Q;
    let z = x * std::f32::consts::FRAC_1_SQRT_2;
    let a = z.abs();
    let half = 0.5 * x;
    let t = z * z;
    let erf = (z * (((p3 * t + p2) * t + p1) * t + p0)) / (((q3 * t + q2) * t + q1) * t + 1.0);
    let near = half * (1.0 + erf);
    let u = a - 2.0;
    let erfc = ((((r4 * u + r3) * u + r2) * u + r1) * u + r0)
        / ((((s4 * u + s3) * u + s2) * u + s1) * u + 1.0);
    let h = half * erfc;
    let mid = if x > 0.0 { x - h } else { h };
    let far = if x > 0.0 { x } else { 0.0 * x };
    if a < 2.0 {
        near
    } else if a < 4.0 {
        mid
    } else {
        far
    }
}

/// `LayerNorm` over each row of `x` (rows of `gamma.len()` values), in place: the biased
/// variance, `(x - mean) / sqrt(var + eps) * gamma + beta`. Sums are taken in `f64`.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn layer_norm(x: &mut [f32], gamma: &[f32], beta: &[f32], eps: f32) {
    let h = gamma.len();
    for row in x.chunks_exact_mut(h) {
        let n = h as f64;
        let mean = row.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
        let var = row
            .iter()
            .map(|&v| (f64::from(v) - mean).powi(2))
            .sum::<f64>()
            / n;
        let inv = 1.0 / (var + f64::from(eps)).sqrt();
        for ((v, g), b) in row.iter_mut().zip(gamma).zip(beta) {
            *v = ((f64::from(*v) - mean) * inv) as f32 * g + b;
        }
    }
}

/// Softmax of one row, in place, shifted by its maximum.
pub fn softmax(row: &mut [f32]) {
    let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mut sum = 0.0f32;
    for v in row.iter_mut() {
        *v = (*v - max).exp();
        sum += *v;
    }
    for v in row.iter_mut() {
        *v /= sum;
    }
}

/// Scales `v` to unit L2 norm; a norm below `1e-12` is taken as `1e-12` (`PyTorch`'s
/// `F.normalize`).
#[allow(clippy::cast_possible_truncation)]
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v
        .iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum::<f64>()
        .sqrt()
        .max(1e-12) as f32;
    for x in v.iter_mut() {
        *x /= norm;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (x, erf x, erfc x) from glibc's libm.
    const TABLE: &[(f64, f64, f64)] = &[
        (0.1, 0.112_462_916_018_284_9, 0.887_537_083_981_715_2),
        (0.5, 0.520_499_877_813_046_5, 0.479_500_122_186_953_5),
        (1.0, 0.842_700_792_949_714_9, 0.157_299_207_050_285_13),
        (1.5, 0.966_105_146_475_310_8, 0.033_894_853_524_689_274),
        (2.0, 0.995_322_265_018_952_7, 0.004_677_734_981_047_265),
        (2.5, 0.999_593_047_982_555, 0.000_406_952_017_444_958_9),
        (3.0, 0.999_977_909_503_001_4, 2.209_049_699_858_543_8e-5),
        (3.5, 0.999_999_256_901_627_6, 7.430_983_723_414_128e-7),
        (4.5, 0.999_999_999_803_383_9, 1.966_160_441_542_887_3e-10),
        (6.0, 1.0, 2.151_973_671_249_891_6e-17),
    ];

    #[test]
    fn erf_matches_libm() {
        for &(x, e, c) in TABLE {
            assert!((erf(x) - e).abs() < 1e-15, "erf({x}) = {}", erf(x));
            assert!((erf(-x) + e).abs() < 1e-15, "erf(-{x})");
            if x >= ERF_SPLIT {
                let rel = (erfc_fraction(x) - c).abs() / c;
                assert!(rel < 1e-13, "erfc({x}) = {} rel {rel}", erfc_fraction(x));
            }
        }
        assert!(erf(0.0).abs() < f64::MIN_POSITIVE);
        assert!((erf(30.0) - 1.0).abs() < f64::EPSILON);
        assert!((erf(f64::INFINITY) - 1.0).abs() < f64::EPSILON);
        assert!(erf(f64::NAN).is_nan());
    }

    #[test]
    fn erf_is_continuous_at_the_split() {
        let below = erf(ERF_SPLIT - 1e-12);
        let above = erf(ERF_SPLIT);
        assert!((below - above).abs() < 1e-15, "{below} vs {above}");
        // Monotone and smooth over a fine grid across both branches.
        let mut prev = erf(0.0);
        for i in 1..=6000 {
            let x = f64::from(i) * 1e-3;
            let v = erf(x);
            assert!(v >= prev, "erf not monotone at {x}");
            prev = v;
        }
    }

    #[test]
    fn gelu_exact_form() {
        assert!(gelu(0.0).abs() < f32::MIN_POSITIVE);
        assert!((gelu(1.0) - 0.841_344_7).abs() < 1e-7);
        assert!((gelu(-1.0) + 0.158_655_25).abs() < 1e-7);
        assert!((gelu(3.0) - 2.995_950_5).abs() < 1e-6);
        assert!((gelu(10.0) - 10.0).abs() < 1e-6);
        assert!(gelu(-10.0).abs() < 1e-6);
    }

    /// The exact GELU in `f64`: from `|x / sqrt 2| >= 6` on, `x` or 0 (the difference is
    /// below `1e-15` there).
    fn gelu_exact(x: f32) -> f64 {
        let x = f64::from(x);
        let z = x * std::f64::consts::FRAC_1_SQRT_2;
        if z >= 6.0 {
            x
        } else if z <= -6.0 {
            0.0
        } else {
            0.5 * x * (1.0 + erf(z))
        }
    }

    #[test]
    fn gelu_f32_is_close_to_the_exact_form() {
        let mut worst = 0.0f64;
        for i in -200_000..=200_000 {
            let x = f64::from(i) * 5e-5;
            #[allow(clippy::cast_possible_truncation)]
            let x = x as f32;
            worst = worst.max((f64::from(gelu_f32(x)) - gelu_exact(x)).abs());
        }
        assert!(worst <= 1e-6, "max abs error {worst}");
        assert_eq!(gelu_f32(0.0).to_bits(), 0);
        assert_eq!(gelu_f32(-0.0).to_bits(), (-0.0f32).to_bits());
        assert_eq!(gelu_f32(100.0), 100.0);
        assert_eq!(gelu_f32(f32::INFINITY), f32::INFINITY);
        assert_eq!(gelu_f32(f32::MAX), f32::MAX);
        assert_eq!(gelu_f32(-100.0), 0.0);
        assert_eq!(gelu_f32(f32::MIN), 0.0);
        assert!(gelu_f32(f32::NAN).is_nan());
        assert!(gelu_f32(f32::NEG_INFINITY).is_nan());
    }

    /// Every finite `f32` against the exact GELU: the error bound stated on [`gelu_f32`]. About
    /// 2 * 10^9 exact evaluations; run at opt-level 3 (minutes, on every core).
    #[test]
    #[ignore = "long: every f32; run at opt-level 3"]
    fn gelu_f32_error_over_every_f32() {
        let threads = std::thread::available_parallelism().map_or(1, usize::from);
        let chunk = (1u64 << 32).div_ceil(threads as u64);
        let worst = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads as u64)
                .map(|i| {
                    s.spawn(move || {
                        let mut worst = (0.0f64, 0.0f32);
                        for bits in (i * chunk)..((i + 1) * chunk).min(1 << 32) {
                            #[allow(clippy::cast_possible_truncation)]
                            let x = f32::from_bits(bits as u32);
                            if !x.is_finite() {
                                continue;
                            }
                            let d = (f64::from(gelu_f32(x)) - gelu_exact(x)).abs();
                            if d > worst.0 {
                                worst = (d, x);
                            }
                        }
                        worst
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("a sweep thread"))
                .fold((0.0f64, 0.0f32), |a, b| if b.0 > a.0 { b } else { a })
        });
        eprintln!("gelu_f32: max abs error {:e} at x = {:e}", worst.0, worst.1);
        assert!(worst.0 <= 1e-6, "max abs error {} at {}", worst.0, worst.1);
    }

    #[test]
    fn layer_norm_rows() {
        let mut x = vec![1.0, 2.0, 3.0, 4.0, 10.0, 10.0, 10.0, 10.0];
        layer_norm(&mut x, &[1.0, 1.0, 2.0, 1.0], &[0.0, 0.0, 0.0, 0.5], 1e-12);
        let s = 1.0 / 1.25f32.sqrt();
        let want = [-1.5 * s, -0.5 * s, 2.0 * 0.5 * s, 1.5 * s + 0.5];
        for (a, b) in x[..4].iter().zip(want) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
        // A constant row is all beta.
        assert_eq!(&x[4..], &[0.0, 0.0, 0.0, 0.5]);
    }

    #[test]
    fn softmax_row() {
        let mut r = [1.0, 2.0, 3.0, 1000.0];
        softmax(&mut r);
        assert!((r.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        assert!((r[3] - 1.0).abs() < 1e-6);
        let mut r = [0.0, 0.0];
        softmax(&mut r);
        assert_eq!(r, [0.5, 0.5]);
    }

    #[test]
    fn l2_normalize_unit_and_zero() {
        let mut v = [3.0, 4.0];
        l2_normalize(&mut v);
        assert_eq!(v, [0.6, 0.8]);
        let mut z = [0.0, 0.0];
        l2_normalize(&mut z);
        assert_eq!(z, [0.0, 0.0]);
    }
}
