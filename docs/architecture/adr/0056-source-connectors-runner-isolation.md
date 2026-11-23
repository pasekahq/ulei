# ADR-0056: Source connectors: the contract C11 0.1.0, the scheduled runner `ulei-connectors`, Linux isolation of a run, release, the test connector and the MCP wrapper example, and their cards

Status: accepted
Date: 2026-11-22
Affects: crates/core, crates/contracts, crates/connector-test, crates/connector-mcp-example, contract:C11, contract:C9, dep:landlock, dep:ulei-contracts, dep:serde, dep:serde_json, dep:rustix, deploy/systemd, deploy/install.sh, ci/release.sh, ci/install_run.py, ci/budgets.sh, .github/workflows/ci.yml

## Context

The source connectors are sprint item d9 (issue:0276511448bf4134a435). The issue carries three
decisions of the owner of 2026-11-21, quoted by their time:

- 11:24 to 12:00: source connectors are plugin modules of ulei. The core holds only the contract
  "put raw material into the archive" and the schedule. Each connector is a separate crate and
  binary in the workspace, or any program or MCP server, talking MCP or stdin/stdout; in the
  release they are separate files, and a server gets only the connected ones. When none fits, the
  agent writes a connector to the contract or wraps a ready MCP server.
- 12:01: the owner's own connectors live on the owner's server in a connectors directory under a
  local git; GitHub is not needed.
- 12:36: no separate sandbox product: when ulei starts a connector it applies the standard Linux
  isolation (Landlock, seccomp, systemd network limits). A connector sees only its own directory
  in the archive, its own key from the secret store and the network address of its source; wider
  access only by the owner's permission. This covers scheduled runs only; the agent's one-off
  scripts stay under the ordinary terminal rules.

Its scope items: (1) the connector contract: manifest (name, version, transport MCP or stdio,
schedule, source address, the key it needs), the protocol that hands raw items to the archive,
cursor and increment, error codes; (2) the runner in ulei: start by schedule, one process per
run, timeout, log, only the connected connectors of the connectors directory (from config, may be
a local git repository); (3) isolation of a scheduled run: Landlock (writes only into its own
directory), a seccomp filter, systemd network limits (only the source's address), the key by
pointer and only its own; wider access by an explicit owner record in config; a kernel without
Landlock or seccomp is refused with a clear reason or run under an explicit owner opt-out, as
this record decides; (4) connector binaries as separate release files, only the connected ones
installed; (5) a stdio test connector and an MCP server wrapper example in the repository. Out of
scope: real connectors (mail, calendar, ...), a shared connector catalogue and publishing
connectors.

What exists on main:

- The archive is decided and not built: `ulei-memory serve` serves C10 0.1.0 on a Unix socket
  with the peer uid check; `put` takes an item's fields with the original as `bytes_b64`, body
  at most 1 MiB, answers `stored` or `duplicate`, refuses a changed record under a stored
  `(source, external_id)` with `409 conflict`; `source` is `^[a-z][a-z0-9_-]{0,63}$`, `dialogue`
  for the C8 dialogue and a connector's manifest name for d9; `sync` pulls the dialogue and is
  called by the d9 runner's schedule; nothing in the archive wakes on a timer (ADR-0051 sections
  2, 3 and 8, cards 6 to 8).
- The daemons run as systemd user units of the deployment's account, hardened with
  `NoNewPrivileges`, `SystemCallFilter=@system-service`, `RestrictAddressFamilies` and limits;
  the job `systemd-units` starts them under the runner user's manager with lingering and checks
  each process for `NoNewPrivs`, a seccomp filter, `memory.max` and `pids.max` (ADR-0042
  sections 8 and 9). On a host with `kernel.apparmor_restrict_unprivileged_userns=1`, the
  Ubuntu 24.04 default, the units run without their namespace options (ADR-0042 section 8).
- One `ulei.toml` checked against C9 (ADR-0040, ADR-0041). Secrets live in env-files whose path
  the config names; a process gets the path, never the value (ADR-0015 section 6).
- The engine's MCP client speaks Streamable HTTP on loopback (ADR-0026); there is no stdio MCP
  client. A module crate depends on no other module crate (ADR-0001 section 3), so the core
  cannot link it.
- `rustix` 1.1.6 is locked with `std` and `net`; it wraps `set_no_new_privs` and reads the
  seccomp mode, but has no Landlock calls and cannot install a seccomp filter program
  (`thread/prctl.rs`). The workspace denies `unsafe`.
- The release is five static musl binaries with one `SHA256SUMS`, installed by
  `deploy/install.sh` (ADR-0042 section 10).

Reference: none. The port map has no entry for source connectors; the reference's `cronjob`
tool (`THIRD_PARTY_NOTICES.md#ref-agent`) schedules agent prompts, not source pulls, and is not
ported by this record.

## Decision

### 1. Places and names

| Part | Where | Traces to |
|---|---|---|
| Contract C11 "Source connector" 0.1.0 | `contracts/c11-connector.schema.json`, fixtures, generated types in `ulei-contracts` | scope 1, 11:24 ("the core holds the contract") |
| Runner | binary `ulei-connectors` of `ulei-core` (`src/bin/ulei_connectors.rs`, module `src/connectors/`): `serve` (the schedule), `run <name>` (one run now, same isolation), `list`, `reset <name>` (drops the cursor), and `exec`, the launcher inside a run's unit (section 5) | scope 2, 11:24 ("the schedule in the core") |
| Its unit | `deploy/systemd/ulei-connectors.service`, in the layout of ADR-0042 | scope 2 |
| Test connector | crate `crates/connector-test`, `ulei-connector-test`, binary `ulei-connector-test`, transport stdio | scope 5 |
| MCP wrapper example | crate `crates/connector-mcp-example`, `ulei-connector-mcp-example`, transport MCP | scope 5 |
| Connectors on a server | `connectors.dir` of `ulei.toml`: one subdirectory per connector holding `connector.json` and its program; may be a git work tree | 12:01 |

A connector crate is a class-2 crate under the existing arch rules: it lives in
`crates/connector-<name>`, is named `ulei-connector-<name>`, and depends on `ulei-contracts`
only among workspace crates; its `[package.metadata.ulei]` role says "source connector" and its
contracts are `["C11"]`.

Reason: the owner put the contract and the schedule into the core; the core binary `ulei-inbox`
already has the daemon shape (config through C9, one socket client per call, a user unit), and a
separate binary keeps `ulei` and `ulei-inbox` free of the runner's code and budget.

Rejected:
- The runner in `ulei-engine`: a connector run is no agent turn, and the engine's unit is the
  agent's sandbox, wide enough for the terminal and MCP servers (ADR-0042 section 3).
- The runner inside the archive daemon: ADR-0051 section 3 keeps timers out of the archive, and
  the archive would supervise foreign processes.
- A new module crate `ulei-connectors`: the owner placed the schedule in the core, and the core
  already holds the config and wire clients the runner needs.
- Connectors as libraries linked into the runner: any program or MCP server must be a connector
  (11:24), and one faulty connector would share the runner's address space.

### 2. The contract C11 0.1.0

C11 is a JSON Schema 2020-12 file with `$defs` for each message below; `cargo xtask
gen-contracts` writes their Rust types; fixtures `valid/` and `invalid/` per type. MCP and stdio
connectors share the fixtures of `Request`, `Item` and `Error`.

Manifest `connector.json` in the connector's directory:

| Field | Meaning |
|---|---|
| `name` | `^[a-z][a-z0-9_-]{0,63}$`, equal to the directory name; the C10 `source` of its items; `dialogue` is refused |
| `version` | the connector's own semver |
| `contract` | the C11 version it speaks; the runner refuses another major |
| `transport` | `stdio` or `mcp` |
| `command` | argv; the first element is a path relative to the connector's directory or an absolute path under `/usr` |
| `schedule` | `{"every": <seconds>}`, 60 to 604800 |
| `timeout` | seconds, 1 to 3600, default 300 |
| `source` | `{"addresses": ["host:port", ...]}`, at most 8; empty for a source that needs no network |
| `key` | optional; the name of the key it needs, `^[a-z][a-z0-9_-]{0,63}$` |

Example: `{"name": "notes", "version": "0.1.0", "contract": "0.1.0", "transport": "stdio",
"command": ["bin/notes"], "schedule": {"every": 900}, "source": {"addresses":
["notes.example.org:443"]}, "key": "notes"}`.

`Request`, written by the runner once per run: `contract`, `cursor` (string up to 4,096 bytes,
or null on the first run), `limit` (items per page, default 100), `key_file` (absolute path or
null, section 5.4), `data_dir` (the connector's writable directory).

`Item`, one raw record: `external_id` (at most 512 bytes), `who` (optional), `at` (optional Unix
time), `kind` (media type), `text` (the text the archive indexes), `bytes_b64` (the original).
These are C10 `put`'s fields without `source`, which only the runner sets.

stdio transport: the runner writes the `Request` as one JSON line on stdin and closes it. The
connector writes `Line`s on stdout, one JSON object per line, each with `type`:
`item` (the `Item` fields), `checkpoint` (`cursor`: every item before it may be considered
stored), `done` (optional `cursor`; the run's last line) or `error` (an `Error`; the run's last
line). stderr is free text for the log.

MCP transport: the connector is an MCP server over stdio (newline-delimited JSON-RPC). The runner
initializes with the protocol version and client info of ADR-0026, then calls the tool
`ulei_pull` with the `Request` as arguments; the result's `structuredContent` is a `Page`
(`items`, `cursor`, `more`). While `more` is true the runner calls again with the page's
`cursor`. A result with `isError` carries an `Error` as `structuredContent`. Each page maps onto
the stdio stream as its items followed by a checkpoint, so one driver handles both transports.

`Error.code` (with `message`, and `retry_after` seconds for `rate_limited`):

| Code | Meaning | Runner's answer |
|---|---|---|
| `auth_failed` | the source refused the key | logged; next run on schedule |
| `key_unreadable` | the key file is missing or malformed | logged; next run on schedule |
| `source_unavailable` | network or source failure | logged; next run on schedule |
| `rate_limited` | the source asked to wait | next run not before `retry_after` |
| `bad_cursor` | the stored cursor is not accepted | logged on every run until the owner runs `reset` |
| `bad_request` | the `Request` is not understood | logged |
| `internal` | anything else | logged |

`BrokerRequest` and `BrokerReply` are the network broker's messages (section 5.3); `RunRecord`
is a line of the run log (section 4).

Owner records in `ulei.toml`, C9 next minor: `connectors.dir`, `connectors.state` (the runner's
state and the connectors' data), `connectors.log`, `connectors.run_dir` (the brokers' sockets),
`connectors.enabled` (the connected names), `connectors.sync_every` (seconds, default 900), and
per connector `[connectors.<name>]` with `key_file`, `allow`, `read`, `write`, `network` and
`landlock` (section 5.5).

Traces to scope 1 and the decision of 11:24.

Reason: the manifest is JSON so the runner reads it with the generated types and `serde_json` it
already links; C11 makes the connector's side a contract a program in any language can meet,
which is what lets the agent write a connector or wrap an MCP server (11:24).

Rejected:
- A TOML manifest: a second TOML reader, or the C9 decoder widened past `ulei.toml`.
- MCP only: the smallest connector the agent can write is a program printing JSON lines; the owner
  named both transports.
- stdio only: the owner named MCP and wrapping ready servers.
- MCP over Streamable HTTP: the connector would listen on a socket inside its sandbox, which
  section 5.1 denies.
- Cron expressions as the schedule: a parser no pulled source needs; a period covers them.

### 3. Delivery into the archive (the C10 boundary)

- The runner is a C10 client: for every `Item` it sends `put` with `source` = the manifest's
  `name` and the item's fields, in order, one request per connection as C10 requires. A connector
  never reaches C10 and cannot name another source.
- 1 MiB cap: an item whose `put` body would exceed 1,048,576 bytes is not sent; the runner reads
  at most 1 MiB plus 4 KiB of a stdout line and discards the rest of a longer line. The item counts
  as `too_large` and its `external_id` goes into the run record; the run continues. ADR-0051
  leaves items above 1 MiB undecided, so they are logged, not stored.
- A cut line keeps its id: the runner scans the whole line, the discarded rest included, for its
  top-level `type` and `external_id` without keeping the rest, and holds at most 3,072 raw bytes
  of the id's JSON string (512 bytes of id, each at most a six-byte `\u` escape). A cut line whose
  `type` is `item` and whose `external_id` is a string of 1 to 512 bytes, wherever they stand,
  is `too_large`. Any other cut line, an item without a readable `external_id` included, is not a
  valid `Line` and ends the run as `protocol`, its stderr line saying which field could not be
  read; without an id there is nothing to keep, and every readable id is kept.
- Answers: `stored` and `duplicate` are counted; `409 conflict` counts as `conflict` with its
  `external_id` logged and the run continues, because the archive keeps the first bytes of a
  record (ADR-0051 section 2); any other failure, or C10 not answering, ends the run with outcome
  `archive` and the cursor unchanged.
- Cursor: kept by the runner in `<connectors.state>/<name>/runner.json` (written through a
  rename) with the schedule's last start and `not_before` (section 4), outside the connector's
  data directory. It moves to a `checkpoint`'s or the `done`
  line's cursor only after every item before it was answered. A crash repeats at most the items
  since the last stored cursor, which C10's dedup answers `duplicate`.
- `sync`: the schedule calls C10 `sync` at start and then every `connectors.sync_every` seconds
  as the runner's own call on a thread of `serve`, with no process of its own. A failed `sync`, or
  one that answers `synced: false`, is one stderr line; the next one stays on schedule, and a
  `sync` still in flight when the next is due is not doubled.

Traces to scope 1 ("the protocol that hands raw items to the archive", "cursor and increment")
and to 11:24 ("the core holds only the contract to put raw material into the archive").

Reason: the runner, outside the sandbox, is the only party that holds an archive client, so a
connector can add items under its own name and do nothing else to the archive; the cursor moves
with the archive's answers, so it never runs ahead of what was stored.

This agrees with ADR-0051 as it stands: `put` with `source` the manifest name, `sync` called by
the runner's schedule, the 1 MiB body, and the archive's `cursors` table kept for its own pull of
the dialogue. ADR-0051 needs no change.

Rejected:
- The cursor in C10: a new method for a value only the runner reads.
- The cursor kept by the connector in its directory: the connector could move it past items the
  archive never stored.
- The connector calling C10 itself: the archive socket inside the sandbox would let it put under
  any source and call `delete`.
- Items as files the runner imports from the connector's directory: a second format beside C10.
- Refusing the run on a too-large item: one record over the cap would stop its source for good.

### 4. The runner

- `ulei-connectors serve` reads `ulei.toml` at start; a config change takes a restart of its unit.
  It lists the subdirectories of `connectors.dir` that hold a `connector.json` and runs only those
  named in `connectors.enabled`; a listed name without a valid manifest is one stderr line and is
  not scheduled. It never runs git, and `.git` holds no manifest. Once its schedule is set it
  writes its ready line `ulei-connectors: ready: <n> connectors, sync every <s> s` on stderr.
- Schedule: a connector is due at `max(last start + every, not_before)`; one never run, or overdue
  after downtime, is due at once and runs once, not once per missed period. The daemon sleeps
  until the nearest due time of the connectors and of the `sync` entry, or the end of a run, so it
  has no wake-up while nothing is due (ADR-0001). Each run is a thread of `serve`.
- State: each run that starts writes its start time into `runner.json` and clears `not_before`;
  a run ending `error` with `rate_limited` and `retry_after` sets `not_before` to its end plus
  `retry_after`. Every other code, and `rate_limited` without `retry_after`, keeps the next run on
  schedule. `serve` reads both at start, so the schedule survives a restart, and keeps them in
  memory afterwards.
- One process per run: each run is a transient service of the user manager, started through
  `systemd-run --user --wait --pipe --collect --unit=ulei-connector-<name>-<unix time>`
  with the properties of section 5.1; its stdin, stdout and stderr are the runner's pipes.
- One run per connector at a time, across processes: a run holds an exclusive `flock` of
  `<connectors.state>/<name>/lock` from before it reads the state until it ends, so `run <name>`
  while `serve` runs that connector, or the other way round, is recorded `busy`: a `RunRecord`
  without `unit`, with no state written. In `serve`, a connector due while its own run still runs
  is recorded `busy` once and is next due a period later. `reset <name>` takes the same lock and
  refuses while a run is in flight, so a run cannot overwrite it.
- At most four runs at once: a due run without a free slot starts, not `busy`, when one frees,
  earliest due first.
- Shutdown: on SIGTERM or SIGINT, `serve` starts nothing more, stops the units of its runs in
  flight with `systemctl --user stop` (a unit that had not started yet is stopped as soon as
  `systemd-run` reports it running), writes their run records, whose outcome is what the stopped
  unit ended with and whose stderr line says the run was stopped at the shutdown, and exits 0. A
  `sync` in flight is not waited for.
- On the shared stderr pipe the runner reads `systemd-run`'s `Running as unit:` line (the unit
  started) and its last `Finished with result:` line (the unit's `Result=`), and the launcher's
  first line, its refusal or the Landlock it executes the connector under (`landlock abi <n>` or
  `landlock off`), and, only as the line right after that one, its `exec_failed` refusal when
  `execve` of the connector fails (outcome `refused`, code `exec_failed`); the rest is the
  connector's, so a connector cannot pass for a `landlock` or `seccomp` refusal. A `systemd-run` that did not start the unit is
  outcome `refused`, code `unit_start_failed`, with its last line as the reason. A run the runner
  gives up on (`protocol`, `archive`) is stopped with `systemctl --user stop`, after which
  `systemd-run` still reports the result.
- Timeout: the manifest's `timeout` as `RuntimeMaxSec=`; systemd stops the unit, SIGKILL after
  `TimeoutStopSec=10s`. Outcome `timeout` when the unit's result is `timeout`; cursors already
  stored stay.
- `ulei-connectors run <name>` starts one run now with the same unit and isolation, `busy` while
  another run of it is in flight, whatever the schedule says; it is the owner's and the tests' way
  to run a connector. A run it makes moves `runner.json` but not the schedule of a `serve` already
  running. The agent's one-off scripts are not connector
  runs and stay under the terminal rules (12:36).
- Log: one `RunRecord` JSON line per run appended to `connectors.log` (0600): `at`, `connector`,
  `unit`, `outcome` (`ok`, `error`, `timeout`, `refused`, `exit`, `protocol`, `archive`, `busy`),
  `code`, the counts `stored`, `duplicate`, `conflict`, `too_large`, the external id of every
  `too_large` item with none dropped, the first 16 `conflict` external ids (the archive holds the
  first bytes of each, so the count suffices past them), whether the cursor moved, the Landlock
  ABI used, the owner grants in effect, `duration_ms` and the last 2 KiB of the connector's
  stderr; and one stderr line per run for the journal. The key's value never appears; the run
  record holds no item content.
- Exit: a connector that exits non-zero without an `error` line is `exit`; a line that is not a
  valid `Line` ends the run as `protocol`.

Traces to scope 2 and the decisions of 11:24, 12:01 and 12:36.

Reason: the owner put the schedule into the core; a daemon that sleeps until the nearest due run
keeps ADR-0001's idle rule, and a transient unit per run is the one systemd mechanism that gives
each run its own seccomp filter, cgroup limits and timeout under the user manager.

Rejected:
- A systemd timer per connector: the schedule would live in unit files generated whenever the
  connected set changes, outside the core.
- One periodic timer running `run-due`: wake-ups while nothing is due, schedules rounded to its
  period.
- Long-running connectors kept between runs: idle memory per connector, and the isolation is
  applied per start (12:36).
- `systemd-run --scope`: a scope carries cgroup limits only, no seccomp or `OpenFile=`.

### 5. Isolation of a scheduled run

Traces to scope 3 and the decision of 12:36.

| Mechanism | Enforces | Needs | Applied by |
|---|---|---|---|
| seccomp (systemd) | no socket of any family; the `@system-service` call set; no namespaces, personality, realtime or set-uid | `CONFIG_SECCOMP_FILTER`, NoNewPrivs; systemd 253 or later for `OpenFile=` | the transient unit's properties |
| Landlock | reads only system files, its own program, its key file and its data; writes, truncates and creates only in its data directory | Linux 6.2 or later (ABI 3) with `landlock` in the LSM list | the launcher `ulei-connectors exec` before it executes the connector |
| Network broker | TCP only to the `host:port` entries of its manifest and the owner's `allow` | nothing beyond the above | the runner, over a socket the unit receives |

#### 5.1 seccomp through the transient unit

Properties: `NoNewPrivileges=yes`, `SystemCallFilter=@system-service` plus
`landlock_create_ruleset landlock_add_rule landlock_restrict_self`, `SystemCallErrorNumber=EPERM`,
`SystemCallArchitectures=native`, `RestrictAddressFamilies=` with an empty value,
`OpenFile=<connectors.run_dir>/<unit>.sock:broker`, `RestrictNamespaces=yes`,
`LockPersonality=yes`, `RestrictRealtime=yes`, `RestrictSUIDSGID=yes`, `UMask=0077`,
`MemoryMax=256M`, `TasksMax=64`, `TimeoutStopSec=10s` and `RuntimeMaxSec=`. `systemd-run` sends
the empty `RestrictAddressFamilies=` as an empty allow list, which the manager keeps as `none`;
the word `none` itself `systemd-run` passes on as a family name, which the manager refuses, so
the runner never sends it. The empty allow list denies every `socket(2)`, `AF_UNIX` included, so
a connector cannot reach the daemons' sockets, the archive or the user bus; sockets passed in and
`socketpair()` are not affected (systemd.exec(5)), which is how the broker socket arrives. 256M
and 64 tasks cover an interpreter runtime and its threads.

The launcher reads `/proc/self/status` before anything else and refuses unless `NoNewPrivs: 1`
and `Seccomp: 2`: a manager or kernel that skipped the filter is caught by the run itself. It
then clears the environment (a user unit inherits the manager's), sets `HOME` and `TMPDIR` to the
data directory, `PATH=/usr/bin:/bin`, `LANG=C.UTF-8` and keeps `LISTEN_FDS`, `LISTEN_FDNAMES`
and `LISTEN_PID`, which stay valid because `execve` keeps the pid.

#### 5.2 Landlock in the launcher

The launcher gets the run's paths in its argv (paths only, no secret: `exec NAME DATA_DIR DIR
[--key FILE] [--read PATH]... [--write PATH]... [--landlock-off] -- PROGRAM [ARG...]`), builds a
ruleset that handles every filesystem right of the running ABI, with the `landlock` crate's
compatibility level set to hard requirement, and restricts itself before it executes the
connector; children inherit the domain. A path the table requires that does not exist refuses
the run with code `landlock`. A file gets only the rights a file can have.

| Path | Rights |
|---|---|
| `/usr`, `/etc`, and `/lib`, `/lib64`, `/bin`, `/sbin` where present | read, execute |
| `/proc`, `/dev/null`, `/dev/zero`, `/dev/urandom`, `/dev/random` | read (`/dev/null` also write) |
| the connector's directory in `connectors.dir` | read, execute |
| its key file (section 5.4) | read, that file only |
| `<connectors.state>/<name>/data` | read, write, create, remove, truncate, refer |
| the owner's `read` grants; its `write` grants | read; as its data directory |

- Minimum ABI 3: before it, truncation is always allowed (kernel Landlock docs, "File truncation
  (ABI < 3)"), so a connector could truncate a file it may not write, such as the archive, which
  breaks "writes only into its own directory".
- ABI 6 or later also sets the scopes `LANDLOCK_SCOPE_SIGNAL` and
  `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`. On ABI 3 to 5 a connector can signal other processes of
  the account; the run record names the ABI.
- Landlock's ptrace rule (a sandboxed process traces only processes in its own domain) keeps
  `/proc/<pid>/mem` and `environ` of the daemons closed although `/proc` is readable.
- The home, `ulei.toml`, the other connectors' directories and keys, the archive, the journal
  and every socket path stay unreadable and unwritable.

This is "sees only its own directory" of 12:36: the connector reaches the archive only through
the runner's `put` under its own `source` (section 3), and its only writable directory is its
own.

#### 5.3 Network: the broker

The runner listens on `<connectors.run_dir>/<unit>.sock` (0600; `run_dir` created 0700) for one
connection before it starts the unit; systemd makes it through `OpenFile=` and hands it to the
connector as fd 3 named `broker` (sd_listen_fds(3)); the runner then unlinks the path. The path
holds no colon, which `OpenFile=` separates at, and at most 107 bytes (`sun_path`); a path that
breaks either, or a socket that cannot be bound, refuses the run (`run_dir`, `broker`). On it the connector sends `BrokerRequest` lines
(`{"connect": "host:port"}`); the runner answers with a `BrokerReply` line (`{"ok": "ip:port"}`
with the connected TCP socket attached as `SCM_RIGHTS` in the same message, or `{"error": code}`
with `not_allowed`, `resolve_failed`, `connect_failed` or `limit`).

- Allowed: exactly the `host:port` strings of the manifest's `source.addresses` and the owner's
  `allow`. The runner resolves the name at every request through the system resolver and tries
  each address with a 10 s connect timeout, so a source whose addresses change keeps working.
- A name that resolves to a loopback, private (RFC 1918, `fc00::/7`), link-local, unspecified or
  multicast address is refused unless that `host:port` is in the owner's `allow`; one such
  address among a name's answers refuses it. These classes also hold the shared address space
  `100.64.0.0/10`, `0.0.0.0/8`, the reserved `240.0.0.0/4` with `255.255.255.255`, and the local
  NAT64 prefix `64:ff9b:1::/48`; an IPv6 address embedding an IPv4 one (IPv4-mapped,
  IPv4-compatible, NAT64 `64:ff9b::/96`, 6to4 `2002::/16`) takes the class of the embedded
  address.
- At most 32 connections per run; the 33rd request is `limit`. The runner never reads the
  stream: TLS and its host name belong to the connector.
- A line that is not a `BrokerRequest`, or longer than 4 KiB, ends the broker without a reply.
- The broker lives only as long as its run: it ends with the run on every outcome, a connect in
  flight included, which ends at once instead of running out its 10 s, and no socket file of it
  stays in `run_dir`.
- `rustix::net` (`sendmsg` with `SendAncillaryMessage::ScmRights`, already locked with `net`)
  passes the socket; the connector receives it with `recvmsg`.

Reason: the 12:36 decision names systemd network limits for "only the source's address", and the
mechanism systemd has for addresses does not apply under the user manager the deployment runs:
`IPAddressAllow=` is a cgroup BPF program that systemd installs only as root ("not running as
root" in `src/core/bpf-firewall.c`) and that otherwise has no effect (systemd.resource-control(5)).
The systemd limit that does apply, an empty `RestrictAddressFamilies=` allow list, takes every
socket from the connector, and the broker gives back exactly the source's connections. Resolution at each request
answers the host whose addresses change.

Rejected:
- `IPAddressAllow=` and `IPAddressDeny=`: no effect under the user manager (above); addresses and
  prefixes only, fixed when the unit starts.
- `PrivateNetwork=` with a proxy: in a user manager it needs unprivileged user namespaces
  (systemd.exec(5)), which Ubuntu 24.04 restricts by default (ADR-0042 section 8).
- Landlock TCP rules (ABI 4, Linux 6.7): they match ports, not addresses (kernel Landlock docs).
- A seccomp filter on `connect(2)`: BPF cannot dereference the address argument (kernel
  seccomp_filter docs).
- A proxy named in `HTTPS_PROXY` with `AF_INET` allowed: a connector that ignores the variable
  connects anywhere.

#### 5.4 The key by pointer

The manifest's `key` names the key the connector needs; the owner's `connectors.<name>.key_file`
names an env-file. The runner checks that the file exists before the run (`refused`,
`key_not_configured` otherwise), never reads it, and passes its path as `Request.key_file`;
Landlock grants read on that one file. The value is never in argv, the environment or the log
(ADR-0015 section 6). A connector without `key` gets `key_file: null` and reads no key file.
Traces to scope 3 ("the key by pointer, only its own") and 12:36.

Rejected:
- The runner reading the key and writing its value into the `Request` or the environment: the
  value would pass through the runner's memory and pipes, and an environment is inherited by
  every child of the connector.
- One shared env-file for all connectors: Landlock grants whole files, so every connector would
  read every key.

#### 5.5 Owner grants

Every widening is a record in `[connectors.<name>]` of `ulei.toml` (12:36), listed in each run
record:

| Key | Grants |
|---|---|
| `allow` | more `host:port` entries for the broker, and private addresses for those entries |
| `read`, `write` | more paths for Landlock |
| `network = "direct"` | `RestrictAddressFamilies=AF_INET AF_INET6` instead of the empty list, so `AF_UNIX` stays closed, with Landlock TCP connect (`LANDLOCK_ACCESS_NET_CONNECT_TCP`) limited to the ports of `source.addresses` and `allow` where the ABI is 4 or later; the run record's `connect_ports` names those ports, and is absent below ABI 4 or under `landlock = "off"`, where the ports are not limited; the broker socket is still passed. Landlock limits TCP connect only: UDP, DNS included, is not limited. For a wrapped MCP server with its own network stack |
| `landlock = "off"` | the opt-out of section 5.6 |

#### 5.6 Kernels without Landlock or seccomp

A run is refused with outcome `refused` and one stderr and log line naming the mechanism, the
errno or value seen and the requirement:

- Landlock: `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` answering
  `ENOSYS` (no Landlock in the kernel), `EOPNOTSUPP` (disabled at boot) or an ABI below 3.
- seccomp: `NoNewPrivs` or `Seccomp: 2` missing in the launcher's `/proc/self/status`.
- The user manager: `systemd-run` failing to start the unit, code `unit_start_failed` with its
  last line as the reason. A manager older than systemd 253 has no `OpenFile=`: `systemd-run`
  answers `Unknown assignment: OpenFile=<path>:broker`, or the manager `Cannot set property
  OpenFile, or unknown property.`, and no run starts.

Example, the launcher's line, which the run record keeps in `stderr`: `ulei-connectors: exec
refused landlock: landlock unavailable (ENOSYS); Linux 6.2 or later with landlock in the LSM list
is required, or set connectors.notes.landlock = "off"`. The runner's own line for the journal is
`ulei-connectors: notes refused landlock: stored 0, duplicate 0, conflict 0, too_large 0, cursor
unchanged, <n> ms; ` followed by the same reason.

The owner may opt out of Landlock alone, per connector, with `landlock = "off"`; the run then
proceeds with seccomp and the broker, and every run record says `landlock: off`. There is no
opt-out of seccomp or the broker.

Reason: refusal by default keeps 12:36 true on every host; without Landlock a connector still has
no socket and no network beyond its source, while reading the account's files is a widening the
owner may grant (12:36 "wider access by the owner's permission"). Without seccomp the connector
could open any socket, including the daemons' and the archive's, which no owner record narrows
again.

Rejected:
- Refusal with no opt-out: a container or old-kernel host could never run a connector, though the
  owner may grant wider access.
- An opt-out of everything: the run would have the account's whole reach, which 12:36 excludes
  for scheduled runs.
- The `landlock` crate's best-effort level: it runs unconfined on an old kernel without saying so.

#### 5.7 How CI proves the refusals

Job `systemd-units` (its user manager with lingering, ADR-0042 section 9) runs a stdlib driver
`ci/connectors_run.py` with the test connector, a fake C10 socket and synthetic paths; each check
reports its measured value or the reason in an annotation:

| Refusal | Proven by |
|---|---|
| read outside its paths, another connector's key file, `ulei.toml` | the test connector's probes answer `EACCES` |
| write, create or truncate outside its data directory, truncating the fake archive file | probes answer `EACCES`; the file's bytes and size unchanged |
| `socket(AF_INET)`, `socket(AF_UNIX)` toward the fake C10 socket | probes answer `EAFNOSUPPORT` or `EPERM` |
| a broker request for an unlisted `host:port`; a listed name resolving to loopback without `allow`; the 33rd connection of a run | `not_allowed`, `limit`; with an owner `allow` on a loopback fake source the connection works and its first line arrives; no broker socket stays in `run_dir` |
| `socket(AF_UNIX)` and connect ports under `network = "direct"` | `socket(AF_INET)` succeeds, `socket(AF_UNIX)` answers `EAFNOSUPPORT` or `EPERM`; on ABI 4 or later a connect to an unlisted port answers `EACCES` and the listed port connects |
| a kernel without Landlock | the launcher in a test unit with `SystemCallFilter=~landlock_create_ruleset` and `SystemCallErrorNumber=ENOSYS`, then `EOPNOTSUPP`, is refused with its line; with `landlock = "off"` it runs and logs it |
| no seccomp filter | the launcher started outside any unit is refused before it executes anything |

The checks run as groups, one CI step each (`sandbox`, `outcomes`, `broker`, and `schedule` for
section 4's `serve`). GitHub keeps at most 10 notices per step and 50 annotations per job, so
each group folds its passed checks into two to four notices, one line per check with its title
and measured value, and writes each failed check as an error annotation of its own. Each step
first reads `uname -r`, `/sys/kernel/security/lsm` and the Landlock ABI into its first notice and
fails if Landlock is absent or below ABI 3, so the refusals above are measured on a kernel that
enforces them. The kernel-without-Landlock case cannot be booted on the runner; the
seccomp-injected `ENOSYS` and `EOPNOTSUPP` are the values such a kernel returns to the same
probe.

### 6. Release and install

- `ci/release.sh` adds `ulei-connectors` to the core binaries and a tree `connectors/<name>/`
  per released connector with its `connector.json` and binary; the one `SHA256SUMS` lists every
  file by its relative path. The released connector is `test` (`ulei-connector-test`), so the
  owner can check isolation on a host; the MCP example is not released.
- `deploy/install.sh RELEASE_DIR` verifies the whole `SHA256SUMS` before any copy, as now, and
  installs the core binaries and units only. `deploy/install.sh --connector NAME --dir DIR
  RELEASE_DIR` installs that one connector into `DIR/NAME/` the same way (rename, mode 0755 for
  the binary and 0644 for the manifest, an identical file left as it is). It never edits
  `ulei.toml`; connecting stays the owner's `connectors.enabled`.

Traces to scope 4 and the decision of 11:24.

Reason: separate files installed only by name (11:24), one checksum file for one commit.

Rejected:
- Connectors linked into the `ulei-connectors` binary: every server would carry every connector.
- One artifact per connector: several checksum files for one commit.
- `ulei-connectors install`: the release path stays POSIX sh without `ulei.toml`, as ADR-0042
  section 10 decided.

### 7. The test connector and the MCP wrapper example

- `ulei-connector-test` (stdio): emits synthetic items numbered from its cursor (`limit` per run,
  a checkpoint after each item), reads its key file when given one and puts a hash of it into an
  item, connects to its source through the broker when its manifest has one, and runs the probes
  of section 5.7 when its argv names them. It is the conformance subject of every runner test.
- `ulei-connector-mcp-example` (MCP): an MCP server over stdio serving `ulei_pull` that wraps a
  ready MCP server given as its argv: it starts that server as a child over stdio, calls the
  listing tool named in its argv and maps each entry to an `Item` with the field names given in
  its argv. Its tests wrap a synthetic stdio MCP server; the same synthetic records delivered
  through the example and through the test connector produce the same C10 `put` bodies. A wrapped
  server with its own network stack runs under `network = "direct"` (section 5.5).

Traces to scope 5 and the decision of 11:24.

Reason: these are the patterns the agent copies when it writes a connector or wraps a ready MCP
server (11:24); both are Rust in the workspace, tested in CI with no interpreter.

Rejected: a Python test connector: the release ships Rust, and an interpreter's own reads would
widen the Landlock paths the test proves.

### 8. Budgets

`ulei-connectors serve` is a daemon under ADR-0001's row: static musl binary at most 2,621,440 B,
VmHWM at most 4,096 kB after 100 runs of the test connector, median spawn to its ready line at
most 10,000 us, 0 voluntary context switches in 10 s with nothing due, wchar per run at most
16,384 B (its log line and cursor file). A section of `ci/budgets.sh` with the stdlib driver
`ci/connectors_budget.py` measures it; the per-run lines run in job `systemd-units`, on the musl
release build: 100 enabled test connectors, never run, so all due at start, run four at a time
against a fake C10, then VmHWM of `serve` and its wchar, which holds the writes of its reaped
`systemd-run` children, over the 100 runs. Connector binaries have no budget row:
they are not resident, and the unit's `MemoryMax` bounds a run.

### 9. Implementation cards, in order

Each card merges alone with the workspace green, tests on synthetic data only, and rewrites in
place every document that describes what it changes. A card that adds a crate, a dependency or a
contract version carries the short ADR the ADR gate needs, pointing here. "Next minor" means the
next free version when the card lands.

1. C11 0.1.0 and the `[connectors]` keys (about 60 min). Contracts: C11 new, C9 next minor;
   crates: `contracts`. Goal: section 2's schema with fixtures, generated types, the keys and
   `docs/ulei.example.toml`. Check: `cargo xtask contracts`; valid fixtures round-trip and
   invalid ones are refused (a manifest named `dialogue`, a schedule below 60, a third
   transport).
2. The test connector (about 60 min). Crate `connector-test` (new, `ulei-contracts`,
   `serde_json`, `rustix`). Goal: section 7's stdio behaviour without probes and broker. Check:
   its lines parse as C11 `Line`; a second run from the stored cursor continues the numbering; a
   key file's hash appears in an item.
3. Runner core (about 60 min; after ADR-0051 card 6). Crate `core`. Goal: manifest discovery and
   the `enabled` filter, the stdio driver, the C10 `put` client, the cursor file, the run record
   and `reset`. Check: against a fake C10 socket and a scripted connector over pipes, items arrive
   with `source` = name; the cursor moves only after the answers; a too-large item and a
   `conflict` are logged and the run goes on; C10 down ends the run `archive` with the cursor
   unchanged.
4. Transient unit and seccomp (about 60 min). Crates `core`, `connector-test`. Goal: `run <name>`
   through `systemd-run` with section 5.1's properties, `exec` with its status check and clean
   environment, the timeout, the probes. Check in `systemd-units` (`ci/connectors_run.py`): a run
   lands items in a fake C10; `socket(AF_INET)` and `socket(AF_UNIX)` are refused; a sleeping
   connector ends `timeout`; `exec` outside a unit is refused.
5. Landlock (about 60 min). Crate `core`, dependency `landlock`. Goal: section 5.2's rules,
   ABI 3 as a hard requirement, the scopes from ABI 6, `read`, `write` and `landlock = "off"`.
   Check in `systemd-units`: the Landlock rows of section 5.7, the injected `ENOSYS` and
   `EOPNOTSUPP` refusals, the opt-out's run record, the kernel notice.
6. The broker (about 60 min). Crates `core`, `connector-test`. Goal: section 5.3 with `OpenFile=`,
   the allow rules, resolution per request, the limit, `network = "direct"`. Check in
   `systemd-units`: a listed loopback fake source under an owner `allow` is reached; an unlisted
   one is `not_allowed`; a name resolving to loopback without `allow` is refused; the 33rd
   connection is `limit`.
7. The schedule (about 60 min). Crate `core`. Goal: `serve` with due times, catch-up once,
   `busy`, four at once, `rate_limited`, the `sync` entry. Check: with periods of 60 s against a
   fake C10, each connector runs once per period, an overdue one once at start, `sync` is called,
   and no wake-up happens between due times.
8. Unit and budget (about 60 min). `deploy/systemd`, `ci/units_run.py`, `ci/budgets.sh`. Goal:
   `ulei-connectors.service` in ADR-0042's layout and hardening with its ReadWritePaths, and
   section 8's budget section. Check: jobs `systemd-units` (security threshold, verify, start)
   and `budgets`.
9. MCP transport in the runner (about 60 min). Crate `core`. Goal: the stdio MCP client of
   section 2 (initialize, `ulei_pull`, pages, `isError`). Check: against a scripted MCP server,
   the pages' items and cursors reach the fake C10 as the stdio driver delivers them; an `Error`
   result is logged with its code.
10. The MCP wrapper example (about 60 min). Crate `connector-mcp-example` (new). Goal: section 7's
    wrapper and its synthetic ready server in tests. Check: the example and the test connector
    produce identical `put` bodies for the same records; both pass the C11 `Request`, `Item` and
    `Error` fixtures.
11. Release and install (about 60 min). `ci/release.sh`, `deploy/install.sh`, `ci/install_run.py`.
    Goal: section 6. Check: job `release` builds and lists the connector tree; `install.sh`
    without `--connector` installs no connector; with it installs one, a second run changes
    nothing, a corrupted connector file is refused before any copy.
12. End to end with the archive (about 45 min; after ADR-0051 card 8). Check in `systemd-units`:
    the scheduled test connector's items are found by C10 `search` with `source` `test`, a
    repeated run adds nothing, and `sync` is answered by the real daemon.

### 10. External facts checked

Checked on 2026-11-22.

| Fact | Source |
|---|---|
| Landlock ABI 1 Linux 5.13, 2 Linux 5.19, 3 Linux 6.2, 4 Linux 6.7, 5 Linux 6.10, 6 Linux 6.12, 7 Linux 6.15; the docs list ABIs up to 12 | docs.kernel.org/userspace-api/landlock.html; the ABI table of the `landlock` crate's `compat.rs` (docs.rs/crate/landlock/0.4.5/source/src/compat.rs) |
| ABI 3 adds truncation control ("always allowed" before); ABI 4 TCP bind and connect by port only; ABI 6 signal and abstract socket scopes; ABI 9 pathname Unix socket connect; ABI 10 UDP | docs.kernel.org/userspace-api/landlock.html, "Previous limitations" and "Network rules" |
| `landlock_restrict_self` needs no_new_privs or `CAP_SYS_ADMIN`; `landlock_create_ruleset(..., VERSION)` gives the ABI, `ENOSYS` without Landlock, `EOPNOTSUPP` when disabled at boot; a sandboxed process may ptrace only processes in its domain | docs.kernel.org/userspace-api/landlock.html |
| `landlock` 0.4.8, released 2026-11-20, MIT OR Apache-2.0 (in `deny.toml`'s allow list), depends on `enumflags2`, `libc` (locked, 0.2.191) and `thiserror`; compatibility levels `BestEffort`, `SoftRequirement`, `HardRequirement` | crates.io/api/v1/crates/landlock, docs.rs/landlock/0.4.8 `CompatLevel` |
| `rustix` 1.1.6 (locked) has `set_no_new_privs` and reads the seccomp mode but has no Landlock calls and cannot install a filter | its source, `src/thread/prctl.rs` |
| `seccompiler` 0.5.0, 2025-03-07, Apache-2.0 OR BSD-3-Clause, pure Rust on `libc`; not needed, since the transient unit installs the filter | crates.io/api/v1/crates/seccompiler |
| A seccomp filter needs no_new_privs or `CAP_SYS_ADMIN`; "BPF programs may not dereference pointers" | docs.kernel.org/userspace-api/seccomp_filter.html |
| An empty `RestrictAddressFamilies=` allow list (`none`) denies all families; sockets passed in and `socketpair()` are unaffected; `systemd-run` sends an empty assignment as that empty allow list, and the word `none` as a family name, which the manager refuses | systemd.exec(5), systemd 255; job `systemd-units` on systemd 255.4 |
| `OpenFile=` opens a file or connects an `AF_UNIX` socket and passes the fd as in sd_listen_fds(3); added in systemd 253 | systemd.service(5), systemd 255 |
| `PrivateNetwork=` in a user manager implies `PrivateUsers=` and needs unprivileged user namespaces | systemd.exec(5), freedesktop.org, systemd 262 |
| `IPAddressAllow=`/`IPAddressDeny=` take addresses, prefixes and `any`, `localhost`, `link-local`, `multicast`; "no effect" where unsupported | systemd.resource-control(5), freedesktop.org, systemd 262 |
| The BPF firewall is skipped with "not running as root" when the manager is not root | github.com/systemd/systemd `src/core/bpf-firewall.c`, `emit_bpf_firewall_warning` |
| Ubuntu 24.04 restricts unprivileged user namespaces by default (`kernel.apparmor_restrict_unprivileged_userns=1`) | Launchpad bug 2046477; ubuntu.com/blog/ubuntu-23-10-restricted-unprivileged-user-namespaces |
| Ubuntu kernels set `CONFIG_LSM="landlock,lockdown,yama,integrity,apparmor"` | Launchpad bug 1953192 (Fix Released); `/boot/config-6.8.0-147-generic` of an Ubuntu 24.04 host |
| GitHub `ubuntu-24.04` runner image 20261116: Ubuntu 24.04.5, kernel 7.0.0-1017-azure, systemd 255.4 | github.com/actions/runner-images `images/ubuntu/Ubuntu2404-Readme.md` |

On the runner: kernel 7.0 carries Landlock ABI 3 and later and seccomp filters; systemd 255 has
`OpenFile=`; the job `systemd-units` already starts a user manager and sees a seccomp filter on
its units' processes. That the runner's LSM list holds `landlock` is checked by the first step of
section 5.7 on every run, which fails the job otherwise.

## Consequences

- A connected connector runs only as a transient user unit with seccomp, Landlock and the broker,
  or not at all; a host without Landlock runs it only under the owner's `landlock = "off"`.
- A connector cannot open sockets; contract-native connectors get their connections from the
  broker, and a wrapped server with its own network stack needs the owner's `network = "direct"`,
  which is not pinned to an address.
- On Landlock ABI 3 to 5 a connector can still signal the account's processes.
- The archive receives connector items only through the runner's `put`; ADR-0051 is unchanged.
- The agent writes a connector into `connectors.dir` only when the owner adds that directory to
  the engine unit's ReadWritePaths in a drop-in (ADR-0042 section 3); connecting it stays an
  owner edit of `connectors.enabled`.
- `connectors.log` grows by one line per run and is not rotated by ulei.
- Not decided here: real connectors, a shared catalogue, rotation of the run log, and
  `ulei status` lines for the runner.
