# ADR-0042: systemd user units of `ulei-inbox`, `ulei-engine` and `ulei-tg`

Status: accepted
Date: 2026-11-20
Affects: deploy/systemd, ci/units_run.py, .github/workflows/ci.yml

## Context

The three daemons run under a supervisor that owns their processes: no daemon writes a PID file
or takes a runtime lock, and each exits non-zero for its supervisor on a fatal error (ADR-0040
section 7, port map R16). The supervisor is the owner's systemd user manager. Each daemon finds
`ulei.toml` by the path order of ADR-0040 section 2 and binds its socket at start (ADR-0007,
ADR-0028, ADR-0036 section 1); `ulei-tg` serves no socket (ADR-0032). The resource budgets are
ADR-0001's rows as `ci/budgets.sh` enforces them. The model for the files and the CI job is the
deploy/systemd units of qb at the pin of `ecosystem.lock` and its `systemd-units` job. There is
no reference implementation for units.

## Decision

### 1. Place

`deploy/systemd/ulei-inbox.service`, `ulei-engine.service` and `ulei-tg.service`: user units for
the account that owns the deployment, installed by hand (`systemctl --user link` or a copy into
the user's unit directory) and enabled with `WantedBy=default.target`. Each runs its binary from
`%h/.local/bin`: `ulei-inbox serve`, `ulei-engine serve`, `ulei-tg`. The units need systemd 255 or
later, the version CI runs. Nothing is installed on a host by this record.

### 2. Config

Each unit sets `Environment=ULEI_CONFIG=%E/ulei/ulei.toml`: the file the binaries' own order finds
under `$XDG_CONFIG_HOME` or `$HOME/.config`, pinned to the user manager's `%E` so a shell
variable of the manager cannot point a unit at another file. No unit sets an engine `ULEI_*`
setting variable, so the file alone configures every daemon. `ulei.toml` names
`engine.service_unit = "ulei-engine.service"`, the unit the engine's lifecycle guard keeps the
agent from stopping (port map R10).

### 3. Writable directories

Every unit is read-only but for its own directories: `ProtectSystem=strict`, `ProtectHome=read-only`
and `PrivateTmp=yes`, with `ReadWritePaths=` naming the directories that hold the paths
`ulei.toml` gives the daemon to write, and nothing else. A deployment keeps those paths in this
layout:

| Unit | Keys it writes | ReadWritePaths entry | Created by |
|---|---|---|---|
| `ulei-inbox` | `inbox.root` (`events.sqlite3`, `producer.key`, `consumer.lock`) | `%S/ulei/inbox` | `StateDirectory=ulei/inbox` |
| | `inbox.socket` | `%t/ulei` | `RuntimeDirectory=ulei` |
| `ulei-engine` | `engine.journal`, `engine.terminal_output`, `engine.web_store`, `engine.no_ask_guard.log`, and `engine.memory_file`, `engine.user_file`, `engine.skills_dir`, which the agent edits with `write_file` and `patch` | `%S/ulei/engine` | `StateDirectory=ulei/engine` |
| | `engine.c6_socket` | `%t/ulei` | `RuntimeDirectory=ulei` |
| `ulei-tg` | `telegram.attachments_dir` | `%S/ulei/telegram` | `StateDirectory=ulei/telegram` |

- The rule CI checks: every path of a key in the table lies under one of its unit's
  ReadWritePaths entries, and every entry holds one of them.
- The state directories and the runtime directory are created with mode 0700. `ulei-inbox` and
  `ulei-engine` share `%t/ulei` with `RuntimeDirectoryPreserve=yes`, so stopping one keeps the
  other's socket; the user manager removes it at its own exit.
- Every other path `ulei.toml` names is read: the token, env-files, brain files, the web helper,
  MCP commands. The engine's tools and child processes (`terminal`, `write_file`, `patch`, MCP
  servers, `ulei-web`) live in the same sandbox and write only these directories and `/tmp`.
- A deployment that must keep a written path elsewhere names its directory in a drop-in
  (`systemctl --user edit`) beside the unit's list.

### 4. Order and dependencies

| Unit | Wants | After | Why |
|---|---|---|---|
| `ulei-engine` | none | none | It reaches qb per turn (ADR-0038) and needs no other daemon to start. |
| `ulei-inbox` | none | `ulei-engine` | It serves C1 publishers whether the engine runs or not; with `inbox.deliver` its loop calls the engine. |
| `ulei-tg` | `ulei-engine` | `ulei-engine` | The channel only relays to the engine, so starting it starts the engine. |

- No `Requires=` or `BindsTo=`: a stop or restart of the engine would then stop the channel and
  drop updates it accepted but has not handed on. Each client outlives a missing peer: the inbox
  delivery loop treats a failed `is_idle` as busy and claims nothing (ADR-0036 section 2),
  `ulei-tg` retries `inbox_turns` every 2 s, a turn sent while the engine is down gets the engine
  failure reply in the chat (ADR-0033), and `ulei status` reports a down daemon as a line.
- Socket readiness: `Type=exec`. Each daemon binds its socket before its ready line; the binaries
  speak no readiness protocol, so `After=` orders the starts and no client relies on a peer's
  socket at its start. A failed bind or config exits 1 within milliseconds of the start.
- `network-online.target` is a system unit a user manager cannot order against; a `getMe` that
  fails at boot exits 1 and the restart of section 6 retries it.

### 5. MemoryMax and TasksMax

| Unit | MemoryMax | Budget and margin | TasksMax | Count and margin |
|---|---|---|---|---|
| `ulei-inbox` | 16M | 4 MiB peak RSS (ADR-0007, ADR-0036); four times it, the rest for the SQLite page cache and socket buffers the cgroup charges, which it reclaims before an OOM kill | 4 | the main thread and `deliver`, doubled |
| `ulei-engine` | 512M | 128 MiB for its own budget and helper: 4 MiB idle RSS (ADR-0010), one `ulei-web` at its 80 MiB budget (ADR-0015) and three copies of the 16 MiB reply it reads from the helper; 384 MiB for terminal commands and MCP servers, which have no budget | 256 | about 40 of its own (main, held C6 calls, the turn, 8 tool workers, 2 threads per terminal call) and the processes of terminal commands and MCP servers, doubled and rounded up |
| `ulei-tg` | 32M | 4 MiB idle RSS (ADR-0017); eight times it: attachments stream to disk and speech to text reads its file in parts, and the page cache of a 20 MiB download is charged and reclaimable | 16 | main, `signals`, `poller`, `outbound`, `typing`, `engine`, `inbox` and one `follow` per owner chat, doubled |

A deployment with heavier MCP servers raises the engine's limits in a drop-in. An OOM kill at
MemoryMax is a failure and restarts the unit (section 6).

### 6. Restart

- `Restart=on-failure`: a non-zero exit, a kill by a signal other than SIGHUP, SIGINT, SIGTERM or
  SIGPIPE (an OOM kill, the SIGABRT of a panic under `panic = "abort"`) or a start timeout
  restarts the unit; a clean stop does not.
- `RestartSec=2s` for `ulei-inbox` and `ulei-engine`, whose clients wait for them; `5s` for
  `ulei-tg`, so a failing token or network does not hammer the Bot API. `RestartSteps=5` and
  `RestartMaxDelaySec=60s` grow the delay to one minute over five restarts.
- `StartLimitIntervalSec=0`: no start rate limit, so a daemon that fails on a transient cause
  (the network at boot, a full disk) comes back by itself; the growing delay bounds the loop.
- `RestartPreventExitStatus=2`: a usage error is a wrong `ExecStart`, which no restart fixes.
- Stop: SIGTERM. The default 90 s stop timeout covers `ulei-tg`'s longest clean stop, the long
  poll plus its read timeout (30 s at the defaults, ADR-0032 section 3).

### 7. Fatal exit per binary

| Binary | Fatal: exit 1 with one stderr line, restarted | Not restarted |
|---|---|---|
| `ulei-inbox serve` | config refused, store open, delivery consumer open, socket bind, ready line write, `accept` failure | SIGTERM (no handler, the default action); exit 2, usage |
| `ulei-engine serve` | config refused, journal open, socket bind, ready line write, `accept` failure | SIGTERM (no handler in `serve`); exit 2, usage |
| `ulei-tg` | startup failure (settings, attachments directory, `getMe`, the first `deleteWebhook`) and a fatal polling error: an invalid token, or a conflict or network failure that outlived its retries, after flushing the ingress (ADR-0032 section 3) | exit 0 after SIGTERM or SIGINT; exit 2, usage |

### 8. Hardening

Every unit carries the options of the deployment goal: `NoNewPrivileges=yes`,
`ProtectSystem=strict`, `ProtectHome=read-only` with the ReadWritePaths of section 3,
`PrivateTmp=yes`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`,
`SystemCallFilter=@system-service`, MemoryMax and TasksMax of section 5 and `Restart=on-failure`.
On top, as qb's user units: `SystemCallErrorNumber=EPERM` (a denied call fails instead of killing
the process), `SystemCallArchitectures=native`, `RestrictNamespaces=yes`, `LockPersonality=yes`,
`RestrictRealtime=yes`, `RestrictSUIDSGID=yes`, `UMask=0077`, and `MemoryDenyWriteExecute=yes` on
`ulei-inbox` and `ulei-tg`. The engine has no `MemoryDenyWriteExecute`: MCP servers and terminal
commands may run a JIT.

`ProtectSystem`, `ProtectHome`, `PrivateTmp` and `ReadWritePaths` are mount namespace options;
in a user unit the manager sets them up inside an unprivileged user namespace, so the host must
allow the user manager to create one with capabilities inside it. CI checks that each unit's
process runs in its own mount namespace with these mounts, so a host that cannot apply them fails
the check instead of passing without them.

### 9. CI

Job `systemd-units`, parallel to the others:

- `systemd-analyze security --offline=yes` per unit within its threshold, 5.7 each;
- the release musl builds of `ulei-core`, `ulei-engine` and `ulei-tg` installed into
  `$HOME/.local/bin`, then `systemd-analyze --user verify` of the three units, failing on any line
  that names one of them;
- the start test `ci/units_run.py` under the runner user's manager (lingering enabled): a
  synthetic `ulei.toml` in the layout of section 3, a fake qb gateway and a loopback fake Bot API;
  each unit running, `ulei status` exit 0, the rule of section 3 against the unit's expanded
  ReadWritePaths, each process with `NoNewPrivs` and a seccomp filter, its own mount namespace
  with `/` and the home read-only, its ReadWritePaths writable and a private `/tmp`, and its
  cgroup's `memory.max` and `pids.max` equal to the unit's limits; a token revoked once makes
  `ulei-tg` exit 1, systemd restarts it and `ulei status` exits 0 again; a stop leaves every unit
  inactive with result `success`.

Every check runs even after an earlier one failed, and writes its result as a `::notice` or
`::error` annotation with the measured value or the reason (at most ten per step), which the
public check-runs API serves without the job log; the job fails if any check failed.

## Consequences

- A deployment's `ulei.toml` keeps every written path in the layout of section 3, or extends the
  unit's ReadWritePaths in a drop-in.
- The engine's agent cannot write the home or the system through any tool while it runs under
  the unit.
- A host that restricts unprivileged user namespaces for the user manager cannot run the units as
  written.
- The release artifact and the install script that place the binaries in `%h/.local/bin` are
  separate work.
