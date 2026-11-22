# ADR-0061: The crate `connector-test`, the stdio test connector of C11

Status: accepted
Date: 2026-11-22
Affects: crates/connector-test, dep:ulei-contracts, dep:serde_json

## Context

ADR-0056 section 7 decides the test connector `ulei-connector-test` and its card 2 builds it as
the crate `crates/connector-test` without the probes and the broker. The ADR gate asks for an ADR
added with a new crate and its dependencies.

## Decision

- The workspace gains `crates/connector-test`, package and binary `ulei-connector-test`, class 2
  with role "source connector" and contracts `["C11"]`.
- It depends on `ulei-contracts` (the C11 types) and `serde_json` (their JSON lines), both
  workspace declarations already in `Cargo.lock`. `rustix`, which ADR-0056 names for this crate,
  comes with the probes and the broker that use it.
- The hash of the key file is SHA-256, written in the crate: ADR-0056 names no hash crate for it.

## Consequences

- `Cargo.lock` gains only the new package.
- The runner's tests (ADR-0056 cards 3 to 6 and 12) and the MCP example (card 10) run against
  this binary.
