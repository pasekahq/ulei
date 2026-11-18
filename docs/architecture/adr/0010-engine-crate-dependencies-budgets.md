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

`ulei-engine` takes the "every daemon" row of ADR-0001. `ci/budgets.sh` holds the release
binary built for `x86_64-unknown-linux-musl` to:

| Measure | Limit | Environment variable |
|---|---|---|
| Static size | ≤ 2,621,440 B (2.5 MiB), statically linked | `ULEI_ENGINE_MAX_BINARY_BYTES` |
| Idle RSS | ≤ 4,096 kB (4 MiB) | `ULEI_ENGINE_MAX_RSS_KB` |
| Idle wake-ups | 0 voluntary context switches over 10 s idle | `ULEI_ENGINE_MAX_IDLE_WAKEUPS` |

Idle is the one waiting state of `run`: the process has sent its turn request and is blocked
reading the gateway socket before any answer. The idle timeout of `run` (600 s) keeps it
there well past the measuring window.

How each budget is measured:

- Size: `stat -c %s` of the binary; `file` must not report it dynamically linked.
- Idle: `ci/engine_idle.py` listens on a fake gateway socket and starts `ulei-engine run` with
  a config file naming that socket, a fresh journal and a service unit, so the `terminal`
  guard is on as in a deployed service, all in a work directory on the runner
  disk (`/var/tmp`, never tmpfs). The fake reads the complete `POST /v1/model/turn` request
  and never answers. After the request and a settling second, the driver reads
  `voluntary_ctxt_switches` from `/proc/<pid>/status`, again 10 s later, and then `VmHWM`;
  wake-ups are the difference and idle RSS is `VmHWM`. It kills the process and removes its
  files. The run fails when the engine exits or sends no complete turn request within 10 s.

## Consequences

- Later engine work adds code to this crate, not a new crate; each new dependency still needs
  its own ADR.
- The binary with everything linked, measured on gnu, leaves about 0.75 MiB of the static
  budget.
