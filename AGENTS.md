# AGENTS.md — rules for coding agents in this repository

Read this before changing anything. `README.md` is for humans; this file is for you.

## What this is

`ulei` is the monorepo of an agent ecosystem: core (owner inbox, registry, config, MCP
facade, CLI), hive (cards and heads), garden (facts and retrieval), memory (forest), runtime adapters,
channels, glue to separate products and engine (own agent loop, models only through qb).
**All code is Rust**, optimised for size, RSS, CPU, IO and cold start. Architecture: `docs/architecture/adr/`, generated map:
`docs/architecture/module-map.md`, plan: `docs/roadmap.md`.

## Layout

| Path | What |
|---|---|
| `crates/contracts` | `ulei-contracts`: Rust types **generated** from `contracts/`. The only crate others may depend on. Hand-written code stays under 1,500 lines and is thin wire helpers plus the two decoders of C9, `config` (finds `ulei.toml` from the flag, `ULEI_CONFIG`, `XDG_CONFIG_HOME` and `HOME`, reads it, checks it against `c9::KEYS`) and `env_file` (reads an env-file the config names). These two are the only code in the crate that reads a file or the environment; nothing else in it holds logic. |
| `crates/{core,hive,garden,memory,runtime-adapters,channels,glue,engine,connector-test}` | Class-2 modules. Each depends only on `ulei-contracts`. Each has its own `AGENTS.md`. |
| `contracts/` | Language-neutral contracts C1–C9 (JSON Schema 2020-12 / OpenAPI 3.1), `index.json`, fixtures, `CHANGELOG.md`. Single source of truth. |
| `ecosystem.lock` | Pins of class-1 products (qb). Their code is never edited here. |
| `xtask/` | Tooling: `cargo xtask arch | module-map | gen-contracts | contracts | adr | check`. |
| `ci/budgets.sh`, `ci/inbox_load.py`, `ci/engine_idle.py`, `ci/session_search_run.py`, `ci/web_rss.py`, `ci/tg_idle.py`, `ci/garden_run.py`, `ci/memory_run.py`, `ci/broad_check.py` | Budgets, the stdlib load client of the `ulei-inbox` budgets, the stdlib idle drivers of the `ulei-engine` and `ulei-tg` budgets (`ci/engine_idle.py` also holds the fake gateway stream and C6 helpers the engine drivers share), the stdlib `session_search` latency driver of the `ulei-engine` budgets, the stdlib RSS driver of the `ulei-web` budget and the stdlib driver of the `ulei-garden` budgets with its fake C1 socket, the stdlib driver of the `ulei-memory` budgets; stdlib driver of the local checks. |
| `ci/garden_onnx.py` | Stdlib driver of the `garden-onnx` CI job: writes the tiny ONNX model and its `tokenizer.json` at run time, then runs `ulei-garden serve` with the real `ulei-garden-embed` on it and checks `health` and a dense hit. |
| `ci/status_run.py` | Stdlib driver of the `ulei status` line of the `ulei-inbox` budgets: fake C1, C6 and C4G sockets, one `ulei status` under `strace` and `/usr/bin/time`. |
| `deploy/systemd/` | User units of `ulei-inbox`, `ulei-engine` and `ulei-tg` with their hardening, order, limits and restart (ADR-0042). A unit change keeps the `systemd-units` CI job green: its exposure threshold, `systemd-analyze verify` and the start test. |
| `ci/release.sh` | The static musl release of `ulei`, `ulei-inbox`, `ulei-engine`, `ulei-web` and `ulei-tg` with `SHA256SUMS` (ADR-0042 section 10), optionally rebuilt from a clean target directory and compared; the CI job `release` runs it, uploads the artifact `ulei-x86_64-unknown-linux-musl` and tests the install script on it. |
| `deploy/install.sh` | POSIX sh install of a release directory for the current account without root: checks `SHA256SUMS` first, puts the binaries into `$HOME/.local/bin` and the units into the user's unit directory, prints the `systemctl --user` commands. |
| `deploy/rollback.sh` | POSIX sh rollback of the cutover for the current account without root (`docs/runbooks/mvp-pilot-cutover.md`, part 3): checks `ulei.toml`, its inbox root and the old runtime's unit first, then stops and disables the ulei units, sets `inbox.deliver = false`, makes sure `ENABLED` exists and starts the old unit; deletes nothing. |
| `ci/install_run.py` | Stdlib test of `deploy/install.sh` on the release files under fake homes: files and modes in place, idempotent second run, a corrupted binary refused before any copy. |
| `ci/rollback_run.py` | Stdlib test of `deploy/rollback.sh` under fake homes with a recording `systemctl`: calls in order, the config change, nothing deleted, an idempotent second run, refused runs and a failed step as one stderr line; job `architecture` runs it. |
| `ci/connectors_run.py` | Stdlib check of source connector runs in their transient units (ADR-0056 section 5.7) under the `systemd-units` job's user manager, in four groups, one CI step each so every check is a visible annotation: the kernel, LSM list and Landlock ABI as a notice that fails the step below ABI 3 and, last, no broker socket left in `run_dir`, in every group; `sandbox`: items into a fake C10 with the Landlock ABI in the record, the test connector's socket probes refused, its Landlock file probes (`EACCES` outside its paths and grants, the fake archive unchanged, success inside), the injected `ENOSYS` and `EOPNOTSUPP` refused, the `landlock = "off"` opt-out; `outcomes`: a missing or unexecutable program `refused exec_failed`, a unit that cannot start `refused`, a non-zero exit `exit`, a timeout from `Result=timeout` with its cursor kept, `ulei-connectors exec` refused outside a unit; `broker`: a listed loopback fake source under an owner `allow` reached with its first line, an unlisted entry and a listed loopback name without `allow` `not_allowed`, the 33rd connection `limit`, and under `network = "direct"` its own `AF_INET` socket, `AF_UNIX` refused and connect ports limited from Landlock ABI 4; `schedule`: `ulei-connectors serve` for two periods of 60 s with each connector once per period and an overdue one once at start, `rate_limited` putting a run off across a restart, `sync` every period, 0 voluntary context switches between due times, a manual `run` during a scheduled one `busy`, and SIGTERM stopping the running unit with its record written. |
| `ci/units_run.py` | Stdlib start test of the user units (ADR-0042 section 9): a synthetic `ulei.toml`, fake qb and Bot API, the units under a user manager, `ulei status`, their sandbox, limits and a restart after a fatal exit. |
| `ci/deliver_kill.py` | Stdlib delivery kill test of the owner inbox (ADR-0037 section 6): SIGKILLs `ulei-inbox serve` with `inbox.deliver` and `ulei-engine serve` at the kill points K1 to K9 and at random, then reconciles every event's C1 row with its turns in the journal. |
| `ci/gitleaks_scan.py` | Stdlib driver of the `gitleaks` CI job: the scan and its self-test in a throwaway repository. |

## Commands (all must pass before you push)

If `cargo` is not on `PATH`, use `$HOME/.cargo/bin/cargo`.

```sh
python3 -m ci.broad_check          # fmt, clippy -D warnings, tests (all features), cargo xtask check
cargo xtask gen-contracts          # after any change in contracts/
cargo xtask module-map             # after any change of crates, deps, contracts or pins
cargo build --release --locked -p ulei-core --target x86_64-unknown-linux-musl
cargo build --release --locked -p ulei-engine --target x86_64-unknown-linux-musl
cargo build --release --locked -p ulei-engine --features web --bin ulei-web --target x86_64-unknown-linux-musl
cargo build --release --locked -p ulei-channels --bin ulei-tg --target x86_64-unknown-linux-musl
cargo build --release --locked -p ulei-garden --bin ulei-garden --target x86_64-unknown-linux-musl
cargo build --release --locked -p ulei-garden --features onnx --bin ulei-garden-embed \
  --target x86_64-unknown-linux-gnu
cargo build --release --locked -p ulei-engine --features web-loopback-http --bin ulei-web \
  --target x86_64-unknown-linux-musl --target-dir target/web-loopback && ci/budgets.sh
```

CI also runs `cargo deny check`, the MSRV check (1.89, without `xtask`), the ADR gate, schema
compatibility against the base revision, the engine's contract test against qb built at
the pin (job `qb-contract`, ADR-0038 section 6), the reproducible release with the install
script test (job `release`, ADR-0042 section 10), the garden's retrieval eval on the synthetic
gold set (job `garden-eval`, ADR-0045 section 5) and its ONNX path on a tiny model written at
run time (job `garden-onnx`, ADR-0047 section 8).
Gitleaks scans the history of the commit under check, not the other branches.
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
