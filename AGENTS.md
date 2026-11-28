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
| `crates/contracts` | `ulei-contracts`: Rust types **generated** from `contracts/`. The only crate modules may depend on besides library crates. Hand-written code stays under 1,500 lines and is thin wire helpers plus the two decoders of C9, `config` (finds `ulei.toml` from the flag, `ULEI_CONFIG`, `XDG_CONFIG_HOME` and `HOME`, reads it, checks it against `c9::KEYS`) and `env_file` (reads an env-file the config names). These two are the only code in the crate that reads a file or the environment; nothing else in it holds logic. |
| `crates/{core,hive,garden,memory,runtime-adapters,channels,glue,engine,connector-test,connector-mcp-example,hive-mcp,priv}` | Class-2 modules. Each depends only on `ulei-contracts` and on library crates that list it in their `users`. Each has its own `AGENTS.md`. |
| `crates/infer` | Library crates (class `"lib"`, ADR-0072): pure computation, no workspace dependency, no binary, no config, environment, process or socket; each names the modules that may link it in `[package.metadata.ulei] users`. No budget row of their own. Each has its own `AGENTS.md`. |
| `crates/garden-embed` | Binary packages (class `"bin"`, ADR-0104): binaries of the one module crate named in `[package.metadata.ulei] module`, built at another opt-level than that module's package; no library, no build script, no contract of their own; they depend on their module, `ulei-contracts` and library crates that list them in `users`. `ulei-garden-embed` is the garden's embedder helper. Each has its own `AGENTS.md`. |
| `hive-py/` | The Python task hive (ADR-0068), the one Python tree besides `ci/` until step P6; no cargo package. Its own `AGENTS.md`, `module.toml`, hash-pinned locks, `SOURCE` and the CI job `hive-py`. |
| `contracts/` | Language-neutral contracts C1 to C11 (JSON Schema 2020-12 / OpenAPI 3.1), `index.json`, fixtures, `CHANGELOG.md`. Single source of truth. |
| `ecosystem.lock` | Pins of class-1 products (qb). Their code is never edited here. |
| `xtask/` | Tooling: `cargo xtask arch | module-map | gen-contracts | contracts | adr | units | check`; `adr` refuses two files under `docs/architecture/adr/` with one number, naming both (a number held for an ADR not yet written is a file with `Status: reserved` and no decision); `units` refuses a `deploy/systemd/*.service` without one `Slice=` of a class slice (ADR-0103). |
| `ci/budgets.sh`, `ci/inbox_load.py`, `ci/engine_idle.py`, `ci/session_search_run.py`, `ci/web_rss.py`, `ci/tg_idle.py`, `ci/garden_run.py`, `ci/memory_run.py`, `ci/connectors_budget.py`, `ci/broad_check.py` | Budgets, the stdlib load client of the `ulei-inbox` budgets, the stdlib idle drivers of the `ulei-engine` and `ulei-tg` budgets (`ci/engine_idle.py` also holds the fake gateway stream and C6 helpers the engine drivers share), the stdlib `session_search` latency driver of the `ulei-engine` budgets, the stdlib RSS driver of the `ulei-web` budget and the stdlib driver of the `ulei-garden` budgets with its fake C1 socket, the stdlib driver of the `ulei-memory` budgets, the stdlib driver of the `ulei-hive-mcp` budgets `ci/hive_mcp_budget.py` (a stdlib fake hive CLI, 40 MCP sessions of the five tools), the stdlib driver of the `ulei-connectors` budgets (`idle` in `ci/budgets.sh`; `runs`, 100 runs of the test connector four at a time for VmHWM and wchar per run, then a scripted MCP connector's page of ten items of about 1 MiB and one over the cap for VmHWM, in job `systemd-units`); stdlib driver of the local checks. |
| `ci/night_run.py` | Stdlib end to end check of the night gardener `ulei-engine night` (ADR-0078) in job `release`: the released `ulei-garden serve` and `ulei-memory serve` on fresh stores and a scripted C4G gateway; a dry run that sends and writes nothing, the seedbed into facts and the digest, a malformed answer leaving its record for the next run, the forget queue closing a fact, three archive sessions into facts and summaries with a rerun sending nothing, the spend limit stopping a run the next run resumes, and the judge of stage d merging a duplicate silently, recording a contradiction with its reason, and an undo of the merge (ADR-0096). |
| `ci/reconcile_eval/`, `ci/judge_eval.py` | The evals of the night's judge (ADR-0096): `gen.py` with its phrase bank `bank.py` writes (and `--check`s) the frozen synthetic selection set of `crates/garden/tests/fixtures/reconcile-eval/` that `crates/garden/tests/reconcile_eval.rs` scores; `judge-eval.jsonl` is its base set with E15's e5-small hints and gold relations, which `ci/judge_eval.py` scores a judge on through the qb gateway, an OpenAI-compatible endpoint or the headless Claude Code CLI (`--claude`), by hand or in workflow `judge-eval.yml` (on demand, weekly and path-filtered pull requests, never required, skipped without a Claude credential; the CLI pinned by hash in `ci/claude-cli/`); `--self-test`, no model, runs in job `architecture`. |
| `ci/priv_run.py` | Stdlib driver of the `ulei-priv` budgets: `serve` as the runner's user on a temp policy, 200 calls with one audit line each, then VmHWM and the idle wake-ups. |
| `ci/annotations.py` | The GitHub annotations of the stdlib CI drivers with escaped titles and messages, and the folding of a step's passed checks into a few notices. |
| `ci/garden_onnx.py` | Stdlib driver of the `garden-onnx` CI job: writes the tiny ONNX model and its `tokenizer.json` at run time, then runs `ulei-garden serve` with the real `ulei-garden-embed` on it and checks `health` and a dense hit. |
| `ci/garden_encoder.py` | Stdlib driver of the `garden-encoder` CI job (ADR-0087 E6): writes a tiny BERT checkpoint and converts it with `ulei-garden-embed convert` and checks it with `ulei-garden-embed verify`, then runs the check of `ci/garden_onnx.py serve` with the static musl helper on the engine's weight file. |
| `ci/status_run.py` | Stdlib driver of the `ulei status` line of the `ulei-inbox` budgets: fake C1, C6 and C4G sockets, one `ulei status` under `strace` and `/usr/bin/time`. |
| `deploy/systemd/` | User units of `ulei-inbox`, `ulei-engine`, `ulei-tg`, the connector runner `ulei-connectors`, the archive `ulei-memory`, the hive tools `ulei-hive-mcp` (ADR-0071; its hive root by a drop-in `ReadWritePaths=`) the garden `ulei-garden` with its embedder helper as a child (ADR-0075), the night gardener's oneshot `ulei-night.service` with its timer `ulei-night.timer` (ADR-0078), the uncertain alerter's oneshot `ulei-uncertain.service` with its timer `ulei-uncertain.timer` (ADR-0097 section 5) and the hive's stall watchdog `ulei-hive-watchdog.service` (`hive.py watchdog`, its interpreter, `hive.py` and config from `%E/ulei/hive-watchdog.env`) with its timer `ulei-hive-watchdog.timer` (ADR-0101) with their hardening, order, limits and restart (ADR-0042, ADR-0056, ADR-0070), and the class slices of ADR-0103: `ulei.slice` (interactive, uncapped), `hive-heads.slice` and `hive-urgent.slice` (the hive's heads and its urgent runs) and `background.slice` (`ulei-night`, `ulei-connectors` and its runs); every service sets one of them as `Slice=`. A unit change keeps the `systemd-units` CI job green: its exposure threshold, `systemd-analyze verify` and the start test. `ulei-engine.service.d/10-deny-secrets.conf` is the engine's drop-in of paths its tools must not read; `system/` holds the engine's system unit for hosts whose user manager cannot create namespaces with the socket and service of its qb gateway proxy `ulei-qb-proxy` and the drop-ins of the user units beside it (ADR-0069), and the root unit of the privileged helper `ulei-priv.service` (ADR-0073); `deploy/install.sh` installs none of them, and CI runs only the engine's with its proxy (`ci/units_run.py system`). |
| `deploy/priv/priv.example.toml` | The example policy of `ulei-priv serve` (ADR-0073): the root commands the skills use, with `<ULEI_USER>` and `<ULEI_UID>` to fill; the owner installs it as `/etc/ulei/priv.toml`. |
| `ci/release.sh` | The static musl release of `ulei`, `ulei-inbox`, `ulei-engine`, `ulei-web`, `ulei-tg`, `ulei-connectors`, `ulei-memory`, `ulei-hive-mcp`, `ulei-priv`, `ulei-garden` and its embedder helper `ulei-garden-embed` with the encoder engine (`native`, ADR-0075, ADR-0104), every binary checked static, and the test connector's tree `connectors/test/` (its binary and the manifest kept in `deploy/connectors/test/`, which the released `ulei-connectors list` must accept) with one `SHA256SUMS` by relative path (ADR-0042 section 10, ADR-0056 section 6, ADR-0070), checked by `deploy/install.sh --verify` to hold and list every binary the installer expects, optionally rebuilt from a clean target directory and compared; the CI job `release` runs it, uploads the artifact `ulei-x86_64-unknown-linux-musl` and tests the install script on it. |
| `deploy/install.sh` | POSIX sh install of a release directory for the current account without root: checks the whole `SHA256SUMS` first, puts the binaries into `$HOME/.local/bin` and the units into the user's unit directory, the capped class slices with `CPUQuota` from `nproc` and IOPS caps from `[host.io]` of `ulei.toml` (none without it, said in its output, as is a user manager without the io controller), and installs no connector and not `ulei-priv` (a root step of the owner), prints the `systemctl --user` commands; with `--connector NAME --dir DIR` it installs only that released connector into `DIR/NAME/` and prints the `ulei.toml` keys to set; with `--verify` it only checks the release and writes nothing. |
| `deploy/rollback.sh` | POSIX sh rollback of the cutover for the current account without root (`docs/runbooks/mvp-pilot-cutover.md`, part 3): checks `ulei.toml`, its inbox root and the old runtime's unit first, then stops and disables the ulei units (with `ulei-hive-mcp.service` when it is installed, skipped with a note when its LoadState is `not-found`), sets `inbox.deliver = false`, makes sure `ENABLED` exists and starts the old unit; deletes nothing. |
| `ci/install_run.py` | Stdlib test of `deploy/install.sh` on the release files under fake homes: files and modes in place and no connector, the class slices' `CPUWeight`, `CPUQuota` and, from a synthetic `[host.io]`, `IOReadIOPSMax` and `IOWriteIOPSMax`, a malformed `[host.io]` refused before any copy, enable lines naming `ulei-uncertain.timer` and `ulei-hive-watchdog.timer`, idempotent second run, a corrupted binary refused before any copy; with `--connector test`, exactly its two files with their modes, an idempotent second run, a corrupted connector file and an unknown name refused before any copy; `--verify` accepting the release and refusing it with any one expected binary missing from its files or from `SHA256SUMS`. |
| `ci/rollback_run.py` | Stdlib test of `deploy/rollback.sh` under fake homes with a recording `systemctl`: calls in order, the config change, nothing deleted, an idempotent second run, `ulei-hive-mcp.service` stopped when installed and skipped when `not-found`, refused runs and a failed step as one stderr line; job `architecture` runs it. |
| `ci/connectors_run.py` | Stdlib check of source connector runs in their transient units (ADR-0056 section 5.7) under the `systemd-units` job's user manager, in four groups, one CI step each. Each group folds its passed checks into two to four notices, one line per check with its measured value, so the `systemd-units` job stays under GitHub's 50 annotations per job with every check visible; a failed check is an error annotation of its own. In every group: the kernel, LSM list and Landlock ABI in the first notice, failing the step below ABI 3, and last, no broker socket left in `run_dir`; `sandbox`: items into a fake C10 with the Landlock ABI in the record, a scripted MCP connector's two pages into it, the MCP example wrapping its synthetic server putting the test connector's 100 records into it, a scripted MCP server lingering after its last page stopped after 3 s with the run `ok`, the test connector's socket probes refused, its Landlock file probes (`EACCES` outside its paths and grants, the fake archive unchanged, success inside), the injected `ENOSYS` and `EOPNOTSUPP` refused, the `landlock = "off"` opt-out; `outcomes`: a missing or unexecutable program `refused exec_failed`, a unit that cannot start `refused`, a non-zero exit `exit`, a timeout from `Result=timeout` with its cursor kept, `ulei-connectors exec` refused outside a unit; `broker`: a listed loopback fake source under an owner `allow` reached with its first line, an unlisted entry and a listed loopback name without `allow` `not_allowed`, the 33rd connection `limit`, and under `network = "direct"` its own `AF_INET` socket, `AF_UNIX` refused and connect ports limited from Landlock ABI 4; `schedule`: `ulei-connectors serve` for two periods of 60 s with each connector once per period and an overdue one once at start, `rate_limited` putting a run off across a restart, `sync` every period, 0 voluntary context switches between due times, a manual `run` during a scheduled one `busy`, and SIGTERM stopping the running unit with its record written. |
| `ci/connectors_e2e.py` | Stdlib end to end check of the source connectors with the real archive (ADR-0056 section 9) under the `systemd-units` job's user manager: the released test connector installed with `deploy/install.sh --connector test` and four copies of it, the released `ulei-memory serve` on a fresh archive pulling from the fake engine of `ci/memory_run.py`, and the installed `ulei-connectors.service`. It checks the unit's scheduled `sync` and the next one reaching the daemon with no sync failure line, the scheduled run `ok` with C10 `search` with `source` `test` finding as many items as it stored, a manual `run` after `reset` storing nothing with every item a `duplicate` and the item count unchanged, the four copies running at once with the unit's `memory.peak` and `pids.peak` within its `MemoryMax` and `TasksMax`, and a stop leaving no run unit and no broker socket; its passed checks fold into three notices. |
| `ci/units_run.py` | Stdlib start test of the user units (ADR-0042 section 9, ADR-0070): a synthetic `ulei.toml`, fake qb and Bot API, the owner turn's units in `ulei.slice` and the connector runner in `background.slice`, `ulei-memory.service` started alone without the engine and answering `health`, the units under a user manager, `ulei status`, their sandbox, limits and a restart after a fatal exit, the runner's unit running the test connector to `ok` into that real archive and a stop of it during a run leaving the run's record, `ulei-hive-mcp`'s unit answering `initialize` and `hive_list` with the fake hive CLI and `ulei status` reading it `up`, `ulei-garden`'s unit without the model answering `health` with `dense: off` and a `search`, `ulei-uncertain.service` started once on the inbox without uncertain events ending `success` with no Bot API call, `ulei-hive-watchdog.service` on a synthetic hive running the repository's `hive.py` ending `success` with no alert while healthy and with one due alert after a vendor cooldown, calling no `ulei-tg`; in mode `sandbox` a turn of `ulei-engine.service` ending `completed` before and after the fake qb at `<runtime>/qb-gateway/gateway.sock` is stopped, its directory and socket deleted and made again, with the same MainPID and NRestarts 0; its passed checks fold into four notices per mode. Mode `system` installs with `sudo` the engine's system unit and its gateway proxy, filled for the runner's account, and runs the same turns there in one notice (ADR-0069 section 4). |
| `ci/io_classes_run.py` | Stdlib measurement of the IO classes (ADR-0103) in job `systemd-units` after it delegates io to the runner's user manager: the class slices of `deploy/install.sh` for a synthetic `[host.io]` of the runner's disk, probe reads of the urgent and background classes under a random 4 KiB O_DIRECT read load with the disk's scheduler in a notice, and the read rate of `hive-heads.slice`, by `io.stat` and by the load's own count, against its cap, the one check that fails. |
| `ci/deliver_kill.py` | Stdlib delivery kill test of the owner inbox (ADR-0037 section 6): SIGKILLs `ulei-inbox serve` with `inbox.deliver` and `ulei-engine serve` at the kill points K1 to K9 and at random, then reconciles every event's C1 row with its turns in the journal. |
| `ci/gitleaks_scan.py` | Stdlib driver of the `gitleaks` CI job: the scan and its self-test in a throwaway repository. |
| `ci/encoder_model.py`, `ci/encoder_golden.py`, `ci/encoder_vectors.py`, `ci/encoder-ref/`, `ci/requirements-encoder-ref.txt` | The encoder engine's real models in CI (ADR-0087 section 10): e5-small (and bge-m3, manual) from Hugging Face at pinned revisions, converted by `ulei-garden-embed convert` to the pinned content digest; the PyTorch fp32 golden dumps and tokenizer parity cases generated in a venv with the pinned torch and transformers; the helper's vectors compared byte for byte; the small committed reference files. Workflow `encoder.yml`. |
| `ci/garden_e2e.py`, `ci/garden_e2e_lib.py` | Stdlib end-to-end scenarios of the garden with the encoder engine on the static musl binaries (indexing under queries, helper and daemon kills, corrupted, truncated, replaced and unusable weight files, upgrade from an ONNX-backend store, disk full, a cgroup memory limit), every stored vector checked byte for byte against an independent helper; job `e2e` of `assurance.yml`, and the rented 1 GB machine runs. |
| `ci/mutants_summary.py`, `ci/miri_tests.py`, `ci/lsan-suppressions.txt`, `fuzz/` | The `assurance` workflow's mutation summary and survivor pass, Miri one test per process, LeakSanitizer's suppression of the deliberate leak, and the cargo-fuzz targets (a crate outside the workspace, ADR-0090). |

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
cargo build --release --locked -p ulei-hive-mcp --bin ulei-hive-mcp --target x86_64-unknown-linux-musl
cargo build --release --locked -p ulei-garden-embed --features onnx \
  --target x86_64-unknown-linux-gnu
cargo build --release --locked -p ulei-priv --target x86_64-unknown-linux-musl
cargo build --release --locked -p ulei-garden-embed --features native \
  --target x86_64-unknown-linux-musl
cargo build --release --locked -p ulei-engine --features web-loopback-http --bin ulei-web \
  --target x86_64-unknown-linux-musl --target-dir target/web-loopback && ci/budgets.sh
```

CI also runs `cargo deny check`, the MSRV check (1.89, without `xtask`), the ADR gate, schema
compatibility against the base revision, the engine's contract test against qb built at
the pin (job `qb-contract`, ADR-0038 section 6), the reproducible release with the install
script test (job `release`, ADR-0042 section 10), the garden's retrieval eval on the synthetic
gold set (job `garden-eval`, ADR-0045 section 5), its ONNX path on a tiny model written at
run time (job `garden-onnx`, ADR-0047 section 8) and its encoder engine path on a tiny checkpoint
converted at run time (job `garden-encoder`, ADR-0087 E6), the workspace on aarch64 (job
`aarch64`: clippy, `ulei-infer`'s tests with NEON, its MSRV check), the engine on the real
e5-small model on x86_64, Linux aarch64 and Apple M1 (workflow `encoder.yml`, path-filtered),
and, on demand and weekly, mutation testing, fuzzing, ASan, TSan, Miri and the garden's
end-to-end scenarios (workflow `assurance.yml`, nightly Rust only there; ADR-0087 section 10),
and the night's judge on a real Claude model (workflow `judge-eval.yml`, informational).
Job `systemd-units` also measures the IO classes with io delegated (`ci/io_classes_run.py`, ADR-0103).
Gitleaks scans the history of the commit under check, not the other branches.
Release builds use fat LTO: one cargo build at a time on shared hosts, under `nice`.
The musl build compiles bundled SQLite and needs `x86_64-linux-musl-gcc` (package `musl-tools`).

## Hard rules

1. **A module talks to another module only through `ulei-contracts` and the wire.** Never add a
   dependency between module crates. A module may also link a library crate that lists it in its
   `users` (ADR-0072); a library depends on no workspace crate. `cargo xtask arch` fails on
   anything else.
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
