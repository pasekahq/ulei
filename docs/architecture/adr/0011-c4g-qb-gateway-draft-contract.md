# ADR-0011: C4G, the draft contract of the qb model gateway

Status: accepted
Date: 2026-11-17
Affects: contract:C4G, crates/engine

## Context

ADR-0009 sends every model call of ulei to the qb gateway and drafts the gateway contract in
`contracts/` until qb releases it. The engine, its qb client and the fake gateway need one
checked document to be typed against. qb has not published the gateway yet; the wire shape is
ulei's proposal to qb, and qb fixes the final form in its gateway step (G1).

The existing `C4` is qb's vendored `/v1` API (pick, report, status), pinned in
`ecosystem.lock`. It changes only together with that pin.

## Decision

### 1. Id and placement

- The gateway draft is a separate index entry `C4G`: `kind: openapi`, `status: draft`, file
  `contracts/c4g-qb-gateway.openapi.json`, `source` naming the proposal issue to qb. `C4` and
  its pin stay untouched.
- `crates/engine` declares the contracts `C4G` and `C6` (amends ADR-0010 section 1): the engine
  talks to the gateway, not to qb's pick/report API.

### 2. Flat unions

The codegen subset has no `oneOf`/`anyOf`. Message blocks (`Block`) and stream events
(`StreamEvent`) are each one object with a `type` enum and optional per-variant fields. The
schema enforces the fields each variant requires with `allOf` of `if`/`then`; the generated
types ignore those keywords, so producers and consumers check the variant fields themselves.

- `Block` carries the variant fields directly (`text`, `media_type`, `path`, `base64`, `id`,
  `name`, `input`, `content`, `is_error`, `vendor`, `opaque`).
- `StreamEvent` carries `type` and exactly one payload field (`maxProperties: 2`): `route`,
  `text`, `thinking`, `tool_call`, `usage`, `reason` or `error`. Nesting the object payloads
  keeps the field names of the proposal although `usage.input` (a count) and
  `tool_call.input` (tool arguments) differ in type, and makes the `error` payload the same
  `Error` object that every non-200 answer carries as its body.

### 3. Replacement when qb releases the gateway

When qb ships the gateway contract, it is vendored from qb into `contracts/` and pinned in
`ecosystem.lock` like C4, in one change with an ADR (`Affects: contract:C4G` and `lock:qb`).
That change replaces the draft entry: the vendored file takes the `C4G` id or a new id, the
draft file and its fixtures are removed, and the engine metadata and generated types follow.

## Consequences

- The engine, its qb client and the fake gateway are typed against `ulei_contracts::generated::c4g`.
- Differences qb introduces in G1 are breaking changes of a `0.x` draft: a minor bump, a
  CHANGELOG entry and an ADR, checked by `cargo xtask contracts --base`.
