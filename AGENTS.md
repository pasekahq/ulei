# AGENTS.md — rules for coding agents in this repository

Read this before changing anything. `README.md` is for humans; this file is for you.

## What this is

`ulei` is the monorepo of an agent ecosystem: core (owner inbox, registry, config, MCP
facade, CLI), hive (cards and heads), memory (garden, retrieval, forest), runtime adapters,
channels, glue to separate products and engine (own agent loop, models only through qb).
**All code is Rust**, optimised for size, RSS, CPU, IO and cold start. Architecture: `docs/architecture/adr/`, generated map:
`docs/architecture/module-map.md`, plan: `docs/roadmap.md`.

## Layout

| Path | What |
|---|---|
| `crates/contracts` | `ulei-contracts`: Rust types **generated** from `contracts/`. The only crate others may depend on. No business logic. |
| `crates/{core,hive,memory,runtime-adapters,channels,glue,engine}` | Class-2 modules. Each depends only on `ulei-contracts`. Each has its own `AGENTS.md`. |
| `contracts/` | Language-neutral contracts C1–C8 (JSON Schema 2020-12 / OpenAPI 3.1), `index.json`, fixtures, `CHANGELOG.md`. Single source of truth. |
| `ecosystem.lock` | Pins of class-1 products (qb). Their code is never edited here. |
| `xtask/` | Tooling: `cargo xtask arch | module-map | gen-contracts | contracts | adr | check`. |
| `ci/budgets.sh`, `ci/inbox_load.py`, `ci/engine_idle.py`, `ci/web_rss.py`, `ci/broad_check.py` | Budgets, the stdlib load client of the `ulei-inbox` budgets, the stdlib idle driver of the `ulei-engine` budgets and the stdlib RSS driver of the `ulei-web` budget; stdlib driver of the local checks. |

## Commands (all must pass before you push)

If `cargo` is not on `PATH`, use `$HOME/.cargo/bin/cargo`.

```sh
python3 -m ci.broad_check          # fmt, clippy -D warnings, tests (all features), cargo xtask check
cargo xtask gen-contracts          # after any change in contracts/
cargo xtask module-map             # after any change of crates, deps, contracts or pins
cargo build --release --locked -p ulei-core --target x86_64-unknown-linux-musl
cargo build --release --locked -p ulei-engine --target x86_64-unknown-linux-musl
cargo build --release --locked -p ulei-engine --features web --bin ulei-web --target x86_64-unknown-linux-musl
cargo build --release --locked -p ulei-engine --features web-loopback-http --bin ulei-web \
  --target x86_64-unknown-linux-musl --target-dir target/web-loopback && ci/budgets.sh
```

CI also runs `cargo deny check`, gitleaks over the full history, the MSRV check (1.89, without
`xtask`), the ADR gate and schema compatibility against the base revision.
Release builds use fat LTO: one cargo build at a time on shared hosts, under `nice`.
The musl build compiles bundled SQLite and needs `x86_64-linux-musl-gcc` (package `musl-tools`).

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

## Reference implementation: port Hermes, do not invent (owner directive, 2026-11-17)

The behaviour of the secretary agent already exists and works in Hermes Agent
(NousResearch/hermes-agent, MIT, pinned version plus the owner's local patches). `ulei` ports it
to Rust; it does not design its own.

1. **Hermes is the reference.** A card that implements agent behaviour (tool loop, tools,
   sessions, compaction, system prompt, skills, MCP, web, guards, steer/interrupt/model switch,
   the session API behind C6) ports the behaviour and algorithms of the Hermes modules named for
   it in the port map: same defaults, thresholds, ordering, tool names and argument shapes,
   error texts the model sees, edge cases. Read the Hermes code first, then write the Rust.
2. **Only what the owner's assistant really uses.** The port map lists the Hermes modules in
   live use and an explicit "not ported" list (other messengers, unused toolsets and providers,
   RL/batch runners, setup wizards, unused plugins, ...). Port only from the map; do not pull in
   anything else.
3. **Our changes on top of Hermes are listed in the map** (models only through the qb gateway,
   Rust budgets, invasive steer / emergency cancel, fixes of known Hermes failures), each tied to
   the Hermes module it modifies. Everything else behaves as in Hermes.
4. **Deviations are deliberate and recorded.** Any other difference from Hermes needs an ADR
   (`Affects:` the crate) that names the Hermes module and says why. Inventing an own design
   where Hermes has one is not allowed.
5. **Attribution.** Hermes is MIT (Copyright (c) 2025 Nous Research); its notice is in
   `THIRD_PARTY_NOTICES.md` under the neutral id `ref-agent`. A ported Rust file starts with
   `// Port: THIRD_PARTY_NOTICES.md#ref-agent`, and the file is added to the "Ported modules"
   table there with the upstream paths it ports. Never put the product name or an upstream path
   that contains it into `.rs` files outside `crates/runtime-adapters`.
6. **Hard rule 8 still holds.** It is about code: no dependency on, call to, or naming of Hermes
   outside `crates/runtime-adapters` (`cargo xtask arch` enforces it). Porting behaviour into other
   crates under this section does not break it.

The port map lives with the planning notes of the deployment (outside this repository); the
sprint brief gives its location. If a card has no map entry for the behaviour it needs, ask the
PO instead of inventing one.

## Conventions

- Rust edition 2024, MSRV 1.89; `unsafe` denied; errors are values (no `unwrap()` outside tests).
- Synchronous code; daemons sleep until I/O or the nearest deadline (0 wake-ups when idle).
- Docs in English. Commits in imperative mood; one logical change; green CI before merge.
