# ADR-0010: `ulei-engine` crate, its dependencies and budgets

Status: accepted
Date: 2026-11-17
Affects: crates/engine, dep:ulei-contracts
Amended by: ADR-0053 (the static size limit is 4,194,304 B by the owner's decision).

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
  0; `ulei-engine run` runs one turn of a journaled session; `ulei-engine serve` serves C6 on
  a Unix socket and runs such turns for its callers (ADR-0028); settings, exit codes and checks
  of both are in `docs/runbooks/engine-live-smoke.md`. Every other invocation prints a one-line
  usage to stderr and exits 2.

### 2. Dependencies

Each dependency below is admitted by the record it names; a new one comes with its own ADR.

| Crate | Features | Record | Why |
|---|---|---|---|
| `ulei-contracts` (workspace) | none | this record | Generated wire types of the contracts the engine speaks. |
| `serde_json` | `std`, `float_roundtrip` | ADR-0012 | JSON of the gateway stream, the tools and the C6 bodies. |
| `rustix` | `std`, `net`, `process`, `fs` | ADR-0012, ADR-0028 | Non-blocking Unix `connect`, process groups of the tools, `O_NONBLOCK` opens, the C6 peer uid. |
| `regex-lite` | `std`, `string` | ADR-0013 | Patterns of the search tools, the redaction, the guards and the compaction text. |
| `rusqlite` | `bundled` (C SQLite) | ADR-0014 | The C8 session journal. |
| `sha2` | none | ADR-0024 | Hashes of the tool guardrails, the state-db guard and the web store. |
| `signal-hook` | none | ADR-0029 | SIGINT, SIGTERM and SIGHUP interrupting the turn of `run`. |
| `ureq`, `ring` | optional, feature `web` | ADR-0015, ADR-0021 | TLS of the `ulei-web` helper only; `ulei-engine` never links them. |

`ulei-engine` links one piece of C code, the bundled SQLite. `ring`, linked only into
`ulei-web` under the feature `web`, carries C and assembly of its own.

### 3. Measured size

The size that counts is that of the static release binary for `x86_64-unknown-linux-musl`,
built with the release profile of ADR-0001 and the `opt-level` of ADR-0027:

```sh
cargo build --release --locked -p ulei-engine --target x86_64-unknown-linux-musl
stat -c %s target/x86_64-unknown-linux-musl/release/ulei-engine
```

`ci/budgets.sh` measures it on every CI run and reports it, with the RSS and wake-up figures,
in the `ulei-engine budgets` annotation of the run.

### 4. Budgets

`ulei-engine` takes the "every daemon" row of ADR-0001 except its static size (ADR-0053).
`ci/budgets.sh` holds the release binary built for `x86_64-unknown-linux-musl` to:

| Measure | Limit | Environment variable |
|---|---|---|
| Static size | ≤ 4,194,304 B (4 MiB), statically linked (ADR-0053) | `ULEI_ENGINE_MAX_BINARY_BYTES` |
| Idle RSS | ≤ 4,096 kB (4 MiB) | `ULEI_ENGINE_MAX_RSS_KB` |
| Idle wake-ups | 0 voluntary context switches over 10 s idle | `ULEI_ENGINE_MAX_IDLE_WAKEUPS` |

Idle is each waiting state, and each is held to the RSS and wake-up limits:

- `run` idle: the process has sent its turn request and is blocked reading the gateway socket
  before any answer. The idle timeout of `run` (600 s) keeps it there well past the measuring
  window.
- `serve` idle: the process listens on its C6 socket with no turn and one held `inbox_turns`
  (ADR-0036 section 9), blocked in `accept` and on the condition variable of the hold with no
  timer.

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
  Then it starts `ulei-engine serve` with the same config file and a C6 socket in an owner-only
  directory of the work directory, and after its ready line sends one `inbox_turns` for the
  owner session and keeps the connection open. After a settling second it measures `serve`
  the same way, its wake-ups summed over `/proc/<pid>/task/*/status`. The run fails when
  `serve` exits, prints no ready line within 10 s, answers the held request, or starts or ends
  a thread while measured.

## Consequences

- Later engine work adds code to this crate, not a new crate; each new dependency still needs
  its own ADR.
- The `ulei-engine budgets` annotation of the latest CI run gives the exact size against the
  4 MiB static budget.
