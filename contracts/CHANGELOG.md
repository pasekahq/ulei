# Contracts changelog

Semantic versioning per contract (ADR-0005). A breaking change (removed field, definition,
enum value or operation; a field becoming required; a type or validation keyword changing;
`additionalProperties` becoming `false`) needs a major bump (minor while `0.x`), an entry
here and an ADR with `Affects: contract:<id>`. `cargo xtask contracts --base <rev>` enforces it.

## C1 1.2.0
- Inbox wire operation `counts` (additive; the packet is unchanged): `CountsRequest` (no
  arguments) answered by `Counts`, the number of stored events with status `pending`, `leased`,
  `processing`, `deferred` and `uncertain`, each required and at least 0; `done` is not
  counted. The inbox answers it from one read query and changes no row (ADR-0040 section 6).

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

## C4 0.1.0
- Transcribed again at the qb pin vladmesh/quota-balancer@e4b36e2 (additive): optional
  `PickRequest.on_behalf_of` (1 to 128 bytes), optional `LeaseState.run_id` and
  `LeaseState.on_behalf_of`; `GET /v1/why/{lease_id}` (`WhyResponse`, `WindowCost`);
  `POST /v1/agent/next` (`AgentNextRequest`, `AgentJob`) and `POST /v1/agent/result`
  (`AgentResultRequest`, `ProbeOutcome`, answered by `ReportResponse`).

## C4 0.0.1
- Vendored qb `/v1` API from vladmesh/quota-balancer@80b81c3 (qb 0.0.1).

## C4G 1.0.0
- The draft is replaced by qb's own gateway contract (ADR-0038), vendored byte for byte
  from vladmesh/quota-balancer@e4b36e2 `docs/openapi-gateway.json` and pinned in
  `ecosystem.lock`; the version is qb's. Against the draft: `ErrorClass` adds `cancelled` and
  `no_route`; `Role` adds `system`; `Tool.native` (`NativeTool`); response objects (`RouteInfo`,
  `Thinking`, `ToolCall`, `Usage`, `Error`, `Attempt`, `RouteStatus`, `RoutesAnswer`,
  `CancelAnswer`) accept unknown fields while requests and `StreamEvent` stay closed;
  `CancelAnswer.cancelled` is always true and an unknown or ended turn is answered 404;
  `degraded` is reserved and not sent. New operations: `GET /v1/health`, `POST /v1/model/reload`,
  `GET /v1/model/spend`, `POST /v1/model/spend/reset`, `GET /v1/model/audit`,
  `GET /v1/model/usage`, `POST /v1/model/clients/{name}/rotate`,
  `GET` and `PUT /v1/model/config/routes`, `POST /v1/model/logins/{name}/key`,
  `.../oauth/start` and `.../oauth/code`, with their schemas.

## C4G 0.1.0
- Draft qb model gateway (ADR-0011): `POST /v1/model/turn` with a chunked NDJSON stream of
  `StreamEvent` ending in exactly one `stop` or `error`, `GET /v1/model/routes`,
  `POST /v1/model/cancel`; one error body `Error` for non-200 answers and the `error` event.
  Message blocks and stream events are flat unions keyed by `type`, per-variant fields
  enforced with `allOf` of `if`/`then`.

## C5 0.1.0
- Draft owner side of the A2A gate (preview, consent bound to SHA-256, released) and the
  application objects of wire profile 0.4.

## C6 0.7.0
- Optional `IdleStatus.channel` (ADR-0040 section 6): true while a channel holds `inbox_turns`
  for the owner session or follows the inbox turn of that hold that the session runs. The engine
  always sends it. `IdleStatus` is closed, so a `ulei-inbox` built before C6 0.7.0 refuses an
  `is_idle` answer that carries `channel`: `ulei-inbox` and `ulei-engine` deploy together for
  C6 0.7.0.

## C6 0.6.0
- Descriptions of the `inject` method, `InjectRequest` and `InjectRequest.attempt` (ADR-0037
  section 3; no shape change): `inject` is idempotent on `event_id`. The runtime keeps a record
  of every accepted event across its restarts and answers a recorded `event_id` with the
  recorded `session_id` and `turn` and starts nothing, unless the `attempt` is higher and the
  recorded turn's delivery was `silent`, which is a new delivery; a recorded turn the runtime
  no longer holds reads `unknown`.

## C6 0.5.0
- Breaking (ADR-0036 section 8): `InjectRequest.payload` is replaced by the required
  `InjectRequest.prompt` (at least 1 character), the model text the core builds from the event
  and the runtime runs unchanged as the event turn's prompt.
- `inbox_turns` (`InboxTurnsRequest`, `InboxTurns`): a held answer naming the owner session's
  oldest inbox turn not yet handed over; `report_delivery` (`DeliveryReport`, answered by `Ack`)
  with the `Delivery` of an ended inbox turn; `TurnReport.delivery`, allowed only on an `ended`
  turn.
- Descriptions of `IdleStatus` and `InjectRequest`: the owner session is free when no turn
  runs, none is queued and the runtime can deliver the turn's answer to the owner.

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

## C7 0.3.0
- The garden daemon's wire (additive, ADR-0047 sections 2, 3 and 6): `x-transport` (`POST
  /v1/<name>`, HTTP/1.1 and JSON on a unix socket, one request per connection, 1 MiB body,
  5 s deadline, peer uid check) and `x-error` with `Error` and `ErrorCode`; the method `health`
  (`HealthRequest`, `Health` with `version`, `dense` (`DenseState`), `dense_error`,
  `failed_proposes`, `unpublished_misses`). Optional `SearchResponse.lexical_only` and
  `PointersResponse.lexical_only`; optional `ProposeRequest.pin`; optional `Fact.pinned`,
  `Fact.proof_count` and `Fact.recheck_before_acting`. `DigestReason` gains
  `ambiguous_entity`; `DigestItem` gains optional `entity_candidates` (`EntityCandidate`);
  `ResolveRequest` gains optional `entity`.

## C7 0.2.0
- The garden's API (additive, ADR-0046): `x-methods` `search`, `pointers`, `propose`, `digest`,
  `resolve`. `Fact` gains optional `fact_id`, `kind`, `domain`, `source_kind`, `expires_at`.
  `ProposeRequest` gains optional `kind`, `domain`, `attribute`, `valid_to`, `source_kind`,
  `observed_at`, `confidence`, `ttl_kind`, `secret_ref` (`vault:<path>`), `private`; `ProposeResponse` gains
  optional `fact_id`, `reason` (`ProposalReason`) and `conflicts_with`. New definitions:
  `EntityKind`, `SourceKind`, `TtlKind`, `ProposalReason`, the passive tier `PointersRequest`,
  `Pointer`, `PointersResponse` (at most three pointers, no fact text), and the owner's digest
  `DigestRequest`, `DigestItem`, `DigestReason`, `DigestResponse`, `ResolveRequest`,
  `ResolveAction`.

## C7 0.1.0
- Draft garden search and proposals.

## C8 0.6.0
- `Step.request_id` gains a tag (breaking, ADR-0039): `s<session>-t<turn>-<step>-<tag>`, the
  tag 12 lowercase hex digits drawn at random for each run of a turn, so no two runs of a turn
  send the same id, whatever journal or process sent it. Steps of a 0.5.0 journal keep their
  untagged ids and stay valid.

## C8 0.5.0
- `InboxDelivery` (additive): the engine's durable record of an inbox event C6 `inject`
  accepted (ADR-0037 section 3), with the `event_id`, the highest accepted `attempt`, the
  `turn` it started and the `delivery` the channel reported for that turn. One record per
  event, never removed.

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

## C9 0.2.0
- The closed section `garden` of `ulei-garden serve` (additive, ADR-0047 section 4) with its
  closed subsections `garden.edge`, `garden.api`, `garden.embedder`, `garden.ttl` (one key per
  stored C7 `TtlKind`, none for `never_cache`) and `garden.schedule`.

## C9 0.1.0
- Draft catalogue of `ulei.toml` (ADR-0040): the sections `inbox`, `engine` (with
  `engine.routes.<alias>` and `engine.mcp.<name>`), `qb`, `telegram` and `security`, every
  key with its kind, every section closed, no key for a secret value. `cargo xtask
  gen-contracts` writes the key catalogue `c9::KEYS` and no types; the fixtures and
  `docs/ulei.example.toml` are TOML.
