# ADR-0009: MVP slice, own agent loop, all model access only through the qb gateway

Status: accepted
Date: 2026-11-17
Affects: crates/engine, crates/runtime-adapters, lock:qb

## Context

ADR-0006 orders the strangler steps S0-S7, puts the tool loop on the official Agent SDK
engine (S6) and moves every switch after at least 14 days in shadow mode. The owner chose
instead a thin vertical slice "the owner's assistant lives on ulei" on an engine of our own,
next to the current runtime, which stays a hot standby. `docs/roadmap.md` describes the slice
and its sprints.

## Decision

### 1. Step order (amends ADR-0006)

The steps follow `docs/roadmap.md`:

| Step | What |
|---|---|
| U1 | agent loop against qb: `ulei-engine` with tools and a session journal, on a fake gateway |
| U2 | the assistant: compaction, prompt from memory files, skills, MCP client, C6 server |
| U3 | owner channel (Telegram) |
| U4 | inbox delivery and ops: claim, inject, ack; config, status, service units, runbook |
| Pilot | a separate test bot on the live host; the current runtime keeps the owner chat |
| U5 | parity fixes found in the pilot |
| Cutover | owner inbox and owner bot move by flag or `base_url`, with one-command rollback |

The pilot on a test bot replaces the 14-day shadow of the owner chat. The other ADR-0006
rules stay: switches are a flag or a `base_url`, no destructive migrations, the Hermes patch
series never grows, only `crates/runtime-adapters` knows about Hermes. Step S6 is dropped: no
vendor CLI or SDK is the engine.

### 2. Own synchronous loop

The engine is our own synchronous Rust agent loop in `crates/engine` (`ulei-engine`): tool
loop, tool calls, sessions, compaction, skills as files. It follows the conventions of
ADR-0001: no async runtime, no background threads or timers when idle.

### 3. Model access only through the qb gateway

- Every model call of ulei goes to the qb gateway over its contract and nowhere else. ulei
  knows exactly one provider: the qb gateway on a local Unix socket.
- Subscription OAuth logins, API keys, OpenRouter, account rotation, quotas, fallback chains
  and switching of provider, model or key live in qb and its config. ulei picks only a route
  alias per request.
- The gateway contract is drafted in `contracts/` until qb releases it; then it is vendored
  from qb and pinned in `ecosystem.lock`, as C4 is.
- ulei contains no vendor adapter, no vendor host, no vendor credential path and no vendor SDK
  crate. `cargo xtask arch` rejects vendor hosts in sources and `deny.toml` bans vendor SDK
  crates.
- Tests and CI use only a scripted fake gateway; they never reach a real gateway or any model
  vendor. The live smoke against a real gateway is an operator step, not a CI job.

## Consequences

- Anything the gateway lacks (a vendor, a login kind, a quota rule) is built in qb, not here.
- Until qb ships the gateway, the engine is developed and tested against the draft contract
  and the fake gateway only.
- The current runtime stays the owner's assistant until the cutover; rollback is switching the
  flag back.
