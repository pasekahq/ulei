# ADR-0040: one `ulei.toml` through a config contract, and `ulei status` (C1 1.2.0, C6 0.7.0)

Status: accepted
Date: 2026-11-20
Affects: crates/contracts, crates/core, crates/engine, crates/channels, contract:C1, contract:C6, contract:C9

## Context

Three daemons take their settings three ways. `ulei-inbox serve` takes flags (`--root`,
`--socket`, `--deliver C6_SOCKET` and three test-only flags, ADR-0036 section 1). `ulei-engine`
reads `ulei.toml` in a strict TOML subset (`crates/engine/src/config.rs`), and every setting
also has an environment variable that wins over the file. `ulei-tg` reads the same file with its
own copy of that reader (`crates/channels/src/tg/toml.rs`, ADR-0032 section 5) and reads its
speech-to-text key with a copy of the engine's env-file reader (`crates/channels/src/env_file.rs`,
ADR-0035 section 3). The copies exist because a module crate depends on no crate but
`ulei-contracts` (ADR-0001 section 3). The board tracks them as issue:b59e6aaff56f00343c18 and
issue:66737b71d2348a8fab06.

Nothing reports whether the system runs. The wire already answers most of it: C1 `health` of
the inbox, C6 `is_idle` and `status` of the engine (the core holds a C6 client, ADR-0036
section 1), C4G `GET /v1/health` and `GET /v1/model/routes` of the qb gateway (ADR-0038). C1 has
no queue counts, and nothing on the wire tells whether `ulei-tg` runs: it serves no socket.

Reference: there is none for the file format. The reference's config and setup wizards
(`hermes_cli/`) are on the "not ported" list of the port map, section «Установка и визарды»,
`/home/dev/ummanu-data/instance/state/knowledge/projects/ulei/hermes-port-map-u2b.md`. For the
status the reference is the gateway's start, stop and status in `gateway/run.py`
(`THIRD_PARTY_NOTICES.md#ref-agent`); section 7 has one row per behaviour.

## Decision

### 1. Where the one parse lives

| Option | Bytes per binary (estimate) | Rules touched |
|---|---|---|
| A. A config contract C9: a JSON Schema in `contracts/` from which `cargo xtask gen-contracts` writes the key catalogue, and the decoder in `ulei-contracts` | `ulei` +0 B (it reads no config, section 6). `ulei-inbox` about +12 KB: decoder and catalogue, new to it. `ulei-engine` and `ulei-tg` about +5 KB each: the catalogue; the decoder replaces the parser each has. The env-file reader moves, +0 B. | None of ADR-0001. One new contract (ADR-0005), and the hand-written part of `ulei-contracts` grows by about 300 lines inside its 1,500-line budget (`crates/contracts/AGENTS.md`). |
| B. ADR-0001 amended to admit one shared non-contract crate, `ulei-config` | As A: every binary still needs the whole key list to refuse an unknown key. | ADR-0001 sections 1 and 3, ADR-0002 (a third crate class), AGENTS.md hard rule 1 and its Layout row, the `cargo xtask arch` graph rule, the module map, and the reason of every copy made under ADR-0001 (ADR-0033 section 1, ADR-0036 section 1). |
| C. Per-binary readers checked against one shared conformance fixture | `ulei-inbox` about +8 KB for a third copy; the others +0 B. | None, but every key or syntax change edits each copy; the copies stay. |

The figures are estimates: the parser is about 110 lines without generics, the catalogue about
100 entries of a pattern and a kind. Each card of section 9 records the measured size.

A is chosen. It keeps ADR-0001 whole, and the file shared by three daemons is a contract
between them like a wire: `ulei-tg` and `ulei-inbox` read the engine's socket key, `ulei status`
reads every socket key. C is refused because the copies are what this record removes.

- `contracts/c9-ulei-toml.schema.json` (C9, JSON Schema 2020-12) is the catalogue: every section
  closed (`additionalProperties: false`), every key with its kind, the item sections
  (`engine.routes.<alias>`, `engine.mcp.<name>`) as maps with a name pattern.
  `gen-contracts` writes `ulei_contracts::generated::c9::KEYS`, the dotted key patterns with
  their kinds, instead of types.
- `ulei_contracts::config` is the decoder: it finds the file (section 2), reads the subset,
  checks every section and key against `KEYS` and returns the values by dotted key with their
  line numbers. It is the only TOML reader of the workspace outside `xtask`.
- `ulei_contracts::env_file` is the one reader of the env-files the config names (the syntax of
  ADR-0035 section 3).
- The meaning of a value (a path, seconds with the reference's rules, a step limit, a URL) stays
  with the binary that reads the key. The keys two binaries share (section 3) are strings: three
  socket paths and the owner session key, read as they are, and `engine.timezone`, whose zone
  lookup the engine and `ulei-tg` each make (ADR-0032 section 5).

### 2. The file

- Path: `--config PATH` of the binary, else `ULEI_CONFIG`, else
  `$XDG_CONFIG_HOME/ulei/ulei.toml` when `XDG_CONFIG_HOME` is an absolute path, else
  `$HOME/.config/ulei/ulei.toml`. Every binary that reads the file follows this order; a missing
  file at the chosen path is an error, so no binary runs without one.
- Syntax: the subset both readers accept today, unchanged: section headers, `key = "string"`
  without escapes, `key = integer`, `key = true` or `false`, comments and blank lines; a
  duplicate section or key is an error.
- Kinds: string, integer, boolean, or integer-or-string for values the subset can only write as
  text (decimal seconds such as `"2.5"`, a step limit such as `"unlimited"`, owner ids as
  `"1,2"`). Switches are booleans.
- One verdict: every binary checks the whole file against the catalogue, so an unknown or
  mistyped key anywhere stops each of them. Each then checks the values of the keys it reads.
- Errors, one stderr line naming the file and never a value:

| Case | Line |
|---|---|
| Unreadable file | `config <path>: <reason>` |
| Line outside the subset | `config <path> line <n>: <reason>` (today's reasons) |
| Section not in the catalogue | `config <path> line <n>: unknown section [<name>]` |
| Key not in the catalogue | `config <path> line <n>: unknown key <dotted key>` |
| Wrong kind | `config <path> line <n>: <dotted key> must be a string`, `an integer`, `true or false` or `an integer or a string` |
| Required key absent, value invalid | the reader's `missing <key>` and `<key> <rule>`, without an environment variable |

- Secrets stay in files the config points to: `telegram.token_file`, `telegram.stt_env_file`,
  `engine.web_env_file`, `engine.mcp.<name>.env_file`. The catalogue has no key for a secret
  value, so an inline secret is an unknown key, and its error carries no value.
- Environment: only `ULEI_CONFIG`. The engine's per-setting variables (`ULEI_ENGINE_*`,
  `ULEI_QB_SOCKET`, `ULEI_SECURITY_REDACT_SECRETS` and the item variables) are dropped: a value one
  process sees and another does not breaks the one file, and `ulei status` run from a shell
  would read other sockets than the units. Section 8 lists the reference behaviours this
  changes.

### 3. Sections, keys and readers

| Section | Keys | Read by |
|---|---|---|
| `[inbox]` | `root` (the inbox root, `events.sqlite3` and `producer.key` in it), `socket` (C1, at most 107 bytes), `deliver` (boolean, default false: the delivery loop of ADR-0036) | `root`, `deliver`: `ulei-inbox serve`. `socket`: `ulei-inbox serve`, `ulei status` |
| `[engine]` | `c6_socket` | `ulei-engine serve`, `ulei-tg`, `ulei-inbox serve` when `inbox.deliver` is true, `ulei status` |
| | `owner_session` | `ulei-engine serve`, `ulei status` |
| | `timezone` | `ulei-engine`, `ulei-tg` |
| | `journal` (the C8 session journal) | `ulei-engine` |
| | routes and aliases: `default_route`, `compression.route`, `no_ask_guard.route`, `[engine.routes.<alias>]` `context_length`, `max_tokens` | `ulei-engine` |
| | every other key of `crates/engine/src/config.rs`, names unchanged, `[engine.mcp.<name>]` and `[engine.mcp.<name>.tools]` included | `ulei-engine` |
| `[qb]` | `socket` (the qb gateway, C4G) | `ulei-engine`, `ulei status` |
| `[telegram]` | the keys of ADR-0032 section 5, names unchanged | `ulei-tg` |
| `[security]` | `redact_secrets` | `ulei-engine` |

`ulei-engine run` reads what `serve` reads but `c6_socket`, `owner_session` and
`default_route`. An `[engine.mcp.<name>]` section whose name `engine.mcp_servers` does not list
is an engine error naming the section.

Flags and variables:

| Today | Here |
|---|---|
| `ulei-inbox serve --root DIR` | `inbox.root` |
| `ulei-inbox serve --socket PATH` | `inbox.socket` |
| `ulei-inbox serve --deliver C6_SOCKET` | the switch to `inbox.deliver`, the socket to `engine.c6_socket` |
| `--deliver-tick-ms`, `--lease-ttl-ms`, `--retry-delay-ms` | stay flags of `serve`, allowed only with `inbox.deliver` true: they scale time for the kill test (ADR-0037 section 5), and a deployment file must not carry them |
| `ulei-inbox shadow --live-root --state --out` | stay flags: they name the Python root under comparison and the pass's own output, which no other binary reads, for one pass a timer starts |
| `ulei-engine run --session --route --prompt --new --channel --event` | stay flags: they describe one turn, not the deployment |
| `--config PATH` of `ulei-engine`, `ulei-tg`, and of `ulei-inbox serve` and `status` | stays: it finds the file |
| every `ULEI_*` setting variable of the engine | dropped; its key stays |

### 4. The copies

- `crates/channels/src/tg/toml.rs` and the parser of `crates/engine/src/config.rs` are deleted;
  both binaries read through `ulei_contracts::config`. The engine keeps its typed lookups
  (`path`, `timeout`, `limit`, `switch` and the rest) over the decoded values, without the
  environment argument.
- `crates/engine/src/env_file.rs` moves to `ulei_contracts::env_file`, its callers in the engine
  and in `ulei-tg` switch in the same change, and `crates/channels/src/env_file.rs` is deleted.
- issue:66737b71d2348a8fab06 and issue:b59e6aaff56f00343c18 are closed by cards 2 and 3 of
  section 9, which delete the two copies.

### 5. `ulei status`

`ulei status [--config PATH]` reads the four keys `inbox.socket`, `engine.c6_socket`,
`engine.owner_session` and `qb.socket`; one of them absent, or the file refused, is one stderr
line of section 2 and exit 2, with nothing probed. It probes the three daemons at once, one
thread each; the calls of one daemon run in order, and a daemon whose call fails gets no further
call. Each call has a 2 s connect bound and a 5 s bound from connect to the last answer byte,
so the command ends within 10 s. Every call answers at once on its server; none waits for a turn.

Output is text on stdout, one line per item in this order, times in UTC as
`YYYY-MM-DDTHH:MM:SSZ`, so no zone file is read. There is no JSON form.

| Line when up | Source | Line when down |
|---|---|---|
| `inbox: up <inbox.socket> (<version>)` | C1 `health` (`POST /v1/health`) | `inbox: down <inbox.socket>: <reason>` |
| `queue: pending <n>, leased <n>, processing <n>, deferred <n>, uncertain <n>` | C1 `counts` (C1 1.2.0) | `queue: unknown: <reason>`, `inbox down` when the inbox is |
| `engine: up <engine.c6_socket>` | C6 `is_idle` | `engine: down <engine.c6_socket>: <reason>` |
| `channel: up` | C6 `IdleStatus.channel` (C6 0.7.0) true | `channel: down: no channel holds inbox_turns`; `channel: unknown: the runtime does not report it` when the field is absent; `channel: unknown: engine down` |
| `session: <owner_session> idle since <time>, queue <n>, route <alias>`, `running since` while a turn runs | C6 `status` of `engine.owner_session`: `running`, `since`, `queue_depth`, `route` | `session: unknown: <reason>` |
| `last turn: <turn> <outcome> at <ended>`, then `: <line>` when the report has one; `last turn: <turn> ended, outcome not kept` after an engine restart; `last turn: none` | `SessionStatus.last_turn` of the same answer | `last turn: unknown: <reason>` |
| `qb: up <qb.socket> (<version>)` | C4G `GET /v1/health` | `qb: down <qb.socket>: <reason>` |
| `route <alias>: <state>`, then ` <vendor>/<model>` when given and `, resets <resets_at>` when given, one line per alias in qb's order | C4G `GET /v1/model/routes` (`RouteStatus`: `ready`, `degraded`, `exhausted`, `unavailable`) | `routes: unknown: <reason>` |

A reason is the failure of the call: the connect error (`connect: Connection refused`), `no
answer within 5 s`, the peer uid when the engine runs as another user, the status and error code
of a non-2xx answer, or `malformed answer`. A down daemon is a line, never a crash.

Exit status: 0 when `inbox`, `engine`, `channel` and `qb` read `up`, whatever the queue, the
session, the last turn and the routes show; 1 when one of them reads `down` or `unknown`; 2 for
a usage error or a config `ulei status` cannot use.

### 6. Place and the ADR-0007 budget

`ulei status ARGS` replaces the process with the sibling `ulei-inbox status ARGS`, as
`ulei inbox` does (ADR-0007 section 1): the sibling from the directory of the running `ulei`,
never from `PATH`, chosen on the first argument.

- `ulei`: one more first-argument branch, at most 4,096 B on its 410,296 B against 524,288 B.
  `--version` takes no new path, so its peak RSS, cold start and syscalls (at most 40, no file
  open) are unchanged. No limit is raised.
- Linking status into `ulei` is refused: it would bring `serde_json` and the parse of the C1, C6
  and C4G answers. The JSON, socket and HTTP path of the ADR-0007 prototype weighed 126,968 B
  over `ulei`, more than its 113,992 B of room.
- `ulei-inbox` already links `serde_json`, the HTTP/1.1 framing and the C6 client with `is_idle`
  and `status` (ADR-0036). `status` adds the decoder, a C1 and a C4G client over that framing
  and the text, estimated at +30 to +40 KB under its 2,621,440 B. It opens no database: SQLite
  stays linked for `serve` and `shadow` and is never opened by `status`.
- A status run, estimated: two `execve`, one file opened (`ulei.toml`), six connections of
  about ten syscalls each, three short threads: about 120 syscalls and under 2 MB peak RSS. It is
  not held to the `--version` limits; section 9 card 9 measures it.

Contract gaps:

- C1 1.2.0, additive: `x-methods` `counts`, `CountsRequest` (no arguments) and `Counts` with
  `pending`, `leased`, `processing`, `deferred` and `uncertain`, non-negative integers, all
  required. `ulei-inbox serve` answers it from one read, `SELECT status, COUNT(*) FROM events
  WHERE status != 'done' GROUP BY status`, a scan of the table: the schema is the Python text
  byte for byte (ADR-0007), so no index is added.
- C6 0.7.0, additive: optional `IdleStatus.channel`, true when a channel holds `inbox_turns` for
  the owner session or follows the inbox turn the session runs, which was handed to it. The
  engine always sends it; it stays optional so a runtime that does not know it stays valid.

### 7. The reference: start, stop and status of the gateway

| Reference behaviour (module) | Here | Why |
|---|---|---|
| The running gateway writes its state to `gateway_state.json`: `gateway_state` (`starting`, `running`, `degraded`, `draining`, `startup_failed`, `stopped`), `exit_reason`, `active_agents`, and per platform `platform_state` (`connecting`, `connected`, `retrying`, `fatal`, `paused`, `disabled`) with an error code and message (`run.py` `_update_runtime_status`, `_update_platform_runtime_status`, `_persist_active_agents`, through `gateway/status.py` `write_runtime_status`) | Not ported: `ulei status` asks each daemon over its socket when it runs; no daemon writes a status file | 0 write bytes and 0 wake-ups when idle (ADR-0001); a state file outlives a killed process, which the reference answers with a staleness limit and PID checks (`runtime_status_is_stale`, `runtime_status_pid_is_live`) |
| "Is the gateway up" by a ladder: PID file and runtime lock, then an HTTP health probe, then the PID of the runtime state (`gateway/status.py` `resolve_gateway_liveness`) | Ported in part: the health probe, as C1 `health`, C6 `is_idle` and C4G `GET /v1/health` | ulei writes no PID file and takes no runtime lock; the supervisor owns the processes |
| A platform that fails is marked `retrying` or `fatal` with its error while the gateway runs on; with no platform left the gateway stops with `exit_reason` "All messaging adapters disconnected" (`run.py` `_handle_adapter_fatal_error_impl`) | Ported as a status rule: a component that is down is one line with its reason and `ulei status` goes on; exit 1 | Each component is its own process; a daemon that fails exits 1 for its supervisor (ADR-0032 section 3, section 8 there) |
| The in-flight agent count `active_agents` (`run.py` `_persist_active_agents`) | Ported as the owner session's `running` and queue depth from C6 `status` | One engine serves one owner session (ADR-0036 section 11) |
| `/status` in chat: session key and title, model and provider, context use, token totals, running agent, queue depth, connected platforms (`gateway/slash_commands.py` `_handle_status_command`, dispatched from `run.py`) | Ported in part on the command line: session key, running or idle, queue depth, the route alias in place of model and provider (port map R1), the channel; not ported: title, context use, token totals | Those three are read from the session journal, which `ulei status` does not open (it opens no SQLite); a `/status` chat command is not part of this record |
| `exit_reason` and the `startup_failed` state kept for the next reader (`run.py` start path) | Not ported: the failed daemon's one stderr line is in the supervisor's journal; `ulei status` shows `down` with the connect error | No state file (first row) |
| `draining` while the gateway refuses new turns (`run.py` drain path) | Not ported | The engine has no drain (ADR-0028 section 8) |
| `--replace` stops a running gateway before start; the PID file and runtime lock refuse a second one (`start_gateway`, `acquire_gateway_runtime_lock`, `write_pid_file`) | Not ported | The supervisor runs one unit of each daemon; the inbox's single consumer is `consumer.lock` (ADR-0036 section 1) |
| Respawn-storm check, planned stop and restart markers, takeover marker (`gateway/status.py` `record_start_and_check_storm`, `write_planned_stop_marker`, `write_takeover_marker`, used by `run.py`) | Not ported | Restart policy is the supervisor's (systemd units, later work) |

### 8. Differences from the reference (settings)

| Reference (module) | Here | Why |
|---|---|---|
| Redaction is on by the environment variable the CLI sets from `security.redact_secrets` (`agent/redact.py` `_REDACT_ENABLED`, `hermes_cli/main.py`) | `security.redact_secrets`, a boolean in the file; no variable | One source of settings (section 2). |
| The concurrent tool deadline also comes from an environment variable (`agent/deadline.py` `resolve_timeout`, `agent/tool_executor.py` `_resolve_concurrent_tool_timeout`) | `engine.concurrent_tool_timeout` only | The same. |
| Switches read from strings (`1`, `true`, `yes`, `on`; for `agent.empty_response_guard.enabled` any string but `0`, `false`, `no`, `off`, `agent/empty_response_guard.py` `resolve_guard_settings`) | TOML booleans; a string is a kind error naming the key | The spellings came from environment strings, which are gone; the file is typed by C9. |

### 9. Implementation cards, in order

Each card merges alone with the workspace green, tests on fakes and synthetic files only, and
rewrites in place every document that describes what it changes.

1. C9 0.1.0 (about 120 min): the schema of section 1 with every key of `config.rs`, of
   ADR-0032 section 5 and of `[inbox]`; index, changelog and fixtures (valid: a full and a
   minimal document; invalid: an unknown section, an unknown key, a wrong kind, an inline
   `telegram.token`); `gen-contracts` writes `c9::KEYS`; `cargo xtask contracts` parses
   `docs/ulei.example.toml` with its `toml` and validates it against C9; the example with
   synthetic paths only; the short ADR with `Affects: contract:C9` the ADR gate needs. Tests:
   `xtask` unit tests of the catalogue walk. Budget: none, no shipped binary changes.
2. Decoder and env-file reader (about 100 min): `ulei_contracts::config` (section 2: the path
   order, the subset, the catalogue check, each error line) and `ulei_contracts::env_file` moved
   from the engine with its callers in the engine and `ulei-tg`; `crates/channels/src/env_file.rs`
   deleted; the env-file row of `THIRD_PARTY_NOTICES.md`, `crates/contracts/AGENTS.md`, the crate
   documentation and the root AGENTS.md layout row. Tests: synthetic TOML through the decoder for
   every error line and kind, the moved env-file tests. Budget: the `ulei-engine` and `ulei-tg`
   sizes of `ci/budgets.sh` with their delta. Closes issue:66737b71d2348a8fab06.
3. `ulei-tg` on the decoder (about 90 min): `tg/toml.rs` and its tests deleted, the settings over
   the decoded values, the path order. Tests: the settings tests on synthetic files, an unknown
   key refused. Budget: the `ulei-tg` section of `ci/budgets.sh` as today. Closes
   issue:b59e6aaff56f00343c18.
4. `ulei-engine` on the decoder (about 100 min): the parser of `config.rs` deleted, the lookups
   over the decoded values with the environment still winning, unknown keys and unlisted MCP
   sections refused. Tests: `crates/engine/tests/config.rs` on synthetic files. Budget: the
   `ulei-engine` section as today.
5. The engine without setting variables (about 120 min): every `ULEI_*` setting variable dropped
   (`Setting.env`, `ItemSetting::env`, `Error::Clash`), the error lines without them, switches as
   booleans; `crates/engine/tests/{cli,serve,lifecycle,mcp,config}.rs` and `ci/engine_idle.py`
   write a synthetic `ulei.toml`; ADR-0015, ADR-0016, ADR-0022, ADR-0023, ADR-0025, ADR-0026 and
   `docs/runbooks/engine-live-smoke.md` rewritten without them. Budget: the `ulei-engine`
   section as today.
6. `ulei-inbox serve` on the file (about 120 min): `serve [--config PATH]` with the test-only
   flags, keys `inbox.root`, `inbox.socket`, `inbox.deliver`, `engine.c6_socket`;
   `ci/inbox_load.py`, `ci/deliver_kill.py` and `ci/budgets.sh` write a synthetic config;
   ADR-0036 section 1, ADR-0037 section 5 and `docs/runbooks/inbox-shadow.md` rewritten. Tests:
   the serve tests through a temporary config, the test-only flags refused without
   `inbox.deliver`. Budget: the `ulei-inbox` section as today (ready, RSS, idle, publish p99).
7. C1 1.2.0 `counts` (about 60 min): schema, fixtures, changelog, generated types; the server
   answer. Tests: counts of a store holding rows of every status, `done` left out. Budget: the
   `ulei-inbox` section as today.
8. C6 0.7.0 `IdleStatus.channel` (about 60 min): schema, fixtures, changelog; the engine sets it.
   Tests: false with no hold, true while `ulei-tg`'s fake holds `inbox_turns` and while the
   handed-over inbox turn runs, false after it ends with no new hold. Budget: the `ulei-engine`
   idle section as today.
9. `ulei status` (about 120 min): `ulei status` forward, `ulei-inbox status`, the C1 and C4G calls
   over the core's HTTP/1.1 framing, the lines, reasons and exit codes of section 5, UTC times.
   Tests: against fake C1, C6 and C4G sockets, every line up, each daemon down (no socket,
   refused, never answers, a non-2xx answer, a malformed body), exit 0, 1 and 2, the missing
   sibling. Budget: the `ulei` section unchanged with its size, and a `ulei status` line in the
   `ulei-inbox` section against stdlib fake sockets: syscalls printed, 1 file open, peak RSS at
   most 4,096 kB.

## Consequences

- Every daemon refuses to start without the file or with an unknown key in it; settings come
  only from the file.
- `ulei-contracts` holds two hand-written decoders next to the generated types; a new key is a
  C9 change with its changelog entry.
- `ulei status` reaches every component only over its socket and stays usable while any of them
  is down.
