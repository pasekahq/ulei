# ADR-0027: `ulei-engine` size: opt-level "z" for its package and one shared sort

Status: accepted
Date: 2026-11-18
Affects: crates/engine

## Context

The static musl `ulei-engine` measured 2,530,048 B against its 2,621,440 B budget (ADR-0010
section 4), 91,392 B of room, with idle VmHWM 2,764 kB of 4,096 kB and 0 idle wake-ups. The
C6 server over the ADR-0008 wire still has to land in this binary. Limits are not raised to
make room (ADR-0001), so the room comes from size-neutral changes that keep behaviour and every
model-visible text as they are.

## Decision

### 1. Opt-level "z" for the package `ulei-engine`

`Cargo.toml` sets `[profile.release.package.ulei-engine] opt-level = "z"` and repeats
`strip = true`. The rest of the release profile of ADR-0001 stays.

- Under fat LTO with one codegen unit, the merged module of a binary is optimised and
  generated at the opt-level of the package that links it. Profile-wide "z" with
  `ulei-engine` held at "s" builds 2,505,904 B, the size of "s" everywhere, while "z" for the
  package alone builds 2,319,800 B. The override therefore governs both binaries of the
  package, `ulei-engine` and `ulei-web`; `ulei` and `ulei-inbox` are unchanged to the byte.
- A package override resets `strip` to cargo's default for its package (`debuginfo`), which
  keeps the symbol table: 2,868,448 B without the repeated `strip = true`.
- `ulei-engine` has no CPU or latency budget; its work waits on the gateway, the tools and the
  journal.

### 2. Shared sort and map building

- Stable sorts of the crate go through `crate::sort` (`sort`, `sort_by`): one instantiation of
  the slice sort over indices with a `dyn` comparator, then an in-place permutation. Each
  direct slice-sort call site, and each `BTreeSet` or `serde_json::Map` built by `collect`,
  instantiates its own sort of 3 to 6 KB. Every replaced unstable sort compares whole values
  or unique keys, so its order is the stable order.
- `serde_json::Map` values are rewritten in place (`values_mut`) or inserted in a loop instead
  of collected from a mapped iterator.

### 3. Measurements

Release profile of ADR-0001, rustc 1.100.0, `x86_64-unknown-linux-gnu` stripped, measured as
in ADR-0010 section 3 (`--target x86_64-unknown-linux-gnu`); symbols from the same build with
`strip = false`. C1 is the map building, C2 the shared sort.

| Build of `ulei-engine` | Size | Delta |
|---|---|---|
| Base, "s" | 2,490,296 B | |
| "z" for the package | 2,319,800 B | -170,496 B |
| "z" for the whole profile | 2,307,768 B | -182,528 B |
| "z" for the whole profile, package at "s" | 2,505,904 B | +15,608 B |
| `-C force-unwind-tables=no` | 2,490,296 B | 0 B |
| C1, "s" | 2,462,904 B | -27,392 B |
| C2, "s" | 2,430,264 B | -60,032 B |
| C1 and C2, "s" | 2,401,336 B | -88,960 B |
| C1, "z" | 2,295,864 B | -194,432 B |
| C2, "z" | 2,266,424 B | -223,872 B |
| Adopted: "z", C1 and C2 | 2,241,080 B | -249,216 B |

| Other binary | Base | "z" for the package (adopted) | "z" for the whole profile |
|---|---|---|---|
| `ulei-web` | 1,794,584 B | 1,790,944 B | 1,766,192 B |
| `ulei` | 312,184 B | 312,184 B | 311,288 B |
| `ulei-inbox` | 1,251,952 B | 1,251,952 B | 1,242,096 B |

From base to the adopted build, `.text` shrinks by 297,504 B; `.eh_frame` and
`.eh_frame_hdr` grow by 42,232 B, the frame entries of functions "z" no longer inlines. The
largest symbol deltas: `search::Request::run` -16,658 B, `Redactor::sensitive_text`
-16,185 B, `mcp::connect` -12,495 B, `tools` -11,625 B, `main` -10,117 B,
`Compressor::compress` -8,307 B, against `turn` +54,446 B, which "s" inlines into its
callers. The slice-sort symbols outside std and `regex-lite` fall from 86,781 B to 4,895 B,
one `usize` sort in place of about 20, among them the five `(String, Value)` sorts of the
collected maps (22,118 B).

### 4. Not adopted

| Lever | Measured | Why not |
|---|---|---|
| "z" for the whole profile | `ulei-engine` -12,032 B beyond the package, `ulei` -896 B, `ulei-inbox` -9,856 B, `ulei-web` -24,752 B | Puts the cold start, syscall, publish p99 and ready budgets of `ulei` and `ulei-inbox` at stake for binaries that need no room. |
| `-C force-unwind-tables=no` | 0 B, byte-identical | Under fat LTO the frame entries stay; the remaining Rust entries go only with a rebuilt std (nightly). |
| `#[inline(never)]` on the compressor's `summary_window`, `prune_old_tool_results`, `tail_cut`, `generate_summary` and `assemble` | +512 B at "s", +384 B at "z" | Each has one call site. |
| Hand-written `Debug` for the engine's error types | at most 1,963 B, every engine `Debug` symbol | `std::error::Error` needs `Debug`; a shorter one saves less. |
| One copy of the shared sentence of the two masking notes of `terminal` | about 150 B | The only engine text duplicated in `.rodata`; equal literals are already merged. |
| The std backtrace symbolizer (`gimli`, `addr2line`, `rustc_demangle`, `miniz_oxide`) | 166,269 B of symbols | Reached from the default panic hook; only nightly `panic = "immediate-abort"` drops it. |
| `--remap-path-prefix` | 11,708 B of source paths in panic locations | Shortens, does not remove; CI's global `RUSTFLAGS` overrides target rustflags. |
| `-C relocation-model=static` (no PIE) | -88,576 B on the adopted build (`.rela.dyn` 88,848 B to 3,720 B) | Gives up ASLR of the engine and needs a rustflag. |

## Consequences

- The musl `ulei-engine` measures 2,276,096 B in the `budgets` job, leaving 345,344 B
  (337 KiB) of its budget for the C6 server.
- A new sort in `crates/engine` goes through `crate::sort` (crate `AGENTS.md`).
