# ADR-0053: `ulei-engine` static size budget of 4 MiB

Status: accepted
Date: 2026-11-22
Affects: crates/engine

## Context

ADR-0010 section 4 held the static musl `ulei-engine` to 2,621,440 B, the "every daemon" row of
ADR-0001. At main d5b7d89 the binary measured 2,620,544 B, 896 B under that limit. The
remaining engine items of the U5 parity step of `docs/roadmap.md` (more ported tools and their
behaviour) add code to this binary and do not fit in that room.

ADR-0001 raises no limit to make CI green. The owner decided on 2026-11-22 to raise this one
limit to 4 MiB (4,194,304 B).

## Decision

- `ci/budgets.sh` holds the static release binary `ulei-engine` for
  `x86_64-unknown-linux-musl` to at most 4,194,304 B (`ULEI_ENGINE_MAX_BINARY_BYTES`).
  ADR-0010 section 4 carries the figure.
- Every other limit stays: the engine's idle VmHWM of 4,096 kB and its 0 idle wake-ups, and
  every limit of every other binary, including ADR-0001's 2.5 MiB daemon row.

## Consequences

- The engine has about 1.5 MiB of static room for the remaining U5 items.
- A further raise again needs the owner's decision and an ADR.
