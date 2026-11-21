# ADR-0049: `hmac` and `sha2` in the garden crate for the MAC of `garden_miss`

Status: accepted
Date: 2026-11-21
Affects: crates/garden, dep:hmac, dep:sha2

## Context

ADR-0047 section 5 has the garden sign each miss it publishes as a C1 `garden_miss` packet: the
`mac` is the HMAC-SHA256 of the canonical JSON of the six other fields under the producer key,
as the inbox checks it (ADR-0007). Section 1 of ADR-0047 lists `hmac` and `sha2` as new
dependencies of the crate, and section 8 has the miss card carry the record the ADR gate needs.
The standard library has neither. The inbox's own code in `crates/core` cannot be shared across
the crate boundary (ADR-0001 section 3).

## Decision

- `crates/garden/Cargo.toml` takes `hmac.workspace = true` and `sha2.workspace = true`: the
  workspace declarations without default features, `hmac` 0.13.0 and `sha2` 0.11.1 in
  `Cargo.lock`, the versions `crates/core` signs and checks C1 packets with.
- `crate::miss` uses `Hmac<Sha256>` for the packet MAC alone; the key is read once at start
  and held as the HMAC state.

## Consequences

- The garden signs packets that the inbox verifies with the same primitives; the lock gains no
  new crate, since both are already resolved for the workspace.
- Both crates are MIT OR Apache-2.0, already admitted by `cargo deny`.
