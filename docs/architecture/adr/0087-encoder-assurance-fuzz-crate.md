# ADR-0087: The fuzz crate of the encoder engine and the garden's helper pipe

Status: proposed
Date: 2026-11-26
Affects: dep:libfuzzer-sys, dep:serde_json, dep:sha2, dep:ulei-infer, dep:ulei-garden

## Context

ADR-0086 section 10 (E12a) puts the encoder engine's assurance into CI. Its coverage-guided
fuzzing needs a crate that links `libfuzzer-sys` and the two crates under test: `fuzz/` at the
repository root, built only by nightly `cargo fuzz` in the `assurance` workflow.

## Decision

- `fuzz/` is a package with its own `[workspace]`: not a member of the repository's workspace,
  not under `crates/`, no module and no library class, no budget row, never linked by anything.
  `cargo xtask arch`, the module map, `cargo deny` and the MSRV check do not see it.
- Its dependencies are dev tooling: `libfuzzer-sys` (MIT OR Apache-2.0, pinned `=0.4.13`), `sha2`
  and `serde_json` (already in the workspace), `ulei-infer` (feature `bert`) and `ulei-garden`
  (feature `native`) by path. `fuzz/replay` is a dependency-free helper binary of one target.
- Nightly Rust is used only by the fuzz, sanitizer and Miri jobs, pinned in `assurance.yml`; no
  required check uses it. A finding becomes a stable regression test in the crate concerned.

## Consequences

- One more lock file (`fuzz/Cargo.lock`), updated by hand when the workspace's dependencies move.
