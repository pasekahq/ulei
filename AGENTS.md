# AGENTS.md — rules for coding agents in this repository

Read this before changing anything. `README.md` is for humans; this file is for you.

## What this is

`ulei` is the monorepo of an agent ecosystem: core (owner inbox, registry, config, MCP
facade, CLI), hive (cards and heads), memory (garden, retrieval, forest), runtime adapters,
channels and glue to separate products. **All code is Rust**, optimised for size, RSS, CPU,
IO and cold start. Architecture: `docs/architecture/adr/`, generated map:
`docs/architecture/module-map.md`, plan: `docs/roadmap.md`.

## Layout

| Path | What |
|---|---|
| `crates/contracts` | `ulei-contracts`: Rust types **generated** from `contracts/`. The only crate others may depend on. No business logic. |
| `crates/{core,hive,memory,runtime-adapters,channels,glue}` | Class-2 modules. Each depends only on `ulei-contracts`. Each has its own `AGENTS.md`. |
| `contracts/` | Language-neutral contracts C1–C7 (JSON Schema 2020-12 / OpenAPI 3.1), `index.json`, fixtures, `CHANGELOG.md`. Single source of truth. |
| `ecosystem.lock` | Pins of class-1 products (qb). Their code is never edited here. |
| `xtask/` | Tooling: `cargo xtask arch | module-map | gen-contracts | contracts | adr | check`. |
| `ci/budgets.sh`, `ci/broad_check.py` | Budgets; stdlib driver of the local checks. |

## Commands (all must pass before you push)

If `cargo` is not on `PATH`, use `$HOME/.cargo/bin/cargo`.

```sh
python3 -m ci.broad_check          # fmt, clippy -D warnings, tests, cargo xtask check
cargo xtask gen-contracts          # after any change in contracts/
cargo xtask module-map             # after any change of crates, deps, contracts or pins
cargo build --release --locked -p ulei-core --target x86_64-unknown-linux-musl && ci/budgets.sh
```

CI also runs `cargo deny check`, gitleaks over the full history, the MSRV check (1.85, without
`xtask`), the ADR gate and schema compatibility against the base revision.
Release builds use fat LTO: one cargo build at a time on shared hosts, under `nice`.

## Hard rules

1. **A module talks to another module only through `ulei-contracts` and the wire.** Never add a
   dependency between module crates. `cargo xtask arch` fails on it.
2. **New crate, new or removed dependency, new pin or contract: write the ADR first**
   (`docs/architecture/adr/NNNN-slug.md` with `Status:`, `Date:`, `Affects:` naming
   `crates/<dir>`, `dep:<name>`, `lock:<product>` or `contract:<id>`).
3. **Contracts change only in `contracts/`**: bump the version in `index.json`, add a
   `CHANGELOG.md` entry, add or adjust fixtures, regenerate. Breaking = major bump + ADR.
   Never edit `crates/contracts/src/generated/` or `crates/contracts/tests/fixtures.rs` by hand.
4. **Class-1 products (qb, later the gate) are not edited here**: only bump the pin in
   `ecosystem.lock` together with the vendored contract.
5. **Python is reached only over the wire** (UDS/HTTP + JSON per contract). No PyO3.
6. **Budgets are part of the contract** (ADR-0001). Do not raise a limit to make CI green.
   Banned crates (tokio, hyper, reqwest, openssl, chrono, pyo3, ...) stay banned unless an ADR
   with measurements lifts the ban.
7. **No secrets and nothing personal**: no tokens, chat ids, real account names, home paths or
   host names in code, tests or fixtures. Deployment specifics come from config.
8. Only `crates/runtime-adapters` may know about Hermes. No `#[path]`, no `include!`.

## Conventions

- Rust edition 2024, MSRV 1.85; `unsafe` denied; errors are values (no `unwrap()` outside tests).
- Synchronous code; daemons sleep until I/O or the nearest deadline (0 wake-ups when idle).
- Docs in English. Commits in imperative mood; one logical change; green CI before merge.
