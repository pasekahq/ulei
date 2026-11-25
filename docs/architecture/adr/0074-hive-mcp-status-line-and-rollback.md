# ADR-0074: A `hive-mcp` line in `ulei status`, and the rollback stops `ulei-hive-mcp`

Status: accepted
Date: 2026-11-26
Affects: crates/core, deploy/rollback.sh, ci/rollback_run.py, ci/units_run.py, docs/runbooks/mvp-pilot-cutover.md

## Context

ADR-0071 added `ulei-hive-mcp` and its user unit and decided in section 8 that `ulei status`
gains no line for it: the engine reaches the server at the start of every turn and names a failed
connect on its stderr. On the pilot host that is not enough: the owner reads `ulei status`, not
the engine's journal, and a server that is down shows only as a turn without the hive tools.

`deploy/rollback.sh` (runbook part 3) stops and disables `ulei-tg`, `ulei-inbox` and
`ulei-engine` and nothing else, so after a rollback `ulei-hive-mcp.service` still serves the
hive tools on loopback with nothing of ulei left to call them.

## Decision

### 1. The `hive-mcp` line

`ulei status` (ADR-0040 section 5) prints one more line, last, after the qb lines. It reads no
new key and no file besides `ulei.toml`:

- `hive_mcp.listen` when the config has the key, the address the server listens on;
- else `engine.mcp.hive.url`, the server the engine calls by the name `hive` (runbook item
  4.6), so a host whose `[hive_mcp]` lives in a config file of its own, named by the unit's
  drop-in in `ULEI_CONFIG`, still gets a probe. `localhost` in the URL is taken as `127.0.0.1`,
  so no resolver file is read and the status run keeps its one file open (ADR-0040 section 6).

The probe runs in a fourth thread, at once with the three daemons, with the same bounds: 2 s to
connect and 5 s from connect to the answer's status line. It is one `GET <path>` without a token
(`/mcp` for `hive_mcp.listen`, the URL's path for the engine's URL); the server checks `Host`,
then the bearer token, before anything else, so `401` proves it listens there and never needs
the token's file, which `ulei status` may not be able to read.

| Case | Line |
|---|---|
| No `hive_mcp.listen`, no `[hive_mcp]` section and no `engine.mcp.hive.url` | `hive-mcp: not configured` |
| `401` from `hive_mcp.listen` | `hive-mcp: up <listen>` |
| `401` from the engine's URL | `hive-mcp: up <host:port> (engine.mcp.hive.url)` |
| Any other answer or none | `hive-mcp: down <address>[ (engine.mcp.hive.url)]: <reason>` |
| A `[hive_mcp]` section without `listen`, a `listen` that is not a loopback address, a URL that is not `http://` on `127.0.0.1`, `[::1]` or `localhost` | `hive-mcp: unknown: <key> is missing`, `... is not a loopback address`, `... is not an http loopback URL` |

A reason is `connect: <error>` (`connect: Connection refused`), `no answer within 5 s`, `answered
<status>, not 401`, `socket: <error>` or `malformed answer`.

The line never decides the exit status: 0, 1 and 2 stay as ADR-0040 section 5 defines them. Many
installs do not run the hive tools, and the engine works without them.

### 2. The rollback

`deploy/rollback.sh` reads the `LoadState` of `ulei-hive-mcp.service` among its checks, before
any change; a failed query is a `check` failure. When it is `not-found` (a host where item 4.6
was never set up) the script prints `skipped ulei-hive-mcp.service: not installed` and stops and
disables the three units as before; else it stops and disables the four in one
`systemctl --user disable --now`. It still deletes nothing (the unit file, its drop-in, the
token and the env file stay), and a second run changes nothing more. `ci/rollback_run.py`
covers both cases and the failed query.

## Consequences

- ADR-0071 section 8's "`ulei status` gains no line" is replaced by section 1 above.
- Only `ulei-inbox` changes among the released binaries (`ulei status` runs it); `ulei` itself
  is unchanged. The size of `ulei-inbox` grows by the TCP probe, under its ADR-0007 limit.
- `ci/units_run.py` checks that `ulei status` reads `hive-mcp: up 127.0.0.1:<port>` against the
  started `ulei-hive-mcp.service`.
