# ADR-0001: Rust cargo workspace, module boundaries through contracts, budgets in CI

Status: accepted
Date: 2026-11-16
Affects: crates/contracts, crates/core, crates/hive, crates/memory, crates/runtime-adapters, crates/channels, crates/glue, dep:ulei-contracts, dep:serde, dep:serde_json, dep:jsonschema, dep:toml
Amended by: ADR-0053 (`ulei-engine` is held to 4 MiB static instead of the daemon row's 2.5 MiB).
Amended by: ADR-0068 (decision 5: the Python task hive lives in `hive-py/` next to the stdlib tools of `ci/` until step P6).

## Context

The secretary ecosystem (agent runtime around Hermes, the Hive, the owner inbox, memory,
channels, glue to qb and the A2A gate) grew as Python spread over several repositories,
wired together with `sys.path.insert` and three diverging copies of the inbox. The strangler
plan (step S0) asks for one private monorepo with modules, contracts, ADRs and boundaries
checked by CI before any code moves.

Owner decision, 2026-11-16 13:18: all new ecosystem code is written in Rust and optimised
to the limit from the start (RSS, binary size, CPU, IO, cold start), with budgets enforced in
CI as in qb. The skeleton is a cargo workspace instead of Python packages; module boundaries
are expressed as crate dependencies; existing Python (Hermes, the current Hive, the gate)
talks to the new code only over the wire, never through PyO3.

## Decision

1. **One cargo workspace**, members `crates/{contracts,core,hive,memory,runtime-adapters,channels,glue}`
   (packages `ulei-<dir>`) plus the `xtask` tool. Edition 2024, MSRV 1.89, `unsafe` denied,
   clippy pedantic with `-D warnings`.
2. **Two module classes** (details in ADR-0002). Everything in `crates/` is class 2.
   Class-1 products have their own repositories and are pinned in `ecosystem.lock`.
3. **Boundaries are the crate graph**, checked by `cargo xtask arch` on `cargo metadata`
   (this replaces import-linter):
   - `ulei-contracts` depends on no workspace crate;
   - a module crate depends on exactly one workspace crate, `ulei-contracts`;
   - `xtask` is never a dependency and depends on no workspace crate;
   - path dependencies stay inside the workspace; unknown registries and git sources are
     denied (`deny.toml`);
   - each crate declares `[package.metadata.ulei] class, role, contracts`.
   Source bans (the Rust form of the `sys.path` ban): no `#[path]`, no `include!`, no
   `include_str!/include_bytes!` reaching outside the crate, no spawning another module's
   CLI, Hermes is mentioned only in `crates/runtime-adapters`, no personal paths or ids. An
   integration test of a module may start another module's server binary as the system under
   test and reaches that server only over its contract.
4. **Contracts stay language-neutral**: JSON Schema 2020-12 and OpenAPI 3.1 in `contracts/`
   (ADR-0005). Rust types are generated into `crates/contracts/src/generated/` by
   `cargo xtask gen-contracts`; CI regenerates and fails on a diff.
5. **Python only over the wire**: UDS/HTTP with JSON as specified by the contracts. PyO3,
   `pyo3-ffi` and `cpython` are banned in `deny.toml`. Python in this repository is limited to
   stdlib CI tools under `ci/`: `broad_check.py`, a driver for external harnesses,
   `inbox_load.py`, the load client of the `ulei-inbox` budgets, and `engine_idle.py`, the
   idle driver of the `ulei-engine` budgets.
6. **Generated architecture map**: `docs/architecture/module-map.md` is produced by
   `cargo xtask module-map` from `cargo metadata`, `contracts/index.json` and `ecosystem.lock`.
7. **ADR gate**: `cargo xtask adr --base <rev>` requires a new ADR with a matching
   `Affects:` line for a new crate (`crates/<dir>`), an added or removed dependency
   (`dep:<name>`), a new or removed class-1 pin (`lock:<product>`), or a new or removed
   contract (`contract:<id>`).
8. **Size and speed first**:
   - release profile `opt-level = "s"` (`"z"` for the package `ulei-engine`, ADR-0027), fat
     LTO, one codegen unit, `panic = "abort"`, strip; static musl binaries;
   - C code that `cc` compiles for the musl target (bundled SQLite, `ring`) has no unwind
     tables: `CFLAGS_x86_64_unknown_linux_musl` in `.cargo/config.toml` passes
     `-fno-asynchronous-unwind-tables -fno-unwind-tables`. With `panic = "abort"` nothing
     unwinds through C frames; the tables cost `ulei-engine` 90,112 B;
   - synchronous, small stacks: tokio, async-std, hyper, axum, reqwest, openssl-sys and chrono
     are banned; lifting a ban needs an ADR with measurements;
   - no background threads or timers in idle; daemons wake only on I/O or the nearest deadline.

## Budgets

Enforced by `ci/budgets.sh` in the `budgets` job; never raised to make CI green.

| Binary | Static size | Peak RSS | Cold start | IO |
|---|---|---|---|---|
| `ulei` CLI (S0, `--version`) | ≤ 512 KiB | ≤ 1 MiB | median ≤ 3 ms incl. spawn | ≤ 40 syscalls, 0 file opens |
| every daemon (from S1) | ≤ 2.5 MiB | ≤ 4 MiB after 10⁴ requests | ≤ 10 ms to ready | 0 wake-ups/s idle; write bytes per event declared in its ADR |

Measured on the skeleton (local, 2026-11-16): 373 KiB, 256 kB, 0.5 ms, 22 syscalls.

## Consequences

- Module code moves out of Python step by step (strangler): each step puts a Rust
  implementation next to the Python one, runs it in shadow mode, then switches by a flag.
- The S1 owner inbox is written in Rust with the same SQLite file and packet format and runs
  in shadow next to the current Python copy before any consumer switches.
- `xtask` itself is tooling and is not covered by the MSRV job or the budgets.
- The CI gates are: fmt, clippy, test, msrv, deny, gitleaks, architecture, contracts, budgets.
  `python3 -m ci.broad_check` runs the local subset.
