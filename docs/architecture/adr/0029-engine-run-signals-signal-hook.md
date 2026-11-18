# ADR-0029: signals of `ulei-engine run` through `signal-hook`

Status: accepted
Date: 2026-11-18
Affects: crates/engine, dep:signal-hook

## Context

A signal sent to `ulei-engine run` has to interrupt its turn as the C6 `interrupt` does
(ADR-0028 section 3, port map R4): the tool process groups killed as `_kill_process` does, the
gateway turn cancelled, the turn journaled. The reference does this in its one-shot mode
(`THIRD_PARTY_NOTICES.md#ref-agent`, `cli.py` `_signal_handler_q`): SIGINT, SIGTERM and SIGHUP
call `request_hard_interrupt` on the running agent. The engine forbids `unsafe`, std has no
signal API, and the signal functions of `rustix` (`runtime`) are `unsafe`. A handler needs a
crate.

## Decision

### 1. Dependency

| Crate | Features | Why |
|---|---|---|
| `signal-hook` 0.4 | none (`default-features = false`) | `low_level::pipe::register` installs the handler without `unsafe` at the call site; the handler only sends one byte on a socket, which is async-signal-safe. It brings `signal-hook-registry` 1.4; `libc` and `errno` are in the lock already. MIT OR Apache-2.0. |

Measured as ADR-0010 section 3 measures (`x86_64-unknown-linux-gnu`, the release profile,
stripped), with and without the registration and its reader thread: 2,290,360 B against
2,280,760 B, so the crates and the thread take 9,600 B. The musl size of `ulei-engine` is the
gate's budgets annotation (ADR-0010 section 4).

### 2. `run`

- Before the turn starts, SIGINT, SIGTERM and SIGHUP get one handler that writes to a Unix
  socket pair. A thread named `signals` blocks in `read` on the other end, so the handler adds
  no timer and no idle wake-up.
- The first of them fires the turn's interrupt handle: the running tool groups get SIGTERM and,
  after 1 s, SIGKILL, the local gateway stream ends, and the turn's thread cancels the gateway
  turn at qb as it unwinds. A first MCP connect that waits for a retry ends at once. The turn
  ends `interrupted` and is journaled with every message it has, the input included.
- The process then exits 130 with the stderr line `ulei-engine: the turn was interrupted`. No
  tool process group outlives it: each tool call returns once its group is gone, or 2 s after
  the SIGKILL.
- Later signals change nothing. A signal that arrives before the handler is registered, while
  the arguments and the config file are read, has its default action.

### 3. `serve`

`serve` registers no handler: SIGINT and SIGTERM end the process at once with their default
action. Running turns are not interrupted or journaled; their history stays as of the last
journaled turn, and `status` reads them `unknown` after a restart. The tool process groups are
not the engine's group: under systemd the default `KillMode=control-group` ends them with the
unit, while an engine started by hand leaves them running. ADR-0028 section 8 records the
difference from the reference gateway.

## Consequences

- The ADR-0028 section 8 rows on signals list the differences from `_signal_handler_q` and
  the reference gateway.
