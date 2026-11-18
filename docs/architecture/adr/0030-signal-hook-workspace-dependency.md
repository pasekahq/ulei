# ADR-0030: `signal-hook` declared in the workspace dependencies

Status: accepted
Date: 2026-11-18
Affects: crates/engine, dep:signal-hook

## Context

ADR-0029 admits `signal-hook` 0.4 without default features for `crates/engine`. The engine
manifest declared its version and features itself, while every other shared crate of the
workspace (`serde_json`, `rusqlite`, `sha2`, `rustix`, `regex-lite`) is declared once in the
root `[workspace.dependencies]` and referenced with `workspace = true`.

## Decision

- The root `Cargo.toml` declares `signal-hook = { version = "0.4", default-features = false }`
  in `[workspace.dependencies]`.
- `crates/engine/Cargo.toml` takes it with `signal-hook.workspace = true`.
- The version, the features, the lock and the crates that use it are those of ADR-0029; a
  second crate that needs it takes the same declaration.

## Consequences

- One place pins the `signal-hook` version and features for the workspace.
- The built engine and its budgets (ADR-0010 section 4) are unchanged: the resolved crate and
  its features are the same.
