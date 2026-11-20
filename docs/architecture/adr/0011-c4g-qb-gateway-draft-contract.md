# ADR-0011: C4G, the contract of the qb model gateway

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

- The gateway contract is a separate index entry `C4G`: `kind: openapi`, file
  `contracts/c4g-qb-gateway.openapi.json`. It is qb's own file, vendored at the qb pin
  (ADR-0038); `C4` stays qbd's `/v1` API.
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

### 3. Replacement by qb's contract

qb publishes the gateway contract, and ADR-0038 vendors it under the id `C4G` at the qb pin of
`ecosystem.lock`, in place of the draft. qb keeps the flat unions of section 2.

## Consequences

- The engine, its qb client and the fake gateway are typed against `ulei_contracts::generated::c4g`.
- C4G changes only with the qb pin, as qb versions it, checked by `cargo xtask contracts
  --base`.
