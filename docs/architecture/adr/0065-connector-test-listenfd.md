# ADR-0065: `listenfd` in `connector-test` for the broker's fd

Status: accepted
Date: 2026-11-23
Affects: crates/connector-test, dep:listenfd

## Context

ADR-0056 section 5.3 hands a run's network broker to the connector as fd 3 named `broker`
through `OpenFile=` (sd_listen_fds(3)), and section 7 has the test connector reach its source
through it; card 6 builds both. The test connector is the pattern a connector writer copies. The
workspace denies `unsafe`, and neither std nor `rustix` owns an inherited fd by its number
without it: opening `/proc/self/fd/3` fails with `ENXIO` on a socket, and `pidfd_getfd` is
outside the unit's `@system-service` filter. Taking the fd needs a safe wrapper.

## Decision

- `ulei-connector-test` depends on `listenfd` 1.0.2 (Apache-2.0, in `deny.toml`'s allow list)
  through a workspace declaration without default features. On Linux it depends only on `libc`,
  already locked.
- The connector checks that `LISTEN_PID` names its own process (`listenfd` also accepts a missing
  one), finds `broker` in `LISTEN_FDNAMES`, and takes that fd with `ListenFd::take_custom` as an
  `AF_UNIX` `SOCK_STREAM` socket, which `listenfd` validates and marks close-on-exec.
- No other crate takes it; the runner and the launcher only pass the fd on.

## Consequences

- The crate holds no `unsafe`.
- `Cargo.lock` gains `listenfd` and its Windows-only dependencies `uuid`, `winapi`,
  `winapi-i686-pc-windows-gnu` and `winapi-x86_64-pc-windows-gnu`, with `futures-core`,
  `futures-task`, `futures-util` and `slab` that `uuid`'s `js-sys` pulls for wasm; none of them
  builds for the Linux targets of `deny.toml`'s graph.
- The test connector is no release budget row (ADR-0056 section 8).
