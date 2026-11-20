# ADR-0003: qb stays a separate repository, pinned in ecosystem.lock

Status: accepted
Date: 2026-11-16
Affects: lock:qb, contract:C4

## Decision

qb (vladmesh/quota-balancer) remains its own repository with its own cargo workspace,
license (`Apache-2.0 OR MIT`), releases and CI. This repository pins it in `ecosystem.lock`
and vendors its API as `contracts/c4-qb.openapi.json`.

- The only advantage of a crate inside this workspace would be one atomic PR for C4. It is
  covered by vendoring at a pin plus a consumer test, and by a breaking-change check in qb.
- qb 0.0.1 has no release artifacts yet: the pin is a commit. qb publishes an OpenAPI file
  only for its model gateway, vendored byte for byte as C4G (ADR-0038); C4 is transcribed
  from `crates/qb-proto`. When qb publishes a static binary with SHA256SUMS, the pin gets
  `artifact`/`sha256` and CI starts the pinned binary on a temporary socket to replay
  `contracts/fixtures/c4`.
- CI builds `qb` from the pinned commit and runs the engine's gateway client against it: the
  job `qb-contract` reads `repo` and `commit` from `ecosystem.lock`, caches the binary per
  commit and runs `crates/engine/tests/qb_contract.rs` (ADR-0038 section 6).
- `ulei-glue` maps qb onto ecosystem contracts (C4 client, `on_dead` -> C1 `quota_dead`).
  qb never learns about the ecosystem.
