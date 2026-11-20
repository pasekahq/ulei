# ADR-0038: C4G vendored from qb at the pin, C4 at the same pin (C4G 1.0.0, C4 0.1.0)

Status: accepted
Date: 2026-11-20
Affects: contract:C4G, contract:C4, lock:qb, crates/engine

## Context

ADR-0011 drafted C4G, the qb model gateway, until qb publishes the gateway contract, and its
section 3 says how the draft is replaced. qb now publishes it as `docs/openapi-gateway.json`,
with its prose contract in `docs/api.md`. ADR-0003 pins qb in `ecosystem.lock` and vendors its
`/v1` API as C4; one change bumps the pin together with every contract vendored from it.

Reference: there is no reference. The gateway contract is qb's own and is not ported from the
reference agent (`THIRD_PARTY_NOTICES.md#ref-agent`); the port map,
`/home/dev/ummanu-data/instance/state/knowledge/projects/ulei/hermes-port-map-u2b.md`, has no
entry for it.

## Decision

### 1. Pin

`ecosystem.lock` `[products.qb]` pins `commit = "e4b36e2155ae031cee3fdb1e76645fdaab6351cf"`.
The vendored sources at that commit:

| File at the pin | sha256 |
|---|---|
| `docs/openapi-gateway.json` | `516b5c59f97ffbf553deed2613884b1a69ad710b3d35a70688c716adaa222c50` |
| `docs/api.md` | `c4fa06ee4c519ca83941cea16ad3631d234b87ef9a7a73120430d099b02a733b` |

The entry lists both contracts it vendors: `contracts = ["C4", "C4G"]` and `vendored` with
their two files. `artifact` and `sha256` stay empty until qb publishes release binaries.

### 2. C4G

- The id stays `C4G` and the generated module stays `ulei_contracts::generated::c4g`. qb titles
  the document "qb model gateway (C4)", but ulei's `C4` is qbd's `/v1` API, so the gateway keeps
  its own id.
- `contracts/c4g-qb-gateway.openapi.json` is qb's `docs/openapi-gateway.json` at the pin, byte
  for byte; the draft's content is gone. `contracts/index.json` marks it `vendored`, with qb's
  version `1.0.0` and the pin as `source`.
- The fixtures in `contracts/fixtures/c4g` follow the vendored form. Draft-only content is
  rewritten: route states that send `degraded` or name a vendor for a ladder route, a
  `quota_exhausted` end that is not retryable, an image by `path` (part of the shape, refused by
  qb), and vendor names that are not qb vendor names. They add the classes `no_route` and
  `cancelled`, `system` messages, a `native` tool, a nullable `backup`, and responses with a field
  the contract does not declare.
- The changes against the draft are the rows "take qb" of the reconciliation of C4G 0.1.0 with
  qb (board issue `issue:82473f499a25815f897f`). The engine effects N7, N13 and I2 of that
  reconciliation are not part of this change.

### 3. Codegen

- A generated struct denies unknown fields exactly where its schema says
  `additionalProperties: false`. The vendored C4G closes its requests (`TurnRequest`,
  `Message`, `Block`, `Tool`, `NativeTool`, `CancelRequest` and the admin requests) and
  `StreamEvent`, and leaves its responses open, so `RouteInfo`, `Thinking`, `ToolCall`,
  `Usage`, `Error`, `Attempt`, `RouteStatus`, `RoutesAnswer` and `CancelAnswer` parse with a
  field qb adds later. The codegen adds no rule of its own over a vendored file.
- A required property typed `[<type>, "null"]` becomes `Option<type>` that serializes `null`;
  an optional nullable property is a generation error.
- A fixture case named `extra_*` carries a field its definition does not declare. The
  generated fixture test parses a valid one and checks that it serializes back without that
  field and with every other, and checks that an invalid one is refused by its type.

### 4. C4 at the pin

qb publishes no OpenAPI file for qbd's `/v1` API, so C4 stays a transcription of
`crates/qb-proto/src/lib.rs` and `docs/api.md` at the pin. Between the old pin `80b81c3` and the
new one the API only grew, and C4 0.1.0 follows it: `PickRequest.on_behalf_of`,
`LeaseState.run_id` and `on_behalf_of`, `GET /v1/why/{lease_id}`, `POST /v1/agent/next` and
`POST /v1/agent/result` with their types.

### 5. Engine

- Every `ErrorClass` of the vendored contract parses. A `no_route` end, as an `error` event or
  as a non-200 body, ends the turn `failed` with a line naming the class.
- The compaction maps every class explicitly: `auth`, `quota_exhausted` and `overloaded` keep
  their causes, and `unavailable`, `bad_request`, `context_too_long`, `cancelled` and `no_route`
  are "other", so a new class of a later pin fails the build until it is mapped.
- Every fake gateway of the Rust tests answers in the vendored form only, from the generated
  types: `cancel` answers `{"cancelled": true}` for a running turn, whose held-open stream then
  ends with a `cancelled` error event, and 404 `bad_request` otherwise, an unknown path 404
  `bad_request`. The engine's fake builds an error of a class with qb's `retryable` for it:
  true for `quota_exhausted`, `overloaded` and `unavailable`.

### 6. Contract test against the pinned qb

- The CI job `qb-contract` reads `repo` and `commit` of `[products.qb]` from `ecosystem.lock`,
  fetches that commit from GitHub and builds `qb` as qb's acceptance lane does: its own release
  profile, static for `x86_64-unknown-linux-musl`. The binary is cached under the key
  `qb-<commit>-x86_64-unknown-linux-musl`, so a run on an unchanged pin neither fetches nor
  builds qb. The job runs on every pull request and push, beside the other jobs.
- `crates/engine/tests/qb_contract.rs` starts `qbd` and `qb gateway` of the binary named by
  `ULEI_QB_BIN` on sockets in a fresh temporary directory, with synthetic logins, keys and
  paths. qbd has `login-1` of `vendor-a` and `login-2` of `vendor-b`; the gateway holds both as
  Anthropic API keys with `base_url = "http://127.0.0.1:443"`, sets `max_request_bytes` to
  65536, and serves `chat`, a pin of `vendor-a`, and `chain`, a chain of one step on
  `vendor-b`. A fake vendor on 127.0.0.1:443 answers in the Messages API wire form: event
  streams, error bodies and rate-limit headers. Port 443 is the only one the gateway's sandbox
  connects to, so the job sets `net.ipv4.ip_unprivileged_port_start=443`.
- The qb cases are ignored in the ordinary test run; the job runs them with `--ignored`. A
  missing binary, or one that does not start, fails them.
- The cases drive the engine's `gateway::Client` and assert what it yields. A text turn and a
  tool-call turn: the events, the `route` of `login-1`, the usage, the stop reason, and tool ids
  and input unchanged in both directions. A cancel through `CancelHandle::cancel`: the stream
  ends `Cancelled`, qb answers `cancelled: true` or 404 within `CANCEL_TIMEOUT`, and the vendor
  sees its request closed. And these failures, each with qb's class, `retryable`,
  `retry_after` and `resets_at`:

| Class | Induced by | The engine sees |
|---|---|---|
| `quota_exhausted` | the vendor's 429 with unified status `rejected` and a reset | `error` event, retryable, `resets_at` the reset |
| `auth` | the vendor's 401 `authentication_error` | `error` event, not retryable |
| `no_route` | the next turn, with the only login of the route dead | `error` event, not retryable |
| `context_too_long` | the vendor's 400 "prompt is too long" | `error` event, not retryable |
| `overloaded` | the vendor's 529 with `retry-after: 5` on `chat` | `error` event, retryable, no `retry_after`: the re-pick that avoids the vendor finds none |
| `overloaded` | the same on `chain` | `error` event, retryable, `retry_after` 5 |
| `overloaded` | a second turn while `max_turns = 1` runs one | 429, retryable, `retry_after` 1 |
| `unavailable` | the vendor's 500 with an HTML body | `error` event, retryable |
| `cancelled` | `POST /v1/model/cancel` from outside the engine's client | `error` event after the turn's content, not retryable |
| `bad_request` | an unknown route alias | 400, not retryable |
| `bad_request` | a body of 8 MiB, refused from its head while the engine still writes | 413, not retryable |

- The class qb gives a caller that is not allowed, a 401 `auth` rejection, is not induced: the
  test runs as the gateway's own uid.
- The same file runs each of those failures through the engine's fake gateway and expects the
  same classification.

## Consequences

- A later pin bumps `commit`, copies qb's gateway file again and takes qb's version; `cargo
  xtask contracts --base` judges the change against the previous pin.
- Fields qb adds to its responses reach the engine as ignored fields, not as malformed answers.
