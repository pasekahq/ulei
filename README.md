# ulei

[![CI](https://github.com/pasekahq/ulei/actions/workflows/ci.yml/badge.svg)](https://github.com/pasekahq/ulei/actions/workflows/ci.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/pasekahq/ulei/badge)](https://scorecard.dev/viewer/?uri=github.com/pasekahq/ulei)
[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPL--3.0--only-blue)](LICENSE)

`ulei` is the home of a personal agent ecosystem: the pieces around an AI assistant that
should outlive any particular agent runtime. It is a Rust cargo workspace with strict module
boundaries, language-neutral contracts and resource budgets enforced in CI.

**Status:** pre-alpha, building the MVP (see [roadmap](docs/roadmap.md)). The agent loop
(`ulei-engine`, against qb's model gateway), the Telegram channel (`ulei-channels`) and the owner
inbox (`ulei-core`) are being built in sprints U1–U4; `ulei-memory` serves the archive store as a
daemon; `ulei-hive` and `ulei-glue` are still empty shells. Code moves in step by step from the existing Python system. No release
yet; not ready for use.

## Modules

| Crate | Purpose |
|---|---|
| `ulei-contracts` | Rust types generated from `contracts/` |
| `ulei-core` | owner inbox and events, module registry, config, notifications, MCP tools, `ulei` CLI, `ulei-connectors` runner of the source connectors |
| `ulei-hive` | task cards executed by agent "heads", acceptance, delivery |
| `ulei-garden` | garden of typed facts about the owner's life and their retrieval ([README](crates/garden/README.md)) |
| `ulei-memory` | archive of raw items of every source (C10), later the forest made from it ([README](crates/memory/README.md)) |
| `ulei-runtime-adapters` | bridges to agent runtimes |
| `ulei-channels` | owner channels (Telegram first) |
| `ulei-glue` | bridges to standalone products (quota balancer, A2A gate) |
| `ulei-engine` | own agent loop: tools, sessions; models only through the qb gateway |
| `ulei-connector-test` | source connector over a synthetic source, the test subject of the connector runner (C11) |

Modules never depend on each other; they share only the generated contract types and talk
over the wire. Standalone products (for example [qb](https://github.com/pasekahq/quota-balancer))
live in their own repositories and are pinned in [`ecosystem.lock`](ecosystem.lock).
The current module graph: [docs/architecture/module-map.md](docs/architecture/module-map.md).
Decisions: [docs/architecture/adr/](docs/architecture/adr/).

## Contracts

| Id | Contract |
|---|---|
| C1 | event of the owner inbox |
| C2 | MCP tools of the core |
| C3 | card → head → report |
| C4 | qb API (vendored) |
| C4G | qb model gateway (vendored) |
| C5 | A2A gate, owner side |
| C6 | runtime adapter protocol |
| C7 | garden search and proposals |

They are JSON Schema 2020-12 / OpenAPI 3.1 files in [`contracts/`](contracts/), with golden
fixtures and a changelog, so components in any language (including the existing Python
ones) can use them.

## Requirements

- Rust stable with `rustfmt` and `clippy` (MSRV 1.89), target `x86_64-unknown-linux-musl` for
  release binaries.
- Python 3.9+ only to run `ci/broad_check.py` (standard library).

## Quick start

```sh
cargo build --workspace
cargo test --workspace
python3 -m ci.broad_check     # fmt, clippy, tests, architecture and contract checks
cargo run -p ulei-core --bin ulei -- contracts
```

Release build and budgets: `ci/budgets.sh` measures the static musl binaries `ulei`,
`ulei-inbox`, `ulei-engine`, `ulei-web` (and its `web-loopback-http` test build), `ulei-tg` and
`ulei-garden`, and the glibc helper `ulei-garden-embed`. The builds and the call are the last
lines of [AGENTS.md, Commands](AGENTS.md#commands-all-must-pass-before-you-push).

## `ulei status`

```sh
ulei status [--config PATH]
```

It reads `inbox.socket`, `engine.c6_socket`, `engine.owner_session` and `qb.socket` from
`ulei.toml` (the file from `--config`, else `ULEI_CONFIG`, else `$XDG_CONFIG_HOME/ulei/ulei.toml`,
else `~/.config/ulei/ulei.toml`), probes the inbox, the engine and the qb gateway at once over
their sockets and prints one line per item within 10 s. It opens no database and writes
nothing. The engine's `ULEI_*` variables are not read, so a unit that sets them can serve other
sockets than the file names.

```text
inbox: up /run/ulei/inbox.sock (ulei-inbox 0.0.1)
queue: pending 0, leased 0, processing 0, deferred 0, uncertain 0
engine: up /run/ulei/engine.sock
channel: up
session: owner-session idle since 2026-01-01T09:00:00Z, queue 0, route main
last turn: turn-1 completed at 2026-01-01T09:00:00Z
qb: up /run/qb/gateway.sock (1.0.0)
route main: ready vendor-a/model-a
```

A daemon that is down is its line with the reason, such as `inbox: down <socket>: connect:
Connection refused` or `engine: down <socket>: no answer within 5 s`; the other daemons are still
reported. Every line, its down forms and reasons are in ADR-0040 section 5. Exit 0 when `inbox`,
`engine`, `channel` and `qb` read `up`; 1 when one of them reads `down` or `unknown`; 2 for a
usage error or a config without one of the four keys, with nothing probed.

## `ulei-connectors`

```sh
ulei-connectors list [--config PATH]
ulei-connectors run NAME [--config PATH]
ulei-connectors reset NAME [--config PATH]
ulei-connectors exec NAME DATA_DIR DIR [--key FILE] [--read PATH]... [--write PATH]...
                     [--landlock-off] -- PROGRAM [ARG...]
```

The runner of the source connectors (ADR-0056, contract C11). It reads `connectors.dir`,
`connectors.enabled`, `connectors.state`, `connectors.log`, `memory.socket` and the owner's
`[connectors.<name>]` from `ulei.toml`. `list` prints `name transport every Ns` for each enabled
name whose directory holds a valid `connector.json`, and one stderr line for each enabled name
that does not (exit 1 then). `run` starts one run of a stdio connector now as a transient service
of the user manager, `systemd-run --user --wait --pipe --collect
--unit=ulei-connector-<name>-<unix time>`, with the seccomp and limit properties of ADR-0056
section 5.1 (no socket of any family, `MemoryMax=256M`, `TasksMax=64`, `RuntimeMaxSec=` the
manifest's `timeout`); the unit's stdio are the runner's pipes. It hands the connector its C11
`Request`, puts every item into the archive as C10 `put` with `source` = the connector's name,
stores the cursor in `<connectors.state>/<name>/runner.json` once every item before it was
answered, appends one C11 `RunRecord` line naming the unit and the Landlock ABI to
`connectors.log` (mode 0600) and prints one stderr line ending in `landlock abi <n>` or
`landlock off`. A run whose unit `systemd-run` reports with result `timeout` ends `timeout` with
the cursors it stored kept; a `systemd-run` that cannot start the unit ends the run `refused`
with code `unit_start_failed`; a run the runner ends early (`protocol`, `archive`) has its unit
stopped through `systemctl --user stop`. The owner's `read`, `write` and `landlock = "off"` of
`[connectors.<name>]` reach the launcher in its argv; `read` and `write` must be absolute. Exit 0 for outcome `ok`, 1 for any other, 2 for a usage error, a config
without its keys or a connector that is not enabled or has no valid manifest. `reset` clears the
stored cursor, so the next run starts from the beginning of the source.

`exec` is the launcher the unit runs, not a command for the owner. It refuses unless
`/proc/self/status` shows `NoNewPrivs: 1` and `Seccomp: 2` and `/proc/self/cgroup` ends in the
unit `ulei-connector-<NAME>-<unix time>.service`. Unless `--landlock-off`, it then restricts
itself with Landlock (ADR-0056 section 5.2): read and execute on `/usr`, `/etc`, `/lib`, `/lib64`,
`/bin`, `/sbin` and `DIR`, read on `/proc`, the random and zero devices and `--read` paths, read
and write on `/dev/null`, read on the `--key` file alone, and every right but execute on
`DATA_DIR` and the `--write` paths; it needs ABI 3 or later and sets the signal and abstract
socket scopes from ABI 6. A kernel without Landlock or below ABI 3 refuses with code `landlock`
and a reason naming `ENOSYS`, `EOPNOTSUPP` or the ABI seen. It then writes `ulei-connectors: exec
landlock abi <n>` or `ulei-connectors: exec landlock off` on stderr, clears the environment, sets
`HOME` and `TMPDIR` to `DATA_DIR`, `PATH=/usr/bin:/bin` and `LANG=C.UTF-8`, keeps `LISTEN_FDS`,
`LISTEN_FDNAMES` and `LISTEN_PID`, and executes the connector in `DATA_DIR`. A refusal is one
stderr line `ulei-connectors: exec refused <code>: <reason>` and exit 125, which the run records
as outcome `refused` with that code. A program that is missing or outside the execute paths fails
`execve` after the Landlock line; that refusal has code `exec_failed`, and the run takes it only
as the line right after the Landlock line.

## Release and install

The `release` job of every CI run builds the static `x86_64-unknown-linux-musl` binaries `ulei`,
`ulei-inbox`, `ulei-engine`, `ulei-web` and `ulei-tg` twice, the second time from a clean target
directory at another path, and fails unless both builds give the same `SHA256SUMS` (ADR-0042
section 10). Where to find a release of commit `<commit>`:

- the artifact `ulei-x86_64-unknown-linux-musl` on the run's summary page (Actions, the `CI` run
  of `<commit>`, Artifacts; the download needs a GitHub login): the five binaries and
  `SHA256SUMS`;
- the `SHA256SUMS` notice of the `release` job, readable without a login, beside the `toolchain`
  notice with the `rustc` and `cargo` versions the build used.

`ci/release.sh <out-dir>` in a checkout of `<commit>`, with that toolchain, its musl target and
`musl-tools`, builds the same binaries and `SHA256SUMS`.

Verify and install, as the account that owns the deployment, from a checkout of `<commit>`:

```sh
unzip ulei-x86_64-unknown-linux-musl.zip -d <release-dir>
cd <release-dir> && sha256sum -c SHA256SUMS && cd -   # compare with the run's notice too
<checkout>/deploy/install.sh <release-dir>
```

The script needs no root: it checks every binary against `SHA256SUMS` before it copies anything,
puts the binaries into `$HOME/.local/bin` (mode 0755) and the units of `deploy/systemd` into
`$XDG_CONFIG_HOME/systemd/user` (default `$HOME/.config/systemd/user`, mode 0644), and leaves a
file with the same bytes as it is, so a second run changes nothing. It never writes `ulei.toml`
or a secret and never calls `systemctl`; it prints the `systemctl --user` commands to run next.
On a failure it exits 1 with one line `install.sh: <step>: <reason>`.

## systemd user units

[`deploy/systemd/`](deploy/systemd) holds the user units `ulei-inbox.service`,
`ulei-engine.service` and `ulei-tg.service` (ADR-0042). They run as the account that owns the
deployment, start `ulei-engine` before the inbox and the channel, restart a daemon that exits
non-zero, and allow writes only to the directories of ADR-0042 section 3:

| Directory | Holds |
|---|---|
| `<state>/ulei/inbox` | `inbox.root` |
| `<state>/ulei/engine` | `engine.journal`, `engine.terminal_output`, `engine.web_store`, `engine.no_ask_guard.log`, `engine.memory_file`, `engine.user_file`, `engine.skills_dir` |
| `<state>/ulei/telegram` | `telegram.attachments_dir` |
| `<runtime>/ulei` | `inbox.socket`, `engine.c6_socket` |

`<state>` is `$XDG_STATE_HOME` (default `$HOME/.local/state`), `<runtime>` is `$XDG_RUNTIME_DIR`,
and each unit reads `$XDG_CONFIG_HOME/ulei/ulei.toml` (default `$HOME/.config/ulei/ulei.toml`)
through `ULEI_CONFIG`. `deploy/install.sh` (see [Release and install](#release-and-install))
puts the binaries into `$HOME/.local/bin` and the units into `$XDG_CONFIG_HOME/systemd/user/`
(default `$HOME/.config/systemd/user/`); then, as that account:

```sh
systemctl --user daemon-reload
systemctl --user enable --now ulei-engine.service ulei-inbox.service ulei-tg.service
~/.local/bin/ulei status
```

A written path outside the table needs its directory in a drop-in
(`systemctl --user edit <unit>`, `ReadWritePaths=<dir>`). The units' mount options need a host
that lets the user manager create unprivileged user namespaces.

## Budgets

| | Limit |
|---|---|
| `ulei` CLI binary (static) | ≤ 512 KiB |
| `ulei --version` peak RSS / cold start / syscalls | ≤ 1 MiB / ≤ 3 ms / ≤ 40, no file opens |
| `ulei-inbox` daemon (static) | ≤ 2.5 MiB binary, ≤ 4 MiB peak RSS, ready ≤ 10 ms, 0 wake-ups when idle, publish p99 ≤ 50 ms, ≤ 128 KiB written per event |
| `ulei status` run | 1 file opened (`ulei.toml`), ≤ 4 MiB peak RSS |
| `ulei-engine` (static) | ≤ 4 MiB binary, ≤ 4 MiB peak RSS and 0 wake-ups while `run` waits for the gateway |
| each future daemon | ≤ 2.5 MiB binary, ≤ 4 MiB RSS, 0 wake-ups when idle |

## Tooling

`cargo xtask <command>`:

- `arch` — crate-graph boundaries and source rules;
- `gen-contracts [--check]` — generate Rust types from `contracts/`;
- `contracts [--base REV]` — validate schemas and fixtures, compatibility against `REV`;
- `module-map [--check]` — regenerate the module map;
- `adr [--base REV]` — ADR format and "architecture changes need an ADR";
- `check` — all of the above in check mode.

## License

Copyright (C) 2026 the ulei contributors.

Licensed under the [GNU Affero General Public License v3.0 only](LICENSE) (SPDX: `AGPL-3.0-only`).
If you run a modified `ulei` for users over a network, section 13 asks you to offer them its source.
Third-party notices are in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

Contributions are accepted under the same license with a DCO sign-off; see [CONTRIBUTING.md](CONTRIBUTING.md).
