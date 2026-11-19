# Contracts changelog

Semantic versioning per contract (ADR-0005). A breaking change (removed field, definition,
enum value or operation; a field becoming required; a type or validation keyword changing;
`additionalProperties` becoming `false`) needs a major bump (minor while `0.x`), an entry
here and an ADR with `Affects: contract:<id>`. `cargo xtask contracts --base <rev>` enforces it.

## C1 1.1.1
- Description of the packet: the core consumes the inbox and delivers each event into the
  owner's session through C6 `inject`; the runtime only receives `inject` (ADR-0008).

## C1 1.1.0
- Inbox wire operations (additive; the packet is unchanged): `x-methods` `publish`, `claim`,
  `start`, `renew`, `retry`, `ack`, `get`, `health` with their request and response
  definitions, the stored `Row` (packet as a C1 object via `$ref: "#"`), `EventId`, `Lease`,
  `RowOutcome`, `Applied`, and one error body `Error` with `ErrorCode` (`x-error`). Semantics
  mirror the Python inbox; transport and status mapping: ADR-0007. Golden canonical-JSON,
  digest and HMAC vectors: `fixtures/c1/golden/`.

## C1 1.0.0
- Initial schema of the inbox event packet, byte-compatible with the packet the Python inbox
  stores today (version 1, seven fields, HMAC-SHA256). Kinds: union of the three Python copies
  (`weekly`, `task_result`, `task_blocked`, `provider_outage`, `provider_recovery`) and of the
  product shape (`series_news`, `gate_preview`, `gate_released`, `quota_dead`, `garden_miss`).
  The new kinds are accepted by the schema only; the Python copies accept them after S1.
  `source`, `owner_visible` and `priority` from the product shape are not fields yet (adding
  them would break the Python consumers, which reject unknown fields): planned for C1 2.0.0 in S1.

## C2 0.1.0
- Draft MCP facade of the core (product shape §2.3).

## C3 0.1.0
- Draft card/head/report contract derived from the current Hive.

## C4 0.0.1
- Vendored qb `/v1` API from vladmesh/quota-balancer@80b81c3 (qb 0.0.1).

## C4G 0.1.0
- Draft qb model gateway (ADR-0011): `POST /v1/model/turn` with a chunked NDJSON stream of
  `StreamEvent` ending in exactly one `stop` or `error`, `GET /v1/model/routes`,
  `POST /v1/model/cancel`; one error body `Error` for non-200 answers and the `error` event.
  Message blocks and stream events are flat unions keyed by `type`, per-variant fields
  enforced with `allOf` of `if`/`then`.

## C5 0.1.0
- Draft owner side of the A2A gate (preview, consent bound to SHA-256, released) and the
  application objects of wire profile 0.4.

## C6 0.4.0
- A turn's reply (ADR-0031): `turn_events` (`TurnEventsRequest`, `TurnEventsPage`) with
  `TurnEvent` and `TurnEventType`. A held answer gives the turn's events after a cursor: `start`,
  any `interim` assistant messages, at most one non-empty `final`, then `end` with the
  `TurnOutcome` and the `line` as is; a dropped turn has only `dropped`.
- `SubmitTurnRequest.context` (additive): context appended to the API copy of the prompt for
  that turn only, never journaled.

## C6 0.3.1
- Descriptions: `InterruptResponse.interrupted` is false for a turn an earlier `interrupt`
  already interrupted that is still unwinding; `SessionStatus.last_turn` after a runtime
  restart is the last turn of the journal, without `TurnReport.outcome` and `ended`.

## C6 0.3.0
- Engine-served session methods (ADR-0028): `status` (`StatusRequest`, `SessionStatus`),
  `submit_turn` (`SubmitTurnRequest`, `SubmitTurnResponse`, a FIFO queue of at most 32 turns
  per session; a full queue answers `accepted` false), `interrupt` (`InterruptRequest`,
  `InterruptResponse`) and `set_route` (`SetRouteRequest`, `RouteSet`), with `SessionKey`,
  `RouteAlias`, `TurnId`, `TurnState`, `TurnOutcome` and `TurnReport`.
- `InjectRequest.attempt` (required, breaking): the claim's C1 `Row.attempts`; idempotency
  is on `event_id` and `attempt` together, so a re-offered deferred event is a new turn.
- `InjectResponse.turn` (additive): the accepted turn, which `status` reports by its id until
  it ends with a `TurnOutcome`.
- Descriptions: `InjectRequest.lease` is data only; `IdleStatus` covers queued turns; the
  server is a runtime adapter or the engine and the client the core or another local caller.

## C6 0.2.0
- Transport and errors (additive; the method definitions are unchanged): `x-transport` states
  HTTP/1.1 with JSON bodies over a unix socket, `POST /v1/<name>`, the header and body limits,
  one request per connection and the peer uid check; one error body `Error` with `ErrorCode`
  (`x-error`). The adapter is the server and the core the client; status mapping: ADR-0008.

## C6 0.1.0
- Draft runtime adapter wire protocol: `inject`, `is_idle`, `skills_dir`, `mcp_register`,
  `raw_sessions`, `owner_channel`.

## C7 0.1.0
- Draft garden search and proposals.

## C8 0.4.0
- `Session.route` (additive, optional): the route alias C6 `set_route` stored for the session;
  absent means the engine's default route. A new session of the key starts without one.

## C8 0.3.0
- `Message.active` and `Message.compacted` (additive, optional, defaults `true` and `false`):
  a compaction archives a session's live messages as `active` false, `compacted` true with
  their content and `seq`, and appends its compacted set as the new live set after the highest
  `seq`, carrying the session's last turn. A turn sends only the live set, in `seq` order. A
  0.2.0 journal reads with every message live.

## C8 0.2.0
- `Session.system_prompt` (additive, optional): the system prompt the session's first turn
  built and stored, sent unchanged by every later turn and after a restart. A session without
  one, from a 0.1.0 journal or after `new_session`, gets it built and stored by its next turn.

## C8 0.1.0
- Draft records of the engine's session journal (ADR-0014): `Session` (id and session key),
  `Message` (a C4G `Message` in canonical form at its position in the session) and `Step`
  (request id, route and usage of one gateway request of a turn).
