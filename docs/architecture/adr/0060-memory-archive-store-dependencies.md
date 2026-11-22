# ADR-0060: `rusqlite`, `sha2`, `miniz_oxide` and `serde_json` in the memory crate for the archive store

Status: accepted
Date: 2026-11-22
Affects: crates/memory, dep:rusqlite, dep:sha2, dep:miniz_oxide, dep:serde_json

## Context

ADR-0051 section 2 decides the archive store: one SQLite file with FTS5, items keyed by the
SHA-256 of their bytes, bodies as raw deflate streams through `miniz_oxide` (section 9), and a
`deletions` audit row that keeps the removed entries' metadata. Its card 7 builds that store as a
library in `ulei-memory` and carries the record the ADR gate needs for the crate's new
dependencies. The standard library has no SQLite, hash or deflate; the garden's and the engine's
code cannot be shared across the crate boundary (ADR-0001 section 3).

## Decision

- `crates/memory/Cargo.toml` takes the workspace declarations `rusqlite` (bundled SQLite with
  FTS5, 0.43.0 in `Cargo.lock`, as the garden and the engine use it), `sha2` (0.11.1, without
  default features) and `serde_json` (1.0.154, the encoding of the audit row's entries, since
  `.cargo/config.toml` omits SQLite's JSON functions).
- The workspace declares `miniz_oxide` 0.9 without default features and with `with-alloc`;
  `Cargo.lock` resolves `miniz_oxide` 0.9.1 and `adler2` 2.0.1, the crates and versions
  ADR-0051 section 9 checked. The `simd` feature and its `simd-adler32` stay off.

## Consequences

- The lock gains `miniz_oxide` and `adler2`, both pure Rust and offering MIT, which `deny.toml`
  admits; `rusqlite`, `sha2` and `serde_json` were already resolved.
- The daemon of ADR-0051 card 8 reuses these dependencies and adds only its transport's.
