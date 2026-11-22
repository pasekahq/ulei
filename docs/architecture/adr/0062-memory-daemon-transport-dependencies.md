# ADR-0062: `rustix` and `serde_json` in the memory crate for the daemon's transport

Status: accepted
Date: 2026-11-22
Affects: crates/memory, dep:rustix, dep:serde_json

## Context

ADR-0051 section 2 decides the host and transport of the archive: `ulei-memory serve` binds a
Unix socket at mode 0600, checks the peer uid and answers C10 `x-methods` as JSON over HTTP/1.1,
as the garden does (ADR-0047 section 2). Its card 8 adds the daemon and the owner's command
`ulei-memory delete` and carries the record the ADR gate needs for the crate's new dependencies.
The standard library has no safe `SO_PEERCRED` or `geteuid`, and `unsafe` is denied.

## Decision

- `crates/memory/Cargo.toml` takes the workspace declaration `rustix` (1.x, `std` and `net`)
  with the `process` feature, as the garden takes it: `socket_peercred` for the peer uid check,
  `geteuid` for the daemon's own uid, `getuid` for the uid the `delete` command writes into the
  audit row.
- `serde_json`, which the crate already takes for the audit row (ADR-0060), also decodes the
  method inputs and encodes the outputs and `Error` bodies of C10.
- Standard base64 of `bytes_b64` is written in the crate; no base64 crate is added.

## Consequences

- The lock gains nothing: `rustix` and its `linux-raw-sys` are already resolved for the garden,
  the core and the engine.
- The budget section of `ulei-memory` (ADR-0051 section 7) measures the binary with these
  dependencies.
