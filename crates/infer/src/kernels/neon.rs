//! The NEON int8 kernel (ADR-0087 section 5, `aarch64`): the block sums of one token row
//! against eight weight rows, 16 input channels at a time.
//!
//! - With the `dotprod` feature (Apple M1 and later, Neoverse N1, Graviton 2 and later):
//!   `SDOT`, which adds four `i8 x i8` products into each `i32` lane. It is written as inline
//!   assembly because its intrinsic (`vdotq_s32`) is not stable at the crate's MSRV (1.89).
//! - Without it: `SMULL` / `SMULL2` (`i8 x i8` into `i16`, exact: at most `128 * 128 = 2^14`)
//!   and `SADALP` (pairwise add into `i32` lanes).
//!
//! Each row's lanes are added across (`ADDV`) per block; channels left over after the last run
//! of 16 in a block are summed in scalar code. All of it is exact `i32` arithmetic, so the sums
//! equal [`super::portable::block_sums`].
//!
//! **Run on hardware once**: the fuzz tests of `super::fuzz` and the golden tests passed on an
//! Apple M3 Pro (ADR-0087 E11m, both paths, the widening one forced), and
//! [`super::Kernel::best`] picks it on `aarch64` (open question 17). Linux `aarch64` and a CPU
//! without `dotprod` have not run it natively.
//!
//! The only `unsafe` here: calling the `#[target_feature]` functions (NEON is part of the
//! `aarch64` baseline; `dotprod` is checked by [`Neon::detect`]), loads from slices of the right
//! length, and the `SDOT` instruction.

use std::arch::aarch64::{
    int8x16_t, int32x4_t, vaddvq_s32, vdupq_n_s32, vget_low_s8, vld1q_s8, vmull_high_s8, vmull_s8,
    vpadalq_s16,
};
use std::arch::asm;

use super::TILE;

/// The NEON kernel, with `SDOT` if the CPU has `dotprod`: only [`Neon::detect`] makes one.
#[derive(Debug, Clone, Copy)]
pub(super) struct Neon {
    dotprod: bool,
}

impl Neon {
    /// The kernel for this CPU (NEON is part of the `aarch64` baseline).
    pub(super) fn detect() -> Self {
        Self {
            dotprod: std::arch::is_aarch64_feature_detected!("dotprod"),
        }
    }

    /// This kernel without `SDOT`, for testing the widening path on a `dotprod` CPU.
    #[cfg(test)]
    pub(super) fn widening() -> Self {
        Self { dotprod: false }
    }

    /// Whether it uses `SDOT`.
    #[cfg(test)]
    pub(super) fn has_dotprod(self) -> bool {
        self.dotprod
    }

    /// [`super::portable::block_sums`] for exactly [`TILE`] weight rows.
    ///
    /// # Panics
    /// If `w` is not [`TILE`] rows of `x.len()` or `sums` is not `blocks * TILE` values.
    pub(super) fn block_sums(self, x: &[i8], w: &[i8], block: usize, sums: &mut [i32]) {
        assert_eq!(w.len(), TILE * x.len(), "neon: weight is not {TILE} rows");
        assert_eq!(sums.len(), x.len().div_ceil(block) * TILE, "neon: sums");
        if self.dotprod {
            // SAFETY: NEON is part of the `aarch64` baseline, and `dotprod` is true only if
            // `detect` found that feature on this CPU, which `block_sums_with::<true>` needs
            // for `SDOT`. The slice arguments are checked above.
            unsafe { block_sums_with::<true>(x, w, block, sums) }
        } else {
            // SAFETY: NEON is part of the `aarch64` baseline; the slice arguments are checked
            // above.
            unsafe { block_sums_with::<false>(x, w, block, sums) }
        }
    }
}

/// Loads 16 bytes.
#[inline]
#[target_feature(enable = "neon")]
fn load16(s: &[i8; 16]) -> int8x16_t {
    // SAFETY: `s` is 16 readable bytes; `ld1` has no alignment requirement.
    unsafe { vld1q_s8(s.as_ptr()) }
}

/// `acc` plus, in each lane, the sum of four products of `x` and `w`.
///
/// # Safety
/// The CPU has the `dotprod` feature.
#[inline]
unsafe fn sdot(mut acc: int32x4_t, x: int8x16_t, w: int8x16_t) -> int32x4_t {
    // SAFETY: `SDOT` only reads its three vector registers and writes the first; it touches no
    // memory, stack or flags. The CPU has `dotprod` by this function's contract; the directive
    // only lets the assembler accept the instruction.
    unsafe {
        asm!(
            ".arch_extension dotprod",
            "sdot {acc:v}.4s, {x:v}.16b, {w:v}.16b",
            acc = inout(vreg) acc,
            x = in(vreg) x,
            w = in(vreg) w,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    acc
}

/// `acc` plus, in each lane, the sum of four products of `x` and `w` (lanes paired differently
/// from [`sdot`]; only the total over the lanes is used).
#[inline]
#[target_feature(enable = "neon")]
fn widening(acc: int32x4_t, x: int8x16_t, w: int8x16_t) -> int32x4_t {
    let acc = vpadalq_s16(acc, vmull_s8(vget_low_s8(x), vget_low_s8(w)));
    vpadalq_s16(acc, vmull_high_s8(x, w))
}

/// The block sums, with [`sdot`] if `DOT`, [`widening`] otherwise.
///
/// # Safety
/// With `DOT`, the CPU has the `dotprod` feature.
#[target_feature(enable = "neon")]
unsafe fn block_sums_with<const DOT: bool>(x: &[i8], w: &[i8], block: usize, sums: &mut [i32]) {
    let k = x.len();
    let rows: [&[i8]; TILE] = std::array::from_fn(|j| &w[j * k..(j + 1) * k]);
    for (b, out) in sums.as_chunks_mut::<TILE>().0.iter_mut().enumerate() {
        let start = b * block;
        let end = (start + block).min(k);
        let (xc, xt) = x[start..end].as_chunks::<16>();
        let tail = end - xt.len();
        for (o, row) in out.iter_mut().zip(&rows) {
            let (wc, _) = row[start..end].as_chunks::<16>();
            let mut acc = vdupq_n_s32(0);
            for (xv, wv) in xc.iter().zip(wc) {
                let (xv, wv) = (load16(xv), load16(wv));
                acc = if DOT {
                    // SAFETY: with `DOT` the caller guarantees `dotprod`.
                    unsafe { sdot(acc, xv, wv) }
                } else {
                    widening(acc, xv, wv)
                };
            }
            *o = vaddvq_s32(acc);
            for (&xv, &wv) in xt.iter().zip(&row[tail..end]) {
                *o += i32::from(xv) * i32::from(wv);
            }
        }
    }
}
