//! The dispatch layer's own contract, beside `fuzz` (which compares the kernels): the public
//! wrappers equal the forms they wrap, `Scratch::bytes` counts every buffer, and the kernels a
//! CPU reports are the ones it has. Mutants that survived the first E12a run (ADR-0087 section
//! 10) showed none of this was tested.

#![allow(
    clippy::many_single_char_names,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::float_cmp
)]

use super::*;

#[test]
fn the_vec_wrappers_equal_the_into_forms_and_the_portable_kernels() {
    let x: Vec<f32> = (0..12).map(|i| (i as f32 - 5.5) / 3.0).collect();
    let w: Vec<f32> = (0..24).map(|i| ((i * 7 % 11) as f32 - 5.0) / 4.0).collect();
    let b = [0.25f32, -0.5, 1.0, 0.0, 2.0, -1.5];
    let a = linear(&x, &w, &b, 4);
    let mut into = vec![9.0f32; 3 * 6];
    linear_into(&x, &w, &b, 4, &mut into);
    assert_eq!(a, into);
    assert_eq!(a, portable::linear(&x, &w, &b, 4));
    assert!(a.iter().any(|v| *v != 0.0));
    assert_eq!(dot(&x[..4], &w[..4]), portable::dot(&x[..4], &w[..4]));
    assert_ne!(dot(&x[..4], &w[..4]), 0.0);

    let q = quantize_rows(&x, 4, 2);
    let wq: Vec<i8> = (0..24).map(|i| (i * 37 % 255) as i8).collect();
    let scale = [0.5f32, 0.25, 1.0, 2.0, 0.125, 1.5];
    let y = linear_i8(&q, &wq, &scale, &b);
    let mut y_into = vec![7.0f32; y.len()];
    linear_i8_into(
        &q.view(),
        &wq,
        &scale,
        &b,
        &mut y_into,
        &mut Scratch::default(),
    );
    assert_eq!(y, y_into);
    assert_eq!(y, portable::linear_i8(&q, &wq, &scale, &b));
    assert!(y.iter().any(|v| *v != b[0]));
}

#[test]
fn scratch_bytes_counts_every_buffer_by_capacity() {
    let mut s = Scratch::default();
    assert_eq!(s.bytes(), 0);
    s.quads = Vec::with_capacity(3);
    s.packed = Vec::with_capacity(2);
    s.sums = Vec::with_capacity(5);
    s.lanes = Vec::with_capacity(7);
    s.keys_t = Vec::with_capacity(11);
    s.vals = Vec::with_capacity(13);
    s.probs = Vec::with_capacity(17);
    // Every buffer holds 4-byte values; a packed entry is a tile of them.
    let want = (s.quads.capacity()
        + s.sums.capacity()
        + s.lanes.capacity()
        + s.keys_t.capacity()
        + s.vals.capacity()
        + s.probs.capacity())
        * 4
        + s.packed.capacity() * 4 * TILE;
    assert_eq!(s.bytes(), want);
}

#[test]
fn a_cpu_reports_only_the_kernels_of_its_instruction_set() {
    assert!(Kernel::Portable.available());
    let all = Kernel::all_available();
    assert_eq!(all.first(), Some(&Kernel::Portable), "portable first");
    #[cfg(target_arch = "x86_64")]
    {
        assert!(!Kernel::Neon.available(), "no NEON on x86_64");
        assert_eq!(
            Kernel::Avx2.available(),
            std::arch::is_x86_feature_detected!("avx2")
        );
        assert!(!all.contains(&Kernel::Neon));
    }
    #[cfg(target_arch = "aarch64")]
    {
        assert!(!Kernel::Avx2.available(), "no AVX2 on aarch64");
        assert!(Kernel::Neon.available());
    }
    let names: Vec<&str> = Kernel::ALL.iter().map(|k| k.name()).collect();
    assert_eq!(names, ["portable", "avx2", "neon"]);
    assert!(select(Kernel::Portable));
    assert_eq!(active(), Kernel::Portable);
    assert!(select(Kernel::best()));
    assert_eq!(active(), Kernel::best());
    #[cfg(target_arch = "x86_64")]
    assert!(!select(Kernel::Neon), "a kernel the CPU lacks is refused");
}
