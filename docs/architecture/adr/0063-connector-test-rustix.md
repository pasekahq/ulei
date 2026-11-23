# ADR-0063: `rustix` in `connector-test` for the socket probes

Status: accepted
Date: 2026-11-23
Affects: crates/connector-test, dep:rustix

## Context

ADR-0056 sections 5.7 and 7 give the test connector probes that try `socket(AF_INET)` and
`socket(AF_UNIX)` with a connect to a given path inside a run's unit and report the errno, and
its card 4 builds them. ADR-0056 names `rustix` for this crate; ADR-0061 left it out until the
probes. The workspace denies `unsafe`, so the calls need a safe wrapper.

## Decision

- `ulei-connector-test` depends on `rustix` through the workspace declaration (`std`, `net`),
  already in `Cargo.lock` for `ulei-core`: `rustix::net::socket`, `connect` and
  `SocketAddrUnix`, and `rustix::io::Errno` for the reported value.
- The probes run only when the connector's argv names them (`--probe-inet`,
  `--probe-unix PATH`); each puts one item with the call's answer before the numbered items.

## Consequences

- `Cargo.lock` gains no package.
