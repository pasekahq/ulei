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
- qb 0.0.1 has no release artifacts and no OpenAPI file yet: the pin is a commit and C4 is
  transcribed from `crates/qb-proto`. When qb publishes a static binary with SHA256SUMS and
  an OpenAPI file, the pin gets `artifact`/`sha256` and CI starts the pinned binary on a
  temporary socket to replay `contracts/fixtures/c4`.
- `ulei-glue` maps qb onto ecosystem contracts (C4 client, `on_dead` -> C1 `quota_dead`).
  qb never learns about the ecosystem.
