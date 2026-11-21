# ADR-0048: `rustix` in the garden crate for the daemon's peer uid

Status: accepted
Date: 2026-11-21
Affects: crates/garden, dep:rustix

## Context

ADR-0047 section 2 decides the daemon `ulei-garden serve`: it binds the C7 socket in a directory
of its own uid and answers `403 forbidden` to every connection whose `SO_PEERCRED` uid differs
from its own, as `ulei-inbox` and `ulei-engine` do. The standard library reads neither the peer
credentials of a Unix socket nor the effective uid. Section 1 of ADR-0047 lists `rustix` as a new
dependency of the crate, and section 8 has the daemon's card carry the record the ADR gate needs.

## Decision

- `crates/garden/Cargo.toml` takes `rustix = { workspace = true, features = ["process"] }`: the
  workspace declaration (1.1.6 in `Cargo.lock`, no default features, `std` and `net`) plus
  `process` for `geteuid`, as `crates/engine` takes it.
- `net` gives `sockopt::socket_peercred`, the peer check of every connection; `process` gives
  the uid the daemon serves and owns its socket directory with.
- No other `rustix` module is used by the crate.

## Consequences

- The garden reads its peer and its own uid without `unsafe` and without `libc` calls of its
  own; the lock gains no new crate, since `rustix` 1.1.6 is already resolved for the workspace.
- `rustix` is Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT, already admitted by
  `cargo deny`.
