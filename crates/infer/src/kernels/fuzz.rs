//! The bit-equality fuzz tests of ADR-0086 section 5: every instruction-set kernel this CPU can
//! run against the portable reference, on random shapes (widths that are not a multiple of the
//! block or of the vector width, blocks shorter and longer than a vector, output counts that
//! are not a multiple of the tile) and on random and extreme values (`-128`, `127`). Integer
//! block sums must be equal, and the whole linear layer's `f32` outputs bit for bit.
//!
//! `fuzz_long` (ignored) runs many more cases; run it at opt-level 3:
//! `CARGO_PROFILE_TEST_OPT_LEVEL=3 cargo test -p ulei-infer --lib fuzz_long -- --ignored`.

use super::{Kernel, QuantRows, TILE, Tile, linear_i8_on, portable};

/// xorshift64*: a small deterministic generator, so the cases are the same on every run.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n` (`n > 0`; the modulo bias does not matter here).
    #[allow(clippy::cast_possible_truncation)]
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// An `i8`: uniform, or (one time in four) only the extremes `-128`, `-127`, `127`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn int8(&mut self, extreme: bool) -> i8 {
        if extreme {
            [-128, -127, 127][self.below(3)]
        } else {
            (self.next() >> 56) as u8 as i8
        }
    }

    fn int8s(&mut self, n: usize) -> Vec<i8> {
        let extreme = self.below(4) == 0;
        (0..n).map(|_| self.int8(extreme)).collect()
    }

    /// A finite `f32` of varied magnitude and sign.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::cast_possible_truncation
    )]
    fn scale(&mut self) -> f32 {
        let m = (self.next() >> 40) as f32 / (1u64 << 24) as f32;
        let e = (self.below(40) as i32) - 20;
        let s = if self.below(8) == 0 { -1.0 } else { 1.0 };
        s * m * 2f32.powi(e)
    }
}

/// The instruction-set kernels to check on this CPU, with their names. `Tile::Portable` is
/// left out: it is the reference.
fn arch_tiles() -> Vec<(&'static str, Tile)> {
    let mut tiles = Vec::new();
    for kernel in [Kernel::Avx2, Kernel::Neon] {
        let tile = Tile::of(kernel);
        if matches!(tile, Tile::Portable) {
            eprintln!(
                "kernel {} not available on this CPU: not checked",
                kernel.name()
            );
        } else {
            tiles.push((kernel.name(), tile));
        }
    }
    #[cfg(target_arch = "aarch64")]
    if super::neon::Neon::detect().has_dotprod() {
        // Also the widening path, which a `dotprod` CPU would not take by itself.
        tiles.push(("neon-widening", Tile::Neon(super::neon::Neon::widening())));
    }
    tiles
}

/// Every block sum of `x W^T` (`out` weight rows) as `tile` emits them, dense: `[row][output]
/// [block]`. `None` if `tile` does not take this shape (the AVX2 kernel takes even blocks only).
// `None` only on `x86_64` (the AVX2 kernel takes even blocks only).
#[allow(clippy::unnecessary_wraps)]
fn sums_by(tile: Tile, x: &QuantRows, w: &[i8], out: usize) -> Option<Vec<i32>> {
    let (rows, blocks) = (x.rows(), x.blocks());
    let mut dense = vec![i32::MIN; rows * out * blocks];
    let mut emit = |o0: usize, r: usize, sums: &[i32]| {
        assert_eq!(sums.len(), blocks * TILE, "emitted sums");
        for (b, lanes) in sums.as_chunks::<TILE>().0.iter().enumerate() {
            for (j, &v) in lanes.iter().enumerate().take(out - o0) {
                dense[(r * out + o0 + j) * blocks + b] = v;
            }
        }
    };
    match tile {
        Tile::Portable => super::per_row(x, w, out, &mut emit, |xr, wt, s| {
            portable::block_sums(xr, wt, x.block, s);
        }),
        #[cfg(target_arch = "x86_64")]
        Tile::Avx2(t) => {
            if !x.block.is_multiple_of(2) {
                return None;
            }
            t.linear(x, w, out, &mut emit);
        }
        #[cfg(target_arch = "aarch64")]
        Tile::Neon(t) => super::per_row(x, w, out, &mut emit, |xr, wt, s| {
            if wt.len() == TILE * xr.len() {
                t.block_sums(xr, wt, x.block, s);
            } else {
                portable::block_sums(xr, wt, x.block, s);
            }
        }),
    }
    Some(dense)
}

/// A random shape `(k, block)`: widths around and between vector and block multiples.
fn shape(rng: &mut Rng, max_k: usize) -> (usize, usize) {
    let k = match rng.below(3) {
        0 => 1 + rng.below(max_k),
        1 => 16 * (1 + rng.below(max_k / 16)) + [0, 1, 15, 17][rng.below(4)],
        _ => [384, 1536, 1024, 4096, 31, 33, 47, 48, 65][rng.below(9)].min(max_k + 65),
    };
    let block = match rng.below(3) {
        0 => [32, 32, 64, 16, 48, 96, 2][rng.below(7)],
        1 => 1 + rng.below(80),
        _ => k + rng.below(40),
    };
    (k, block)
}

/// Random activations of `rows x k` in blocks of `block`.
fn quant_rows(rng: &mut Rng, rows: usize, k: usize, block: usize) -> QuantRows {
    QuantRows {
        values: rng.int8s(rows * k),
        scales: (0..rows * k.div_ceil(block)).map(|_| rng.scale()).collect(),
        width: k,
        block,
    }
}

/// Checks `cases` random shapes: every arch kernel's block sums equal the portable ones.
fn block_sums_cases(seed: u64, cases: usize, max_k: usize) {
    let tiles = arch_tiles();
    let mut rng = Rng::new(seed);
    let mut checked = 0usize;
    for case in 0..cases {
        let (k, block) = shape(&mut rng, max_k);
        let rows = 1 + rng.below(9);
        let out = 1 + rng.below(3 * TILE + 3);
        let x = quant_rows(&mut rng, rows, k, block);
        let w = rng.int8s(out * k);
        let want = sums_by(Tile::Portable, &x, &w, out).unwrap_or_default();
        assert!(!want.is_empty() && !want.contains(&i32::MIN));
        for (name, tile) in &tiles {
            if let Some(got) = sums_by(*tile, &x, &w, out) {
                checked += 1;
                assert_eq!(
                    got, want,
                    "{name}: case {case}, rows {rows}, out {out}, k {k}, block {block}"
                );
            }
        }
    }
    eprintln!("block sums: {checked} kernel cases checked");
}

/// Checks `cases` random linear layers: every kernel's `f32` output bit-equal to
/// [`portable::linear_i8`].
fn linear_cases(seed: u64, cases: usize, max_k: usize) {
    let mut kernels: Vec<Kernel> = Kernel::all_available();
    kernels.retain(|k| *k != Kernel::Portable);
    let mut rng = Rng::new(seed);
    for case in 0..cases {
        let (k, block) = shape(&mut rng, max_k);
        let rows = 1 + rng.below(9);
        let out = 1 + rng.below(3 * TILE + 3);
        let x = quant_rows(&mut rng, rows, k, block);
        let w = rng.int8s(out * k);
        let ws: Vec<f32> = (0..out).map(|_| rng.scale()).collect();
        let b: Vec<f32> = (0..out).map(|_| rng.scale()).collect();
        let want: Vec<u32> = portable::linear_i8(&x, &w, &ws, &b)
            .iter()
            .map(|v| v.to_bits())
            .collect();
        // The portable tiling of the dispatcher too: it must not change the reference's order.
        for kernel in std::iter::once(Kernel::Portable).chain(kernels.iter().copied()) {
            let got: Vec<u32> = linear_i8_on(kernel, &x, &w, &ws, &b)
                .iter()
                .map(|v| v.to_bits())
                .collect();
            assert_eq!(
                got,
                want,
                "{}: case {case}, rows {rows}, out {out}, k {k}, block {block}",
                kernel.name()
            );
        }
    }
}

#[test]
fn block_sums_equal_portable() {
    block_sums_cases(1, 3_000, 700);
}

#[test]
fn linear_i8_bit_equal_to_portable() {
    linear_cases(2, 1_000, 400);
}

#[test]
fn extremes_at_the_widest_inputs() {
    // Every product at its largest magnitude, over widths and blocks up to bge-m3's FFN out.
    let tiles = arch_tiles();
    let (rows, out) = (5, TILE + 3);
    for (k, block) in [
        (4096, 32),
        (4096, 4096),
        (1536, 64),
        (4095, 1000),
        (16_384, 16_384),
    ] {
        for (xv, wv) in [(-128i8, -128i8), (-128, 127), (127, 127), (-127, -127)] {
            let mut x = QuantRows {
                values: vec![xv; rows * k],
                scales: vec![1.0; rows * k.div_ceil(block)],
                width: k,
                block,
            };
            // One row of mixed signs, so the rows differ.
            for (i, v) in x.values[k..2 * k].iter_mut().enumerate() {
                *v = if i % 2 == 0 { -128 } else { 127 };
            }
            let mut w = vec![wv; out * k];
            for (i, v) in w[3 * k..4 * k].iter_mut().enumerate() {
                *v = if i % 3 == 0 { -128 } else { 127 };
            }
            let want = sums_by(Tile::Portable, &x, &w, out);
            for (name, tile) in &tiles {
                let got = sums_by(*tile, &x, &w, out);
                assert_eq!(got, want, "{name}: k {k} block {block} x {xv} w {wv}");
            }
            x.values.fill(0);
        }
    }
}

#[test]
#[allow(clippy::many_single_char_names)]
fn portable_block_sums_are_the_dot_products() {
    let mut rng = Rng::new(3);
    let (k, block, n) = (70, 32, 3);
    let x = rng.int8s(k);
    let w = rng.int8s(n * k);
    let mut sums = vec![0; 3 * n];
    portable::block_sums(&x, &w, block, &mut sums);
    for o in 0..n {
        for b in 0..3 {
            let r = b * block..((b + 1) * block).min(k);
            let want: i32 = x[r.clone()]
                .iter()
                .zip(&w[o * k..][r])
                .map(|(&a, &c)| i32::from(a) * i32::from(c))
                .sum();
            assert_eq!(sums[b * n + o], want);
        }
    }
}

/// `gelu` on `x` by `tile`, as bits.
fn gelu_by(tile: Tile, x: &[f32]) -> Vec<u32> {
    let mut y = x.to_vec();
    match tile {
        #[cfg(target_arch = "x86_64")]
        Tile::Avx2(t) => super::avx2::gelu(t, &mut y),
        _ => portable::gelu(&mut y),
    }
    y.iter().map(|v| v.to_bits()).collect()
}

/// `gelu` of every arch kernel bit-equal to the portable one (and to the scalar formula) on
/// every `step`-th `f32` bit pattern from `start` (a million at a time), in slices of every
/// length up to 40 (the vector loop's tails), and on the edges of the formula's branches.
fn gelu_cases(start: u32, step: u32) {
    let tiles = arch_tiles();
    let mut edges = Vec::new();
    for edge in [
        0.0f32,
        2.0,
        4.0,
        1e-30,
        1e-45,
        f32::MAX,
        f32::INFINITY,
        f32::NAN,
    ] {
        let z = edge / std::f32::consts::FRAC_1_SQRT_2;
        for v in [edge, z, z.next_up(), z.next_down()] {
            edges.extend([v, -v]);
        }
    }
    let count = u64::from(u32::MAX / step) + 1;
    let mut i = 0u64;
    let mut x = edges;
    loop {
        let want: Vec<u32> = x
            .iter()
            .map(|&v| crate::ops::gelu_f32(v).to_bits())
            .collect();
        assert_eq!(gelu_by(Tile::Portable, &x), want, "portable slice");
        for (name, tile) in &tiles {
            let (mut at, mut len) = (0, 0);
            while at < x.len() {
                let end = (at + len).min(x.len());
                assert_eq!(
                    gelu_by(*tile, &x[at..end]),
                    want[at..end],
                    "{name}: gelu of {:?}..",
                    x.get(at)
                );
                at = end;
                len = (len + 1) % 41;
            }
        }
        if i >= count {
            break;
        }
        let n = (count - i).min(1 << 20);
        #[allow(clippy::cast_possible_truncation)]
        let next =
            (i..i + n).map(|j| f32::from_bits(start.wrapping_add((j as u32).wrapping_mul(step))));
        x = next.collect();
        i += n;
    }
}

#[test]
fn gelu_bit_equal_to_portable() {
    gelu_cases(12_345, 4_093);
    // Dense around the activations a model sees.
    let tiles = arch_tiles();
    let mut rng = Rng::new(4);
    #[allow(clippy::cast_precision_loss)]
    let x: Vec<f32> = (0..200_000)
        .map(|_| ((rng.next() >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 24.0)
        .collect();
    let want = gelu_by(Tile::Portable, &x);
    for (name, tile) in &tiles {
        assert_eq!(gelu_by(*tile, &x), want, "{name}: gelu");
    }
}

#[test]
#[ignore = "long: run at opt-level 3"]
fn fuzz_long() {
    block_sums_cases(11, 2_000_000, 2_000);
    linear_cases(12, 200_000, 1_600);
    // Every f32 bit pattern.
    gelu_cases(0, 1);
}
