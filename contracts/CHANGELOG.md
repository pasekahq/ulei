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

## C4 0.2.0
- Transcribed again at the qb pin pasekahq/quota-balancer@bd174f4 (additive): optional
  `PickRequest.logins`, an allow-list of 1 to 32 distinct login names; `GET /v1/roles`
  (`RolesResponse`); the admin endpoints `GET` and `PUT /v1/admin/roles` (`AdminRoles` with an
  `ETag`, `AdminRolesPut` with `If-Match`, `AdminRolesWritten`) and `POST /v1/admin/reload`
  (`AdminReloadAnswer`), with `RolesSource`, `RoleConfig`, `LadderStep`, `LadderCandidate` and
  `RoleReserve`; `ErrorCode` adds `precondition_failed` (412) and `precondition_required` (428).

## C4 0.1.0
- Transcribed again at the qb pin vladmesh/quota-balancer@e4b36e2 (additive): optional
  `PickRequest.on_behalf_of` (1 to 128 bytes), optional `LeaseState.run_id` and
  `LeaseState.on_behalf_of`; `GET /v1/why/{lease_id}` (`WhyResponse`, `WindowCost`);
  `POST /v1/agent/next` (`AgentNextRequest`, `AgentJob`) and `POST /v1/agent/result`
  (`AgentResultRequest`, `ProbeOutcome`, answered by `ReportResponse`).

## C4 0.0.1
- Vendored qb `/v1` API from vladmesh/quota-balancer@80b81c3 (qb 0.0.1).

## C4G 1.0.0 at pasekahq/quota-balancer@bd174f4
- Vendored again byte for byte at the new pin; qb keeps the version 1.0.0 (ADR-0038). Additive:
  `CacheMode` adds `auto_1h`, the vendor's one-hour cache lifetime where it has one, else as
  `auto`; `AuditAction` adds `admin_roles` and `admin_reload`; `ConfigRoute` adds optional
  `client_effort` and `logins`. New operations: `GET /v1/model/ladder` (`LadderAnswer`,
  `LadderStepState`, `LadderCandidateState`, `ApiFamily`), `GET` and `PUT /v1/admin/roles`
  (`AdminRoles`, `AdminRolesPut`, `AdminRolesWritten`, `RoleConfig`, `LadderStep`,
  `LadderCandidate`, `RoleReserve`) and `POST /v1/admin/reload` (`AdminReloadAnswer`), forwarded
  to qbd. Descriptions of `RouteStatus` and `AuditEntry` follow.

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

## C6 0.8.0
- `raw_sessions` reads the raw dialogue the archive pulls (additive, ADR-0051 section 3):
  `RawMessage` gains optional `session_key`, `seq` and `message`, the stored C4G `Message` in
  its canonical serialization as a string; the engine always sends them. `RawMessage.ts` and
  `RawSessionsQuery.since` become optional: `ts` is absent for a message written without a
  time, and a message without a time matches no time filter. An absent `limit` means 10000. The
  engine serves the method: its turn messages, live and archived, after the cursor in journal
  order, with `content` and `role` those of its history index; its cursor is the journal
  position `n` in decimal. It leaves out compaction messages, steps, system prompts, a turn's
  context and delivery records; forget marks filter nothing.

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

## C7 0.7.0
- `SearchRequest` gains optional `no_miss` (additive, ADR-0051 section 5, ADR-0047 section 5):
  true queues no miss for a query that finds nothing, so no `garden_miss` repeats the words of
  the owner's forget `query`.

## C7 0.6.0
- Forget in the garden (additive, ADR-0051 section 5, ADR-0055 section 6): `x-methods` gains
  `forget` (`ForgetRequest` with 1 to 100 `refs` of the forms `seed:<seq>`, `c8:<n>` and
  `c8:session:<id>`, `reason` required without `undo`, `provenance` in the form of C8
  `Mark.provenance` and optional `undo`; `ForgetResponse` with one `ForgetOutcome` per ref:
  `ref`, `marks` and `queue`). A marked seedbed record is left out of `search`, `pointers` and
  `seedbed_take`, and `seedbed_done` answers it `not_found`, until `undo` lifts its marks. Every
  ref queues the current facts it reaches for the night's reconciliation; `undo` removes the rows
  not yet taken. `ErrorCode` `not_found` also covers a `forget` of an unknown seedbed ref and a
  `seedbed_done` of a forgotten one.

## C7 0.5.1
- Description of `Pointer.hint`: the form for a fact and the form for a seedbed record,
  with `disputed` after the kind when the record is disputed (ADR-0055 section 3).

## C7 0.5.0
- The garden's seedbed (additive, ADR-0055 section 7): `propose` writes the day's records into
  the seedbed and the night's processing hands them to the edge. `ProposeRequest.entity` is no
  longer required (the edge refuses a candidate without it as `invalid` when it decides one).
  `ProposalStatus` gains `seeded`: `ProposeResponse.proposal_id` is then the record's ref
  `seed:<seq>` and `conflicts_with` the fact the record disputes. `Fact` gains optional
  `unprocessed`, `disputed` and `conflicts_with`; `Pointer` gains optional `unprocessed` and
  `disputed`. `x-methods` gains `seedbed_take` (`SeedbedTakeRequest` with optional `limit` 1 to
  100 and `until`, `SeedbedTakeResponse`, `SeedRecord` with `ref`, `candidate`, `written_at` and
  optional `conflicts_with`) and `seedbed_done` (`SeedbedDoneRequest` with 1 to 100 `items`,
  `SeedbedDoneItem` with `ref` and optional `candidates`, `SeedbedDoneResponse`, `SeedbedOutcome`
  with `ref` and `outcomes`); a seedbed ref is `seed:` and an integer from 1. `ErrorCode`
  `not_found` also covers a `seedbed_done` of an unknown seedbed ref.

## C7 0.4.0
- `ProposalReason` gains `ambiguous_entity` (additive, ADR-0047 section 8, card 12): the answer
  to a `propose` the edge queued because its entity name could name more than one entity, the
  digest item's `entity_candidates`.

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

## C8 0.8.0
- `Mark` (additive; ADR-0051 section 5): a forget mark on one message, with `id`, the message
  `n`, `at`, `reason`, `provenance` (`s<session>-t<turn>-n<n>`, the turn of the owner's command
  and the `n` of its prompt, `-n<n>` absent until that turn is journaled), `lifted_at` and
  `lifted_provenance`. A message is forgotten while one of its marks has no `lifted_at`; marks
  are never removed. A 0.7.0 journal migrates with no marks.

## C8 0.7.0
- `Message.n`, `Message.at` and `Message.origin` (additive, optional; ADR-0051 section 3): the
  message's insertion position over the whole journal from 1, the Unix time the engine wrote it,
  and `turn` or `compaction`; the engine writes all three on every message it writes. A 0.6.0
  journal migrates with its messages numbered in `(session, seq)` order, without `at`, and
  `compaction` for a message whose bytes equal those of an archived message of its session with
  a lower `seq` or that is a handoff summary.

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

## C9 0.7.0
- Section `connectors` of `ulei-connectors` (additive): `connectors.dir`, `connectors.state`,
  `connectors.log` and `connectors.run_dir` (each a `string`), `connectors.enabled` (a `string`,
  comma-separated connector names) and `connectors.sync_every` (an `integer`, seconds, default
  900), beside the item section `connectors.<name>` with the owner's records `key_file`,
  `allow`, `read` and `write` (each a `string`, the last three comma-separated), `network`
  (`enum` `direct`) and `landlock` (`enum` `off`); no item takes the name of a key of the
  section (ADR-0056 sections 2 and 5.5). A section may hold keys and an item section at once.

## C9 0.6.0
- Section `memory` (additive) with the keys `memory.archive` (a `string`, the SQLite file of the
  archive) and `memory.socket` (a `string`, the C10 socket), both required by
  `ulei-memory serve`, which also reads `engine.c6_socket` to pull the dialogue (ADR-0051).

## C9 0.5.0
- Keys `engine.skills_index` (a switch, default off: the system prompt carries the skills
  index only when it is on) and `engine.task_board_file` (a `string`, the file the
  task-delegation meta-skill of the core names), and the item section `engine.core.<id>` with
  `file` (a `string`, the file whose text replaces the shipped text of the safeguard or
  meta-skill `<id>`) and `enabled` (a switch, default on), all additive; the engine refuses an
  id it does not ship (ADR-0054).

## C9 0.4.0
- Key `engine.hardline_malformed_mode` (additive): a `string` with the `enum` `warn` and `block`,
  what the engine's hardline floor does with a `terminal` command whose grep pattern operands do
  not parse; `warn` writes a warning line to stderr and matches the hardline patterns on crude
  variants too, `block` refuses the command; default `warn` (ADR-0016 section 14).

## C9 0.3.0
- Key `engine.prompt_cache` (additive): a `string` with the `enum` `auto_1h`, `auto` and `off`,
  the C4G cache mode of the engine's turn, compaction and `no-ask-guard` classifier requests;
  `off` sends none; default `auto_1h` (ADR-0020 section 5). A `string` key may list its values
  in `enum`, and the binary reading it checks the value (ADR-0041).

## C9 0.2.2
- Description of `garden.api.pointer_min_lexical`: the default `"8.0"`, set on the synthetic
  gold set over the lexical index with stems and transliteration (ADR-0047 section 4).

## C9 0.2.1
- Descriptions of `garden.api.pointer_min_cosine` and `garden.api.pointer_min_lexical`: their
  ranges and the defaults `"0.3"` and `"4.6"` set on the synthetic gold set (ADR-0047
  section 4).

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

## C10 0.1.0
- Draft archive served by `ulei-memory serve` (ADR-0051 section 2): `x-methods` `put`,
  `search`, `read`, `delete`, `sync` and `health` over HTTP/1.1 on a unix socket, one request
  per connection, request bodies at most 1 MiB; an item is identified by the lowercase hex
  SHA-256 of its original bytes (`ItemId`) and named by one `Entry` per `(source, external_id)`;
  `put` answers `stored` or `duplicate`, `search` answers hits with their matching entries and
  an FTS5 snippet and whether the dialogue pull succeeded (`synced`), `delete` answers its
  audit row `Deletion`; one error body `Error` with `ErrorCode`.

## C11 0.1.0
- Draft source connector (ADR-0056 section 2): the manifest `connector.json` (`Manifest`: a
  `name` other than `dialogue`, `version`, `contract`, `transport` `stdio` or `mcp`, `command`,
  `schedule.every` 60 to 604800 s, `timeout`, `source.addresses`, `key`; no isolation field);
  `Request`; `Item`, the fields of C10 `put` without `source`; the stdio `Line` (`item`,
  `checkpoint`, `done`, `error`) of at most 1 MiB plus 4 KiB (`x-stdio`) and the MCP tool
  `ulei_pull` answering a `Page` (`x-mcp`); `Error` with `ErrorCode`; the broker's
  `BrokerRequest` and `BrokerReply`; the run log line `RunRecord` with `Grants`, which keeps
  every `too_large` id.
