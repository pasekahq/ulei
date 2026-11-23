# ADR-0064: `landlock` in `core` for the connector launcher

Status: accepted
Date: 2026-11-23
Affects: crates/core, dep:landlock

## Context

ADR-0056 section 5.2 restricts every scheduled connector run with Landlock in the launcher
`ulei-connectors exec`, and its card 5 builds it. ADR-0056 names the `landlock` crate and checks
its version and license in section 10. The workspace denies `unsafe`, and `rustix` has no Landlock
calls, so the system calls need a safe wrapper.

## Decision

- `ulei-core` depends on `landlock` 0.4.8 (MIT OR Apache-2.0) through the workspace declaration,
  without default features. It brings `enumflags2` and `thiserror` 2 into `Cargo.lock`; `libc`
  is already locked.
- The launcher reads the kernel's status through the crate's domain-less `RestrictSelf`, which
  restricts nothing and tells `ENOSYS`, `EOPNOTSUPP` and the ABI apart, so a refusal names the
  value seen (ADR-0056 section 5.6).
- The ruleset handles `AccessFs::from_all` of that ABI at `CompatLevel::HardRequirement`, adds the
  scopes `Signal` and `AbstractUnixSocket` from ABI 6, and restricts the launcher before it
  executes the connector.

## Consequences

- `Cargo.lock` gains `landlock`, `enumflags2`, `enumflags2_derive`, `thiserror` and
  `thiserror-impl`.
- The crate adds 32 KiB to the static musl `ulei-connectors` (713,512 B), well under ADR-0056
  section 8's 2,621,440 B.
- A kernel ABI newer than the crate's latest is restricted with the crate's latest; a crate update
  extends the handled rights.
