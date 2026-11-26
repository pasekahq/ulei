# ADR-0071: `ulei-hive-mcp`, the hive's five typed tools for the engine over MCP

Status: accepted
Date: 2026-11-25
Amended by: ADR-0074 (a `hive-mcp` line in `ulei status`; the rollback stops `ulei-hive-mcp.service`), ADR-0082 (the timing flags of `hive_create`: reminders and series through the hive).
Affects: crates/hive-mcp, dep:ulei-contracts, dep:serde_json, contract:C2, contract:C9, deploy/systemd, deploy/install.sh, ci/release.sh, ci/install_run.py, ci/budgets.sh, ci/hive_mcp_budget.py, ci/units_run.py, docs/runbooks/mvp-pilot-cutover.md, .github/workflows/ci.yml

## Context

The secretary hands work to the task hive (`hive-py/`, ADR-0068) with `hive.py create` and
follows it with `list`, `show`, `steer` and `cancel`. In ulei the secretary is `ulei-engine`,
and on the pilot host it runs as the hardened system unit of ADR-0069, whose home is an
allow-list: the hive's CLI, its store, its config and its card workspaces are not visible to it,
and must stay so (a prompt injection must not read or rewrite the hive's store or another card's
workspace). The engine's tools are the built-in ones and MCP servers (ADR-0026: Streamable HTTP
on loopback, request headers from an env-file). Item 4.6 of the cutover runbook ("Delegation to
the hive and its return") therefore fails: blocker B2 of the go/no-go.

The owner's card asks for the hive tool "по C11". C11 (ADR-0056, ADR-0059) is the contract of a
*source connector*: a program the runner starts to pull items into the archive. A hive tool
pulls nothing; the engine calls it. C2 (draft 0.1.0) is the contract of the MCP tools the core
exposes to an agent runtime, and already has `hive_create`, `hive_show`, `hive_list` and
`hive_steer`.

## Decision

### 1. Reading of "по C11": C2 shapes, the C11 connector pattern for the packaging

- The tools' input and output shapes are C2: they are agent-facing MCP tools, which is what C2
  is for, and C11's shapes (`ulei_pull`, `Page`, `Item`) describe the archive feed only.
- The packaging and isolation follow the connector pattern of ADR-0056: its own binary and crate,
  its own secret by pointer (`token_file`, never a value in `ulei.toml`), its own hardened unit
  that writes only where it must, its config in `ulei.toml` through C9 and
  `ulei_contracts::config`. Unlike a connector it is a long-lived daemon (the engine connects at
  the start of every turn), so it is a user unit of its own, not a transient unit of the runner.

### 2. A typed MCP server, not the hive bound into the engine

The alternative was `BindPaths=` of `hive-py/`, the hive's config and its root into the engine's
unit plus a terminal command. That makes the hive's store and every card workspace readable and
writable by the engine's `terminal`, `read_file` and `write_file`: any card can be rewritten,
any report read, the dispatcher lock taken, `--script` cards created. A server outside the
sandbox that speaks five typed calls keeps the engine's view of the hive to what those calls
return, and needs no change to the engine (it is one more `engine.mcp_servers` entry) and none to
the engine's unit.

### 3. The crate and the hive CLI

- New crate `crates/hive-mcp`, package and binary `ulei-hive-mcp`, class 2, contracts C2 and C9.
  It depends on `ulei-contracts` (the generated C2 types, the C9 decoder) and `serde_json`, both
  workspace dependencies already in `Cargo.lock`; std only otherwise (no HTTP or MCP crate, no
  async runtime).
- It is not `crates/hive`: ADR-0068 section 1.3 keeps the Rust hive of step P6 free of anything
  of `hive-py/`. `ulei-hive-mcp` imports, links and ships nothing of `hive-py/`; it runs the
  hive's CLI, the interface every caller of the hive uses (the secretary today, the steward,
  the runbook), as a configured argv with `--json`, and reads its exit status and stdout. That is
  the hive's wire in the sense of hard rule 5: a process boundary with JSON out, no PyO3, no
  import. When P6 replaces the hive, only the argv and the mapping of section 5 change; C2 stays.
- Each call is one CLI run: an argv vector built from validated typed fields (never a shell),
  `HIVE_CONFIG` from `hive_mcp.hive_config`, an environment cleared to `PATH`, `HOME`, `LANG`,
  `XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS` as the unit has them (a cancel stops a head's
  transient unit through the user bus), stdin `/dev/null`, working directory `/`, a kill at
  `hive_mcp.timeout` (default 120 s; a cancel waits up to 60 s for a head to exit), stdout read
  up to 4 MiB and stderr up to 64 KiB, a run over either cap an error. Runs are serialised: one
  CLI run at a time; at most 4 connections are served at once and a fifth gets 503.

### 4. The contract (C2 0.2.0, C9 0.8.0)

C2 (breaking, minor bump while 0.x, ADR-0005):

- New tool `hive_cancel`: `HiveCancelInput` (`card_id`, a required `reason` of 1 to 1000
  characters, kept on the card) answered by `Ack`.
- `hive_create` answers the new `HiveCreateOutput` (`card_id`, `created`, `state`), which tells
  a repeated `idem_key` apart from a new card; `CardRef` stays the input of `hive_show`.
- `HiveCreateInput` holds what the secretary's delegation passes to `hive.py create` today:
  `title`, `goal`, `details` (`--body`), exactly one of `acceptance` and `accept_review` (the
  CLI's mutually exclusive group, a `oneOf`), `step` and `source` (both required, as in the
  CLI), `kind` (`--kind`: `owner_task`, `system` or `followup`; `series_run` belongs to
  schedules, which are not exposed; default `owner_task`, as the CLI otherwise refuses a source
  without a known prefix), `review`, `hold`, `owner_visible`, `idem_key`. `source` and
  `idem_key` match `^[A-Za-z0-9][A-Za-z0-9._:@/+-]{0,127}$`, so neither starts with `-`.
  Lengths are bounded (title 200, goal 8000, details 32000, acceptance 2000, accept_review 4000).
- Not exposed, and refused as unknown fields: `--created-by` (the server sets it from
  `hive_mcp.actor`), `--script`, `--script-timeout`, `--executor`, `--assignee`,
  `--on-unavailable`, `--model`, `--effort`, `--panel`, `--body-file`, `--deny-tools`,
  `--every`, `--cron`, `--tz`, `--misfire`, `--start-window`, `--max-runs`, `--until`,
  `--overlap`, `--deliver`, `--tag`, `--type`, `--priority`, `--gate-timeout`,
  `--not-before`, `--recheck-*`, `--blocked-by`, `--epic`, `--parent`, `--optional`,
  `--followup-of`, `--wait`. A script card would run arbitrary code as the hive's user; an
  executor, model or panel spends the owner's quota outside the ladder; a delivery or tag
  changes where results go; a path names something the engine cannot see.
- `CardStatus` gains `title` and `report_line` (the first line of the last head report) and
  bounds `result_line` (the first line of the card's result: its outcome, block or cancel reason)
  to 500 characters; `HiveListInput.state` is an enum of the hive's states; card ids carry the
  pattern `^h_[0-9a-f]{8}$` everywhere; `HiveSteerInput.note` is 1 to 4000 characters.

C9 (additive): section `[hive_mcp]` with `listen`, `token_file`, `hive_command`, `hive_config`,
`actor` (default `secretary`), `timeout` (default 120) and `hold` (default false). The TOML
subset has no arrays, so `hive_command` is split on whitespace without quoting, and its first
word must be an absolute path.

`tools/list` serves the C2 input schemas; `HiveCreateInput` goes without its top-level `oneOf`,
which model tool schemas do not take, and the server enforces it. A unit test compares every
served schema with `contracts/c2-mcp-tools.json`.

### 5. Mapping to the CLI

Values go as `--flag=value` and positionals after `--`, so no value is read as an option:

| Tool | argv after `hive_command` | Answer |
|---|---|---|
| `hive_create` | `create --json --title=.. --goal=.. (--acceptance=.. \| --accept-review=..) --step=N --source=.. --created-by=<actor> --kind=.. [--body=..] [--review] [--hold] [--owner-visible \| --no-owner-visible] [--idem=..]` | `HiveCreateOutput` from the JSON line |
| `hive_list` | `list --json [--state=S]` | `HiveListOutput`, the first `limit` (default 20, at most 200) rows |
| `hive_show` | `show --json -- <id>` | `CardStatus`; exit 1 `no such card` is the tool error `no such card <id>` |
| `hive_steer` | `steer --by=<actor> -- <id> <note>` | `Ack` with the CLI's line (whether and how it was delivered) |
| `hive_cancel` | `cancel --reason=.. --by=<actor> -- <id>` | `Ack` with the CLI's line (`cancelled` or `already cancelled`) |

`--hold` is added to every create when `hive_mcp.hold` is true. The server mirrors the CLI's
cheap checks before it runs anything, so the model gets a typed error rather than a traceback:
blank title, goal, acceptance or criteria; an acceptance that is only a vague word (`done`, `ok`,
`-`, ...); `test -s REPORT.md` without `review`; a NUL in any string. Everything else the CLI
decides (`bash -n` of the acceptance among it): its `hive: card rejected: <text>`, `hive: steer
rejected: ...` or `hive: <text>` is the tool error `<text>` (`isError`). Any other failure is the
tool error `the hive CLI failed (exit N): <its last stderr line, at most 300 characters>`; a kill
at the timeout is `the hive CLI did not finish within N s`.

The calls work without a dispatcher: a card stays `created` or `queued`, `hive_steer` on it
answers that the note is not delivered yet and the next head gets it, `hive_cancel` cancels it.

### 6. The MCP server

MCP Streamable HTTP (2025-11-25 transport page), one endpoint `/mcp` on `hive_mcp.listen`, which
must be a loopback IP address and port; it matches the engine's client of ADR-0026 exactly:

- `POST` of one JSON-RPC message (no batch). `initialize` answers the client's version when it is
  one of `2024-11-05`, `2025-03-26`, `2025-06-18`, `2025-11-25` (the engine's handshake
  versions), else `2025-11-25`, with capability `tools` only and a fresh `Mcp-Session-Id` of 32
  hex digits from `/dev/urandom`; at most 32 sessions are kept, the oldest dropped. Every other
  request needs a known session (none: 400; unknown: 404, which the engine reads as a lost
  session and reconnects). An `MCP-Protocol-Version` header outside the four is 400. A
  notification or response is 202 without body. Methods: `ping`, `tools/list`, `tools/call`;
  others answer JSON-RPC `-32601`. Answers are `application/json` with `Content-Length` and
  `Connection: close`.
- `tools/call` answers `structuredContent` (the C2 output) and one text item with the same JSON,
  or `isError: true` with one text item for a refused input or a failed call (MCP's tool
  execution errors, which the model sees); an unknown tool is JSON-RPC `-32602`.
- `DELETE` ends a session (204; unknown 404). `GET`, `HEAD` and other methods are 405 with
  `Allow: POST, DELETE`: no standing event stream; the engine's preflight reads 405 as "not a web
  page" and goes on.
- `Origin`: absent passes (the engine sends none); present, it must be an `http` or `https`
  origin on `localhost`, `127.0.0.1` or `[::1]`, else 403, as the spec requires against DNS
  rebinding. `Host` must name one of those hosts too, else 403.
- Every request but those refused for `Host` or `Origin` needs `Authorization: Bearer <token>`,
  compared in constant time, else 401 with `WWW-Authenticate: Bearer`. The token is the trimmed
  content of `token_file`: at least 32 visible ASCII characters, the file open to no group or
  other user (a wider mode refuses the start). It is never logged; the server's stderr carries
  one line per call with the tool, the card id, the outcome and the time, never a field value.
- A request head is at most 16 KiB and a body 1 MiB, each read within 10 s. Idle, the server
  blocks in `accept`: no timer, no wake-up (ADR-0010).

`ulei-hive-mcp serve [--config PATH]` writes `ulei-hive-mcp: ready: <address>` to stderr once
listening; a refused config exits 2 (the unit's `RestartPreventExitStatus=2`, as ADR-0069 does
for the engine).

### 7. Threat model

What the server is, honestly: `hive_create` is a deliberate, audited door out of the engine's
sandbox, the same door the secretary uses today, not a sealed boundary. A card's goal is carried
out by a hive head, an agent with tools that runs as the hive's user and is not sandboxed the way
the engine is; a command `acceptance` is bash the hive's gate runs as the hive's user in the card
workspace. So whoever can make the engine call `hive_create` can have work done outside the
engine's sandbox, under the hive's own rules (ladder, budgets, review, the owner's board).

What the server guarantees:

- Only the five typed calls. No field names a script, an executor, a model, a panel, a cadence, a
  delivery, a tag or a path; nothing reaches a shell; an option-like value cannot become a flag.
- `created_by` is `hive_mcp.actor`, never an input, so every card the engine makes is
  attributable on the board; `steer` and `cancel` carry the same actor.
- Bounded sizes of every input and output, one CLI run at a time with a timeout.
- `hive_mcp.hold = true` makes every engine card a `--hold` card that waits for the owner's
  `hive.py release`; default false. The owner decides at cutover which mode runs (runbook 4.6).
- The bearer token lives in an env-file inside the engine's writable state directory, so the
  engine's own `terminal` can read it. That gives a prompt injection nothing beyond the same five
  typed calls over loopback. Other local users are kept out by the token (and the file modes),
  web pages by the `Origin` and `Host` checks, other hosts by the loopback bind.
- The engine never gets a path of the hive: not the CLI, not the store, not the config.

`source` is also what the hive's admission reads (`urgent_sources`, `priority_sources`); the
engine can name any source the pattern allows, as the secretary can today.

### 8. Deployment and limits

- `deploy/systemd/ulei-hive-mcp.service`: a user unit in the layout of ADR-0042 (restart on
  failure, `RestartPreventExitStatus=2`, `NoNewPrivileges`, the `@system-service` syscall filter,
  `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`, `MemoryMax=256M`, `TasksMax=64`,
  `ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateTmp`), writable only in the hive's
  root named by a drop-in `ReadWritePaths=` the host writes (the hive's store, its lock,
  workspaces and results; the unit's default is `%h/.local/share/hive`). On a host of
  ADR-0069 case (b) the namespace options are inert, as for `ulei-inbox` and `ulei-tg`; the
  server's guarantees of section 7 do not rest on them.
- Released as `ulei-hive-mcp` in `ci/release.sh`, `SHA256SUMS` and `deploy/install.sh`; the
  `systemd-units` job checks its exposure (5.8, the threshold of the other units), verifies it,
  and `ci/units_run.py` starts it and calls `initialize` and `hive_list` through it with the fake
  hive CLI writing in its state directory.
- Budgets in `ci/budgets.sh` (ADR-0001): static binary at most 1 MiB, VmHWM after the calls of
  `ci/hive_mcp_budget.py` at most 2048 kB (the server's own, not the hive CLI's), 0 voluntary
  context switches in 10 s idle, median spawn to ready at most 10 ms.
- `ulei status` gains no line: the server is reached by the engine at the start of every turn,
  and the engine's stderr names a failed connect. Amended by ADR-0074: `ulei status` prints a
  `hive-mcp` line, last, from an unauthenticated probe of `hive_mcp.listen` (else
  `engine.mcp.hive.url`) answered `401`, `hive-mcp: not configured` when neither is set; the line
  never changes the exit status.

## Consequences

- The engine registers `mcp__hive__hive_create`, `mcp__hive__hive_list`, `mcp__hive__hive_show`,
  `mcp__hive__hive_steer` and `mcp__hive__hive_cancel` when `engine.mcp_servers` lists `hive`.
- The hive CLI's argv and its JSON fields are now a dependency of ulei; a change to them in a
  carry from the secretary repository must keep `crates/hive-mcp/tests/real_hive.rs` green.
- `Cargo.lock` gains only the new package.
