//! The bit-equality fuzz tests of ADR-0087 section 5: every instruction-set kernel this CPU can
//! run against the portable reference, on random shapes (widths that are not a multiple of the
//! block or of the vector width, blocks shorter and longer than a vector, output counts that
//! are not a multiple of the tile) and on random and extreme values. Integer block sums must be
//! equal, and the whole linear layer's `f32` outputs bit for bit.
//!
//! Activations are what [`portable::quantize`] produces, `[-127, 127]`, with `-127` and `127`
//! as the extremes; weights are any `i8`, `-128` included. A few cases also put `-128` into the
//! activations: the kernels that need `[-127, 127]` must then leave them to the portable path,
//! and the layer's output must still be bit-equal.
//!
//! `fuzz_long` (ignored) runs many more cases; run it at opt-level 3:
//! `CARGO_PROFILE_TEST_OPT_LEVEL=3 cargo test -p ulei-infer --lib fuzz_long -- --ignored`.

use std::cell::RefCell;

use super::{Kernel, QuantRows, Scratch, Softmax, TILE, Tile, linear_i8_on, portable};

std::thread_local! {
    /// One scratch per test thread, reused by every case and kernel: a kernel must give the
    /// same bits with the stale buffers of earlier calls (other shapes, other kernels) as with
    /// new ones.
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

/// [`linear_i8_on`] into an output of NaNs (every value must be written), on the shared
/// scratch.
fn linear_on(kernel: Kernel, x: &QuantRows, w: &[i8], ws: &[f32], b: &[f32]) -> Vec<f32> {
    let mut y = vec![f32::NAN; x.rows() * b.len()];
    SCRATCH.with_borrow_mut(|s| linear_i8_on(kernel, &x.view(), w, ws, b, &mut y, s));
    y
}

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

    /// Activations as quantisation makes them: in `[-127, 127]`, one time in four only `-127`
    /// and `127`. One time in 32, a few values are `-128` (outside what quantisation makes).
    fn act8s(&mut self, n: usize) -> Vec<i8> {
        let extreme = self.below(4) == 0;
        let mut v: Vec<i8> = (0..n)
            .map(|_| match self.int8(extreme) {
                i8::MIN => 127,
                x => x,
            })
            .collect();
        if n > 0 && self.below(32) == 0 {
            for _ in 0..=self.below(3) {
                let i = self.below(n);
                v[i] = i8::MIN;
            }
        }
        v
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
    SCRATCH.with_borrow_mut(|scratch| sums_on(tile, x, w, out, scratch))
}

#[allow(clippy::unnecessary_wraps)]
fn sums_on(
    tile: Tile,
    x: &QuantRows,
    w: &[i8],
    out: usize,
    scratch: &mut Scratch,
) -> Option<Vec<i32>> {
    let (rows, blocks) = (x.rows(), x.blocks());
    let x = &x.view();
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
        Tile::Portable => super::per_row(x, w, out, &mut emit, scratch, |xr, wt, s| {
            portable::block_sums(xr, wt, x.block, s);
        }),
        #[cfg(target_arch = "x86_64")]
        Tile::Avx2(t) => {
            if !t.takes(x) {
                return None;
            }
            t.block_sums(x, w, out, &mut emit, scratch);
        }
        #[cfg(target_arch = "aarch64")]
        Tile::Neon(t) => super::per_row(x, w, out, &mut emit, scratch, |xr, wt, s| {
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
        1 if rng.below(2) == 0 => 4 * (1 + rng.below(20)),
        1 => 1 + rng.below(80),
        // Longer than the row; mostly a multiple of 4, as the AVX2 kernel takes.
        _ => {
            let multiple = if rng.below(4) == 0 { 1 } else { 4 };
            (k + rng.below(40)).next_multiple_of(multiple)
        }
    };
    (k, block)
}

/// Random activations of `rows x k` in blocks of `block`.
fn quant_rows(rng: &mut Rng, rows: usize, k: usize, block: usize) -> QuantRows {
    QuantRows {
        values: rng.act8s(rows * k),
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
        let rows = 1 + rng.below(13);
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
        let rows = 1 + rng.below(13);
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
            let got: Vec<u32> = linear_on(kernel, &x, &w, &ws, &b)
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

/// Every row count from 1 to 13 on one shape: whole groups of [`super::ROWS`] rows that share a
/// weight tile, and each shorter last group (and inputs shorter than one group).
#[test]
fn every_row_count_through_the_groups() {
    let tiles = arch_tiles();
    let mut rng = Rng::new(7);
    let (k, block, out) = (100, 32, 2 * TILE + 3);
    for rows in 1..=13 {
        let x = quant_rows(&mut rng, rows, k, block);
        let mut x = x;
        x.values.iter_mut().for_each(|v| *v = (*v).max(-127));
        let w = rng.int8s(out * k);
        check_all_kernels(&tiles, &x, &w, out, &format!("rows {rows}"));
        let ws: Vec<f32> = (0..out).map(|_| rng.scale()).collect();
        let b: Vec<f32> = (0..out).map(|_| rng.scale()).collect();
        let want = bits(&portable::linear_i8(&x, &w, &ws, &b));
        for kernel in Kernel::all_available() {
            assert_eq!(
                bits(&linear_on(kernel, &x, &w, &ws, &b)),
                want,
                "{} rows {rows}",
                kernel.name()
            );
        }
    }
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

/// Activations of `rows x width` for [`quantize_cases`]: mostly of order 1, at times every
/// magnitude, with halves (`x / scale` exactly `n + 0.5`), zeros of both signs, infinities and
/// `NaN`s mixed in.
#[allow(clippy::cast_precision_loss)]
fn quantize_input(rng: &mut Rng, n: usize) -> Vec<f32> {
    let kind = rng.below(4);
    (0..n)
        .map(|_| match (kind, rng.below(64)) {
            (_, 0) => [
                0.0,
                -0.0,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NAN,
                f32::MAX,
                f32::MIN_POSITIVE,
                -1e-45,
            ][rng.below(8)],
            (0, _) => rng.scale(),
            // Integers and halves of a block whose maximum is 127: `x / scale` lands on them.
            (1, _) => (rng.below(255) as f32 - 127.0) + [0.0, 0.5, -0.5][rng.below(3)],
            _ => ((rng.next() >> 40) as f32 / (1u64 << 23) as f32 - 1.0) * 3.0,
        })
        .collect()
}

/// `quantize_rows_into` of `tile`: values and scales as bits.
fn quantize_by(tile: Tile, x: &[f32], width: usize, block: usize) -> (Vec<i8>, Vec<u32>) {
    let mut values = vec![0x55; x.len()];
    let mut scales = vec![f32::NAN; x.len() / width * width.div_ceil(block)];
    match tile {
        #[cfg(target_arch = "x86_64")]
        Tile::Avx2(t) => {
            super::avx2::quantize_rows_into(t, x, width, block, &mut values, &mut scales);
        }
        _ => portable::quantize_rows_into(x, width, block, &mut values, &mut scales),
    }
    (values, bits(&scales))
}

/// `cases` random activations: every kernel's quantised values and scales equal the portable
/// ones.
fn quantize_cases(seed: u64, cases: usize, max_width: usize) {
    let tiles = arch_tiles();
    let mut rng = Rng::new(seed);
    for case in 0..cases {
        let (width, block) = shape(&mut rng, max_width);
        let rows = 1 + rng.below(5);
        let x = quantize_input(&mut rng, rows * width);
        let want = quantize_by(Tile::Portable, &x, width, block);
        for (name, tile) in &tiles {
            assert_eq!(
                quantize_by(*tile, &x, width, block),
                want,
                "{name}: case {case}, width {width}, block {block}"
            );
        }
    }
}

#[test]
fn quantize_rows_bit_equal_to_portable() {
    quantize_cases(8, 2_000, 700);
}

#[test]
fn block_sums_equal_portable() {
    block_sums_cases(1, 3_000, 700);
}

#[test]
fn linear_i8_bit_equal_to_portable() {
    linear_cases(2, 1_000, 400);
}

/// Block sums and layer outputs of every arch kernel equal the portable ones for `x` and `w`
/// (`out` rows); the kernels must take `x` (activations in `[-127, 127]`, block a multiple of
/// 4).
fn check_all_kernels(tiles: &[(&str, Tile)], x: &QuantRows, w: &[i8], out: usize, what: &str) {
    let want = sums_by(Tile::Portable, x, w, out);
    for (name, tile) in tiles {
        let got = sums_by(*tile, x, w, out);
        assert!(got.is_some(), "{name} refused {what}");
        assert_eq!(got, want, "{name}: {what}");
    }
    let ws = vec![1.0f32; out];
    let b = vec![0.0f32; out];
    let want: Vec<u32> = portable::linear_i8(x, w, &ws, &b)
        .iter()
        .map(|v| v.to_bits())
        .collect();
    for kernel in Kernel::all_available() {
        let got: Vec<u32> = linear_on(kernel, x, w, &ws, &b)
            .iter()
            .map(|v| v.to_bits())
            .collect();
        assert_eq!(got, want, "{} layer: {what}", kernel.name());
    }
}

#[test]
fn extremes_at_the_widest_inputs() {
    // Every product at its largest magnitude (activations +-127, weights -128 and +-127), over
    // widths and blocks up to bge-m3's FFN out.
    let tiles = arch_tiles();
    let (rows, out) = (5, TILE + 3);
    for (k, block) in [
        (4096, 32),
        (4096, 4096),
        (1536, 64),
        (4095, 1000),
        (16_384, 16_384),
    ] {
        for (xv, wv) in [(-127i8, -128i8), (127, -128), (127, 127), (-127, -127)] {
            let mut x = QuantRows {
                values: vec![xv; rows * k],
                scales: vec![1.0; rows * k.div_ceil(block)],
                width: k,
                block,
            };
            // One row of mixed signs, so the rows differ.
            for (i, v) in x.values[k..2 * k].iter_mut().enumerate() {
                *v = if i % 2 == 0 { -127 } else { 127 };
            }
            let mut w = vec![wv; out * k];
            for (i, v) in w[3 * k..4 * k].iter_mut().enumerate() {
                *v = if i % 3 == 0 { -128 } else { 127 };
            }
            check_all_kernels(
                &tiles,
                &x,
                &w,
                out,
                &format!("k {k} block {block} x {xv} w {wv}"),
            );
        }
    }
}

#[test]
fn every_pair_of_extremes_at_every_lane() {
    // Each combination of extreme activation pair (+-127, 0) and weight pair (-128, +-127, 0)
    // in every byte position of every lane: all quads of all rows and outputs hold it, so any
    // saturating pair sum in any position changes a block sum. Odd rows and outputs hold the
    // swapped pair, so both positions of a pair see both values.
    let tiles = arch_tiles();
    let (rows, out, k, block) = (5, 2 * TILE + 3, 96, 32);
    let xs = [-127i8, 127, 0];
    let wsv = [-128i8, -127, 127, 0];
    for &xa in &xs {
        for &xb in &xs {
            for &wa in &wsv {
                for &wb in &wsv {
                    let pair = |r: usize, a: i8, b: i8, i: usize| {
                        if (i + r).is_multiple_of(2) { a } else { b }
                    };
                    let x = QuantRows {
                        values: (0..rows * k).map(|i| pair(i / k, xa, xb, i % k)).collect(),
                        scales: vec![1.0; rows * k.div_ceil(block)],
                        width: k,
                        block,
                    };
                    let w: Vec<i8> = (0..out * k).map(|i| pair(i / k, wa, wb, i % k)).collect();
                    check_all_kernels(
                        &tiles,
                        &x,
                        &w,
                        out,
                        &format!("x ({xa}, {xb}) w ({wa}, {wb})"),
                    );
                }
            }
        }
    }
}

#[test]
fn minus_128_activations_run_portable_and_stay_exact() {
    // Outside what quantisation makes: the kernels that need [-127, 127] leave such inputs to
    // the portable path, and the layer is still bit-equal.
    let (rows, out, k, block) = (6, TILE + 1, 64, 32);
    let mut x = QuantRows {
        values: vec![-128; rows * k],
        scales: vec![0.5; rows * k.div_ceil(block)],
        width: k,
        block,
    };
    x.values[3] = 127;
    let w = vec![-128i8; out * k];
    let ws = vec![1.0f32; out];
    let b = vec![0.0f32; out];
    let want = portable::linear_i8(&x, &w, &ws, &b);
    for kernel in Kernel::all_available() {
        let got = linear_on(kernel, &x, &w, &ws, &b);
        assert_eq!(
            got.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            want.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            "{}",
            kernel.name()
        );
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

/// The reference attention: per head and query, [`portable::scores`], softmax and
/// [`portable::weighted_sum`] into a zero context, as the model ran it before
/// [`super::attention`].
fn attention_reference(
    q: &[f32],
    k: &[f32],
    v: &[f32],
    tokens: usize,
    heads: usize,
    scale: f32,
    softmax: Softmax,
) -> Vec<f32> {
    let hidden = q.len() / tokens;
    let width = hidden / heads;
    let mut context = vec![0.0f32; tokens * hidden];
    let mut scores = vec![0.0f32; tokens];
    for head in 0..heads {
        let off = head * width;
        for i in 0..tokens {
            let at = i * hidden + off;
            portable::scores(&q[at..at + width], &k[off..], hidden, scale, &mut scores);
            softmax.apply(&mut scores);
            portable::weighted_sum(&scores, &v[off..], hidden, &mut context[at..at + width]);
        }
    }
    context
}

/// `attention` of `tile`, as bits.
#[allow(clippy::too_many_arguments)]
fn attention_by(
    tile: Tile,
    q: &[f32],
    k: &[f32],
    v: &[f32],
    tokens: usize,
    heads: usize,
    scale: f32,
    softmax: Softmax,
) -> Vec<u32> {
    // Not zero: the kernel writes every value.
    let mut context = vec![f32::NAN; q.len()];
    SCRATCH.with_borrow_mut(|scratch| match tile {
        #[cfg(target_arch = "x86_64")]
        Tile::Avx2(t) => super::avx2::attention(
            t,
            q,
            k,
            v,
            tokens,
            heads,
            scale,
            softmax,
            &mut context,
            scratch,
        ),
        _ => portable::attention(
            q,
            k,
            v,
            tokens,
            heads,
            scale,
            softmax,
            &mut context,
            &mut scratch.probs,
        ),
    });
    context.iter().map(|x| x.to_bits()).collect()
}

/// `cases` random attention shapes: every kernel bit-equal to the reference loop.
#[allow(clippy::many_single_char_names)]
fn attention_cases(seed: u64, cases: usize, max_tokens: usize) {
    let tiles = arch_tiles();
    let mut rng = Rng::new(seed);
    for case in 0..cases {
        let tokens = match rng.below(3) {
            0 => 1 + rng.below(9),
            1 => 8 * (1 + rng.below(max_tokens / 8)) + [0, 1, 7][rng.below(3)],
            _ => 1 + rng.below(max_tokens),
        };
        let heads = 1 + rng.below(4);
        let width = [32, 64, 32, 8, 16, 1, 5, 13, 36, 24, 40][rng.below(11)];
        let n = tokens * heads * width;
        // Mostly scores of order 1 (softmax neither flat nor one-hot, so a change of one ulp
        // anywhere shows), at times values of every magnitude.
        let wild = rng.below(8) == 0;
        let mag = [0.3f32, 1.0, 2.0][rng.below(3)];
        #[allow(clippy::cast_precision_loss)]
        let mut m = || -> Vec<f32> {
            (0..n)
                .map(|_| {
                    if wild {
                        rng.scale() * 0.5
                    } else {
                        ((rng.next() >> 40) as f32 / (1u64 << 23) as f32 - 1.0) * mag
                    }
                })
                .collect()
        };
        let (q, k, v) = (m(), m(), m());
        #[allow(clippy::cast_precision_loss)]
        let scale = 1.0 / (width as f32).sqrt();
        for softmax in [Softmax::Exact, Softmax::F32] {
            let want: Vec<u32> = attention_reference(&q, &k, &v, tokens, heads, scale, softmax)
                .iter()
                .map(|x| x.to_bits())
                .collect();
            let what =
                format!("case {case}: tokens {tokens}, heads {heads}, width {width}, {softmax:?}");
            assert_eq!(
                attention_by(Tile::Portable, &q, &k, &v, tokens, heads, scale, softmax),
                want,
                "portable: {what}"
            );
            for (name, tile) in &tiles {
                assert_eq!(
                    attention_by(*tile, &q, &k, &v, tokens, heads, scale, softmax),
                    want,
                    "{name}: {what}"
                );
            }
        }
    }
}

#[test]
fn attention_bit_equal_to_the_reference_loop() {
    attention_cases(5, 300, 70);
}

#[test]
#[ignore = "long: run at opt-level 3"]
fn fuzz_long() {
    block_sums_cases(11, 2_000_000, 2_000);
    linear_cases(12, 200_000, 1_600);
    attention_cases(13, 20_000, 300);
    quantize_cases(15, 200_000, 2_000);
    softmax_cases(14, 200_000, 600);
    // Every f32 bit pattern.
    gelu_cases(0, 1);
    exp_cases(0, 1);
}

/// `exp_f32` on `x` by `tile`, as bits.
fn exp_by(tile: Tile, x: &[f32]) -> Vec<u32> {
    let mut y = x.to_vec();
    match tile {
        #[cfg(target_arch = "x86_64")]
        Tile::Avx2(t) => super::avx2::exp(t, &mut y),
        _ => {
            for v in &mut y {
                *v = crate::ops::exp_f32(*v);
            }
        }
    }
    y.iter().map(|v| v.to_bits()).collect()
}

/// `exp_f32` of every arch kernel bit-equal to the scalar formula on every `step`-th `f32` bit
/// pattern from `start` (a million at a time), in slices of every length up to 40, and on the
/// edges of its clamps.
fn exp_cases(start: u32, step: u32) {
    let tiles = arch_tiles();
    let mut x = Vec::new();
    for edge in [
        0.0f32,
        -87.5,
        88.375,
        -87.336_55,
        88.722_84,
        -103.972_08,
        -0.346_573_6,
        0.346_573_6,
        f32::MIN_POSITIVE,
        f32::MAX,
        f32::INFINITY,
        f32::NAN,
    ] {
        for v in [edge, edge.next_up(), edge.next_down()] {
            x.extend([v, -v]);
        }
    }
    let count = u64::from(u32::MAX / step) + 1;
    let mut i = 0u64;
    loop {
        let want: Vec<u32> = x
            .iter()
            .map(|&v| crate::ops::exp_f32(v).to_bits())
            .collect();
        for (name, tile) in &tiles {
            let (mut at, mut len) = (0, 0);
            while at < x.len() {
                let end = (at + len).min(x.len());
                assert_eq!(
                    exp_by(*tile, &x[at..end]),
                    want[at..end],
                    "{name}: exp of {:?}..",
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
fn exp_bit_equal_to_the_scalar_formula() {
    exp_cases(54_321, 4_093);
    // Dense where softmax feeds it: x <= 0 after the maximum is subtracted.
    let tiles = arch_tiles();
    let mut rng = Rng::new(6);
    #[allow(clippy::cast_precision_loss)]
    let x: Vec<f32> = (0..200_000)
        .map(|_| -((rng.next() >> 40) as f32 / (1u64 << 24) as f32) * 100.0)
        .collect();
    let want = exp_by(Tile::Portable, &x);
    for (name, tile) in &tiles {
        assert_eq!(exp_by(*tile, &x), want, "{name}: exp");
    }
}

/// `softmax_f32` of rows of `len` values by `tile`, as bits.
fn softmax_by(tile: Tile, x: &[f32], len: usize) -> Vec<u32> {
    let mut y = x.to_vec();
    match tile {
        #[cfg(target_arch = "x86_64")]
        Tile::Avx2(t) => super::avx2::softmax_rows(t, &mut y, len),
        _ => {
            for row in y.chunks_mut(len) {
                crate::ops::softmax_f32(row);
            }
        }
    }
    y.iter().map(|v| v.to_bits()).collect()
}

/// `cases` random rows of 1 to `max_len` scores, at times with extreme, infinite or `NaN`
/// values: every kernel's `softmax_f32` bit-equal to the portable one.
fn softmax_cases(seed: u64, cases: usize, max_len: usize) {
    let tiles = arch_tiles();
    let mut rng = Rng::new(seed);
    for case in 0..cases {
        let len = match rng.below(3) {
            0 => 1 + rng.below(17),
            1 => 8 * (1 + rng.below(max_len / 8)) + [0, 1, 7][rng.below(3)],
            _ => 1 + rng.below(max_len),
        };
        let spread = [1.0f32, 10.0, 100.0, 1e4][rng.below(4)];
        #[allow(clippy::cast_precision_loss)]
        let mut x: Vec<f32> = (0..len)
            .map(|_| ((rng.next() >> 40) as f32 / (1u64 << 23) as f32 - 1.0) * spread)
            .collect();
        if rng.below(8) == 0 {
            let at = rng.below(len);
            x[at] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX, -0.0][rng.below(5)];
        }
        let want = softmax_by(Tile::Portable, &x, len);
        for (name, tile) in &tiles {
            assert_eq!(
                softmax_by(*tile, &x, len),
                want,
                "{name}: case {case}, {len} values"
            );
        }
    }
}

#[test]
fn softmax_f32_bit_equal_to_portable() {
    softmax_cases(7, 3_000, 600);
}

/// The default kernel by instruction set (ADR-0087 open question 17): AVX2 where it runs, NEON
/// on `aarch64`, portable elsewhere; always one this CPU can run.
#[test]
fn best_is_avx2_on_x86_64_and_neon_on_aarch64() {
    let best = Kernel::best();
    assert!(best.available(), "{}", best.name());
    let want = if Kernel::Avx2.available() {
        Kernel::Avx2
    } else if cfg!(target_arch = "aarch64") {
        Kernel::Neon
    } else {
        Kernel::Portable
    };
    assert_eq!(best, want);
}
