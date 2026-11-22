# ADR-0053: `ulei-engine` static size limit of 4 MiB

Status: accepted
Date: 2026-11-22
Affects: crates/engine

## Context

ADR-0010 section 4 held the static musl `ulei-engine` to 2,621,440 B (2.5 MiB), the "every
daemon" row of ADR-0001. At main `d5b7d89` the release binary measures 2,620,544 B in the
`budgets` job, 896 B below that limit, and the planned engine work (ADR-0051 section 8) still
has to land in it. A raise of a budget is the owner's word, never a way to make CI green
(ADR-0001).

## Decision

1. By the owner's decision the static size limit of `ulei-engine` is 4,194,304 B (4 MiB):
   the default of `ULEI_ENGINE_MAX_BINARY_BYTES` in `ci/budgets.sh` and the row of ADR-0010
   section 4.
2. Nothing else changes: the engine stays statically linked, its idle RSS (4,096 kB) and
   idle wake-up (0) limits stay, and every other binary keeps its limits, the 2.5 MiB daemon
   row of ADR-0001 included.
3. No size reduction is applied with this raise. The attribution of the binary at `d5b7d89`,
   its growth by merge and every reduction found, with its byte saving and its risk, are in
   `docs/architecture/engine-size.md`. Adopting one of them is a change of its own.

## Consequences

- The engine has 1,573,760 B of room at `d5b7d89`.
- The size note is rewritten in place when a reduction is adopted or the binary is measured
  again.
