# ADR-0066: `signal-hook` in `core` for `ulei-connectors serve`

Status: accepted
Date: 2026-11-23
Affects: crates/core, dep:signal-hook

## Context

ADR-0056 section 4 has `ulei-connectors serve` stop its running units with
`systemctl --user stop` on SIGTERM or SIGINT, write their run records and exit 0; its card 7
builds it. The workspace denies `unsafe`, std has no signal API, and the signal functions of
`rustix` are `unsafe`, so a handler needs a crate. ADR-0029 admitted `signal-hook` for the engine
and ADR-0032 for `ulei-tg` with the same pattern.

## Decision

- `ulei-core` depends on `signal-hook` through the workspace declaration of ADR-0030 (0.4,
  without default features). Nothing new enters `Cargo.lock`.
- `serve` registers SIGTERM and SIGINT before its first run with
  `low_level::pipe::register` on a Unix socket pair; a thread named `signals` sleeps in `read`
  on the other end and wakes the schedule, so the handler adds no timer and no idle wake-up.
- `list`, `run`, `reset` and `exec` register nothing; a signal ends them with its default action.
- The same crate enables `rustix`'s `event` feature for `poll`: a broker's connect waits on its
  socket and on a socket pair the run shuts down when it ends, so a connect in flight ends with
  its run (ADR-0056 section 5.3). It adds no crate.

## Consequences

- The size of the static musl `ulei-connectors` is the card's report; ADR-0056 section 8 bounds
  it at 2,621,440 B.
