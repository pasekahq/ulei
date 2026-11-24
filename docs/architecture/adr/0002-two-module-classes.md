# ADR-0002: Two module classes and the four-condition test

Status: accepted
Date: 2026-11-16
Affects: crates/core, crates/hive, crates/memory, crates/runtime-adapters, crates/channels, crates/glue
Amended by: ADR-0043 (the garden and its retrieval are the module `ulei-garden`; `ulei-memory` keeps the forest).
Amended by: ADR-0068 (the Python hive lives in `hive-py/` until the Rust hive of `crates/hive` replaces it; its owner-specific collectors stay in the profile pack).
Amended by: ADR-0072 (besides class-2 modules, `crates/` holds library crates, class `"lib"`: pure computation linked by named modules, not modules themselves).

## Decision

A module is a **class-1 product** only if all four conditions hold:

1. **Value without us**: someone without our agent, Hive or garden can use it end to end.
2. **Wire interface**: it exposes a protocol (HTTP/UDS/A2A/CLI with JSON), not a library import.
3. **Own life cycle**: own semver, releases, license, threat model, CI with budgets.
4. **No reverse dependency**: it never imports ecosystem code or knows its paths, names or Hermes.

Otherwise it is a **class-2 internal module** and lives in `crates/` of this repository.

| Module | Class | Where |
|---|---|---|
| qb (quota balancer) | 1 | vladmesh/quota-balancer, pinned in `ecosystem.lock` (ADR-0003) |
| A2A gate | 1, fails condition 4 today | vladmesh/agent-peer-protocol; split in step G (ADR-0004) |
| core, hive, memory (garden + retrieval + forest as one module), runtime-adapters, channels, glue | 2 | `crates/` |
| profile pack (personal watchers, backups, digests) | outside | stays in the private secretary repository |

A new class-1 candidate goes through the same test and gets an ADR. Class-1 code is never
edited here: only the pin and the vendored contract change.
