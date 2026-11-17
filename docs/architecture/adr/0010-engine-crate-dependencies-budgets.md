# ADR-0010: `ulei-engine` crate, its dependencies and budgets

Status: accepted
Date: 2026-11-17
Affects: crates/engine, dep:ulei-contracts

## Context

ADR-0009 puts the own agent loop into `crates/engine`. The crate needs a manifest, a binary
and the budgets it is held to before loop code lands in it.

## Decision

### 1. Crate

- `crates/engine`, package `ulei-engine`, class 2 (`[package.metadata.ulei]`), contracts C4G,
  C6 and C8.
- Its only workspace dependency is `ulei-contracts` (ADR-0001); it talks to other modules only
  through the wire.
- One binary, `ulei-engine`. `ulei-engine --version` prints `ulei-engine <version>` and exits
  0; `ulei-engine run` runs one turn of a journaled session (settings, exit codes and checks
  in `docs/runbooks/engine-live-smoke.md`); every other invocation prints a one-line usage to
  stderr and exits 2.

### 2. Dependencies

| Crate | Features | Why |
|---|---|---|
| `ulei-contracts` (workspace) | none | Generated wire types of the contracts the engine speaks. |

No third-party crate and no C dependency. A dependency that a later part of the engine needs
(for example `serde_json` for the gateway stream or `rusqlite` for the session journal) comes
with its own ADR.

### 3. Measured size

Release profile of ADR-0001, rustc 1.100.0, `x86_64-unknown-linux-gnu` (dynamically linked
to glibc, stripped), as in ADR-0014 section 5: the musl build of bundled SQLite needs
`x86_64-linux-musl-gcc`, which the measuring host lacks.

```sh
cargo build --release --locked -p ulei-engine
stat -c %s target/release/ulei-engine
```

| Build | Size |
|---|---|
| `ulei-engine` with `run`: loop, tools, policy chain, gateway client, journal and config reader | 1,834,744 B |

### 4. Budgets

`ulei-engine` takes the "every daemon" row of ADR-0001. `ci/budgets.sh` holds the finished
binary to:

| Measure | Limit |
|---|---|
| Static size | ≤ 2,621,440 B (2.5 MiB), statically linked |
| Idle RSS | ≤ 4,096 kB (4 MiB) |
| Idle wake-ups | 0 voluntary context switches over 10 s idle |

The `ulei-engine` section of `ci/budgets.sh` lands with the first long-running mode of the
binary; `run` exits after one turn.

## Consequences

- Later engine work adds code to this crate, not a new crate; each new dependency still needs
  its own ADR.
- The binary with everything linked, measured on gnu, leaves about 0.75 MiB of the static
  budget.
