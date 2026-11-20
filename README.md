# ulei

[![CI](https://github.com/pasekahq/ulei/actions/workflows/ci.yml/badge.svg)](https://github.com/pasekahq/ulei/actions/workflows/ci.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/pasekahq/ulei/badge)](https://scorecard.dev/viewer/?uri=github.com/pasekahq/ulei)
[![License: AGPL-3.0-only](https://img.shields.io/badge/license-AGPL--3.0--only-blue)](LICENSE)

`ulei` is the home of a personal agent ecosystem: the pieces around an AI assistant that
should outlive any particular agent runtime. It is a Rust cargo workspace with strict module
boundaries, language-neutral contracts and resource budgets enforced in CI.

**Status:** skeleton (step S0). The crates are empty shells; the contracts, the boundary
checks, the generated architecture map and the CI gates are in place. Code moves in step by
step from the existing Python system (see [roadmap](docs/roadmap.md)).

## Modules

| Crate | Purpose |
|---|---|
| `ulei-contracts` | Rust types generated from `contracts/` |
| `ulei-core` | owner inbox and events, module registry, config, notifications, MCP tools, `ulei` CLI |
| `ulei-hive` | task cards executed by agent "heads", acceptance, delivery |
| `ulei-memory` | garden of facts, retrieval, archive of past sessions |
| `ulei-runtime-adapters` | bridges to agent runtimes |
| `ulei-channels` | owner channels (Telegram first) |
| `ulei-glue` | bridges to standalone products (quota balancer, A2A gate) |
| `ulei-engine` | own agent loop: tools, sessions; models only through the qb gateway |

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

Release build and budgets:

```sh
cargo build --release --locked -p ulei-core --target x86_64-unknown-linux-musl
cargo build --release --locked -p ulei-engine --target x86_64-unknown-linux-musl
ci/budgets.sh target/x86_64-unknown-linux-musl/release/ulei \
  target/x86_64-unknown-linux-musl/release/ulei-inbox \
  target/x86_64-unknown-linux-musl/release/ulei-engine
```

## Budgets

| | Limit |
|---|---|
| `ulei` CLI binary (static) | ≤ 512 KiB |
| `ulei --version` peak RSS / cold start / syscalls | ≤ 1 MiB / ≤ 3 ms / ≤ 40, no file opens |
| `ulei-inbox` daemon (static) | ≤ 2.5 MiB binary, ≤ 4 MiB peak RSS, ready ≤ 10 ms, 0 wake-ups when idle, publish p99 ≤ 50 ms, ≤ 128 KiB written per event |
| `ulei-engine` (static) | ≤ 2.5 MiB binary, ≤ 4 MiB peak RSS and 0 wake-ups while `run` waits for the gateway |
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
