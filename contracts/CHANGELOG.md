# Contracts changelog

Semantic versioning per contract (ADR-0005). A breaking change (removed field, definition,
enum value or operation; a field becoming required; a type or validation keyword changing;
`additionalProperties` becoming `false`) needs a major bump (minor while `0.x`), an entry
here and an ADR with `Affects: contract:<id>`. `cargo xtask contracts --base <rev>` enforces it.

## C1 1.5.0
- `Kind` gains `reminder_due` and `reminder_fallback` (additive; the packet is unchanged). The
  hive publishes `reminder_due` when a reminder card falls due, its payload carrying the reminder
  text verbatim, and `reminder_fallback` after it sent a reminder to the owner directly through
  `ulei-tg notify`, carrying the text sent and the Telegram receipt (ADR-0100).

## C1 1.4.0
- Inbox wire operation `reconcile` (additive; the packet is unchanged): `ReconcileRequest` (`id`,
  `outcome` a `ReconcileOutcome`, the one value `reconciled-delivered`, and `note` a
  `ReconcileNote`, 1 to 500 characters, not blank, no control character) answered by
  `ReconcileAudit` (`id`, `outcome`, `reason` the former `UncertainReason`, `note`, the caller's
  `uid` and `pid`, `time`). Only an `uncertain` event is closed: its row becomes `done` with that
  outcome and the audit line is stored in the same transaction; an unknown id is `not_found`,
  any other status `conflict`. `RowOutcome` gains `reconciled-delivered`; `Counts` gains the
  optional `reconciled`, the number of audit lines (ADR-0099).

## C1 1.3.0
- Inbox wire operation `uncertain` (additive; the packet is unchanged): `UncertainRequest`
  (optional `limit`, 1 to 100, 20 when absent) answered by `UncertainList`, at most `limit`
  events with status `uncertain`, oldest first by `created` then `id`, each an `UncertainEvent`
  without payload: `id`, `source` (the packet `id` up to its first `:`), `kind` (the packet
  `kind`), `time` (the row `created`) and `reason` (the row `outcome`, an `UncertainReason`).
  The inbox answers it from one read query and changes no row (ADR-0097 section 5).

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

## C2 0.5.1
- Descriptions of `HiveCreateInput` only (no keyword changes): `accept_review` is the judge's
  acceptance and no longer implies `review`; `review` is off by default and only `review: true`
  turns it on, with the author's three cases for it (code cards, complex research, the author's
  own choice). The hive CLI `create --accept-review` no longer sets `--review` (owner, 26.11.2026).

## C2 0.5.0
- New tool `hive_resolve` (additive, ADR-0095): `HiveResolveInput` (`card_id`, `action` one of
  `accept` and `rework`, a required `text` of 1 to 4000 characters) answered by `Ack`. The
  author's decision on a disputed card (`blocked`, wait `disputed`): `accept` finishes it done,
  `rework` queues a new round with `text` as its instructions; it runs `hive.py resolve` with the
  server's actor as `--by`, never an input.

## C2 0.4.0
- Read-only snapshot tools of the hive (additive, ADR-0086): `hive_inspect` (`CardRef` →
  `HiveInspectOutput`: state, round, title, result and report lines, `waiting_on`, one line on
  what a live head waits on, and the whole `snapshot` object of the hive's `inspect --json`:
  rounds with runs, reports, gate and verdicts, live heads with units, commands, pull requests
  and workflow runs, queue and slot holders, links, steers with text, deliveries, the
  workspace's files), `hive_health` (`Empty` → `HiveHealthOutput`: whether the dispatcher ticks,
  slots used, concurrency, queue length and the `snapshot` of `health --json`) and `hive_file`
  (`HiveFileInput`, a card id and a relative path in its workspace or `@out/<name>`, an offset
  and a limit of at most 262144 bytes → `HiveFileOutput`, a file's text or a directory's
  entries). `HiveListInput` gains optional filters `card_class`, `parent`, `followup_of`,
  `created_by`, `placement` (`board` or `hidden`) and `query` (text in the title or goal).

## C2 0.3.0
- `hive_create` takes the hive's own timing (additive, ADR-0082): optional `not_before`
  (`--not-before`, a one-off card held until then), `every` and `cron` (a series of one card per
  period, `series_run`) with `tz`, `until` and `max_runs`, and `recheck_every` with
  `recheck_until` and `recheck_max` (rechecks of a done card's acceptance). Times are RFC 3339
  with an offset (a pattern), durations `^[1-9][0-9]{0,6}[smhd]$`, `cron` five fields of a safe
  character set, `tz` an IANA name pattern, `max_runs` 1 to 10000 and `recheck_max` 1 to 1000.
  `deliver`, `script`, `executor`, `model` and the other flags of ADR-0071 section 4 stay out.
  `HiveCreateOutput` gains the optional `card_class` and `owner_visible` of the CLI's answer.

## C2 0.2.0
- The hive tools as `ulei-hive-mcp` serves them to the engine (ADR-0071; breaking, minor bump
  while 0.x). New tool `hive_cancel` with `HiveCancelInput` (`card_id`, a required `reason`)
  answered by `Ack`. `hive_create` answers the new `HiveCreateOutput` (`card_id`, `created`,
  `state`) instead of `CardRef`. `HiveCreateInput`: `step` and `source` (pattern, no leading
  `-`) become required, as in `hive.py create`; new optional `details` (the card body),
  `accept_review` (exactly one of it and `acceptance`, a `oneOf`), `kind` (`owner_task`,
  `system`, `followup`), `review`, `hold`; length limits on `title`, `goal` and `acceptance`; a
  pattern on `idem_key`. `CardRef`, `HiveSteerInput` and the new defs carry the card id pattern;
  `HiveSteerInput.note` gets a length limit. `CardStatus` gains `title` and `report_line` and
  bounds `result_line`; `HiveListInput.state` becomes an enum of the hive's states.

## C2 0.1.0
- Draft MCP facade of the core (product shape §2.3).

## C3 0.1.0
- Draft card/head/report contract derived from the current Hive.

## C4 0.3.0
- Transcribed again at the qb release pin pasekahq/quota-balancer@a6442c5 (qb 0.2.0, additive):
  optional `ReportRequest.credential`, a fingerprint of 8 to 64 lowercase hex digits of the
  credential the reported request used; a report of another fingerprint clears an `auth_stale`
  or `auth_dead` mark. New operation `POST /v1/admin/logins/reset` (`AdminLoginsReset` with
  `logins` or `all`, answered by `AdminLoginsResetAnswer` with `LoginReset`). `PickRequest.need`
  is described as accepted but not used yet.

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

## C4G 1.0.0 at pasekahq/quota-balancer@a6442c5 (qb 0.2.0)
- Vendored again byte for byte at the release pin; qb keeps the version 1.0.0 (ADR-0038).
  Additive: the Message Batches operations `POST /v1/model/batches`,
  `GET /v1/model/batches/{batch_id}`, `GET /v1/model/batches/{batch_id}/results` and
  `POST /v1/model/batches/{batch_id}/cancel` (`BatchSubmit`, `BatchRequest`, `BatchStatus`,
  `BatchCounts`, `BatchProcessing`) and `POST /v1/admin/logins/reset` forwarded to qbd
  (`AdminLoginsReset`, `AdminLoginsResetAnswer`); `AuditAction` adds
  `admin_logins_reset`, `LoginSpend` adds `reserved_usd` (open batch reserves); the
  descriptions of `UsageRow` and `GET /v1/model/spend` follow. The engine calls none of the new
  operations.

## C4G 1.0.0 and C4 0.2.0 at pasekahq/quota-balancer@d0f9804 (qb 0.1.0)
- Vendored again byte for byte at the release pin; qb keeps the version 1.0.0 (ADR-0038).
  Descriptions only: the refusals of a turn request name their classes (400, 411 and 413 are
  `bad_request`), `cancel` answers after 5 s at most and its 404 and 403 are `bad_request`,
  `session_key` keeps no session on one login, and `Usage.input` is disjoint from `cache_read`
  and `cache_write`. C4 is unchanged but for the commit it names: `crates/qb-proto/src/lib.rs`
  is the same at both commits.

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

## C6 0.13.0
- The facts behind the ack of a quota-exhausted inbox turn (additive, ADR-0036 section 7):
  `TurnReport` gains optional `resets_at`, the Unix time of the nearest quota reset the model
  gateway named for a `quota_exhausted` turn, and optional `tools_ran`, whether the turn ran any
  tool call. The engine sets `tools_ran` on every turn its loop ran and `resets_at` when the
  gateway named one; a `last_turn` read back from the journal holds neither, and an absent field
  means unknown. `TurnReport` is closed, so `ulei-inbox`, `ulei-tg` and `ulei-engine` deploy
  together for C6 0.13.0.

## C6 0.12.0
- A global `set_route` (additive, ADR-0094): `SetRouteRequest` gains optional `global`. With it
  the runtime stores the alias with the session as before and also as its default route, which
  every session that stores no route runs on, a new session of the key included, and which
  survives a restart; an alias equal to the configured default clears the stored default. The
  runtime answers `bad_request` for a global alias its model gateway does not list. `RouteSet`
  gains optional `global`, which the engine always sends. `SetRouteRequest` and `RouteSet` are
  closed, so `ulei-tg` and `ulei-engine` deploy together for C6 0.12.0.

## C6 0.11.0
- Reactions of the agent (additive, ADR-0088): `SubmitTurnRequest` gains optional `message_id`,
  the channel's id of the owner message the prompt answers (the one the channel replies to);
  `TurnEventType` gains `reaction`, a `TurnEvent` with `message_id` and the emoji in `text`,
  after any `interim` and before `final`. The engine records one per successful
  `react_to_message` call and the channel sets it on the message. `SubmitTurnRequest` is
  closed and a client built before C6 0.11.0 refuses an unknown event type, so `ulei-tg` and
  `ulei-engine` deploy together for C6 0.11.0.

## C6 0.10.0
- `submit_turn` carries the owner's images (additive, ADR-0085): `SubmitTurnRequest` gains
  optional `images`, 1 to 10 absolute paths of local image files in arrival order. The engine
  reads each, attaches it as a C4G `image` block after the prompt's text in the copy of the
  prompt each request of the turn sends, and journals the prompt without them; an image it
  cannot attach becomes a line of text in that copy instead. `SubmitTurnRequest` is closed, so
  an engine built before C6 0.10.0 refuses a request that carries `images`: `ulei-tg` and
  `ulei-engine` deploy together for C6 0.10.0.

## C6 0.9.0
- `raw_sessions` never stalls on one message (additive, ADR-0051 section 3): `RawSessionsPage`
  gains optional `skipped`, a list of `RawSkipped` (`session_id`, `len` required; `session_key`,
  `seq`, `ts`), the messages that do not fit an answer of `max_body_bytes` alone, named without
  their text or bytes; the cursor moves past them, and they count toward `limit`. The engine
  answers such a message this way instead of `500`.

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

## C7 0.14.0
- The night's judge of new facts (additive, ADR-0096). `x-methods` gains `reconcile_take`
  (`ReconcileTakeRequest` with `after`, `limit` 1 to 100, default 50, and `hint`, which gives
  the due lexical hints first unless false, answered by
  `ReconcileTakeResponse` of `ReconcileItem`s: `seq`, `entity`, `entity_name`, the new `fact`,
  1 to 10 `hints` as `Fact`s, `basis` `HintBasis` `dense` or `lexical`, `embedder`),
  `reconcile_done` (`ReconcileDoneRequest` with `judge`, up to 100 `ReconcileJudged` items of
  `fact_id` and `ReconcileVerdict`s, each `hint`, `relation` `ReconcileRelation` `duplicate`,
  `supersedes` or `contradiction` and `note`, and optional `JudgeStats`; answered by
  `ReconcileDoneResponse` with one `ReconcileDoneItem` per fact of `ReconcileOutcome`s, each
  `applied` `ReconcileApplied` `merged`, `superseded`, `contradiction`, `not_applied` or `stale`
  with `change_id`), `reconcile_review` (`ReconcileReviewRequest` with `limit` 1 to 200,
  `before`, `entity`, `relation` and `open`, answered by `ReconcileReviewResponse` of
  `ReconcileChange`s with `state` `ChangeState` `applied`, `open`, `resolved` or `undone`, both
  `ChangeFact`s, the judge's `note`, `judge`, `embedder`, `basis`, `proposal_id`, `restored`
  and `undone_at`, and `next`) and `reconcile_undo` (`ReconcileUndoRequest` with `change_id`,
  answered by `ReconcileUndoResponse`). `Fact` gains `conflict_note`, and `disputed` with
  `conflicts_with` also marks a fact in an open judged contradiction. `DigestItem` gains `note`.
  `Health` gains `reconcile` (`ReconcileStats`, anonymous counters with `RelationCounts` and
  `LatencyBucket`s). `ErrorCode` `not_found` and `invalid_request` cover the new methods. No
  existing field changes meaning.

## C7 0.13.0
- `PlaceMarkKind` gains `open_unconfirmed` (additive, ADR-0080 section 5 items 5 and 6): in a
  `places_search` with `open_now`, a place whose `open` is `probably_open` or `unknown` ranks as
  if one more attribute were stale and carries this mark, with `key` `hours` and its `age_days`
  when the place has `hours`; a stale `hours` gets no `stale` mark. The description of
  `open_now` says that a fresh `operating_status` `temporarily_closed` drops a place, and those
  of `local_time` and `ErrorCode` that with `open_now` a `local_time` on no real date from 1970
  (such as `2031-02-30`) is `invalid_request`.

## C7 0.12.1
- Descriptions of `PlaceWrite.name`, `PlaceRefusal` and `PlaceMark` only (no keyword changes):
  a place written by name is matched by the bed's dedup rule (ADR-0080 section 6), by `osm_id`,
  else by a `location` within 75 m and a similar name, else, for a write with neither, by an
  equal name or alias, where two or more are `ambiguous_place`; a new place is paired as a
  `possible_duplicate` with each place within 75 m of another name and each of a similar name
  where either has no location.

## C7 0.12.0
- `Health` gains optional `vectors_missing`, `passages_skipped`, `passages_deferred` and
  `model_check` (additive, ADR-0087 section 8, review E13r findings 3, 5 and 7), so `dense:
  ready` no longer hides a dense side that covers only part of the garden: the versions a search
  can reach without a vector of the dense side's model; of them the passages skipped for good
  under it, and those put behind the others after the helper was killed from outside three times
  in a row while embedding one; and the last content check of the encoder engine's weight file
  since the daemon started (`ModelCheck`: `at`, `result` of `ModelCheckResult` `passed`,
  `corrupted`, `unusable` or `no_result`, and `detail` unless it passed). After every result but
  `passed` the next check comes 1 minute later, doubling up to 1 hour. No existing field changes
  meaning; `SearchResponse.lexical_only` is unchanged.

## C7 0.11.1
- Descriptions of `PlacesFilters`, `min_rating` and `max_price_for_two`: a threshold the request
  gives drops a place without the attribute, while the profile's `min_rating` and the upper end
  of its `budget_for_two` drop only a place whose value misses them (ADR-0080 section 5). The
  schema description names `places_write`, `places_verdict` and `places_profile` as a list.

## C7 0.11.0
- The places bed's profile write (additive, ADR-0080 section 7): `x-methods` gains
  `places_profile` (`PlacesProfileRequest` with `key` `ProfileKey` `home`, `min_rating`,
  `budget_for_two`, `currency` or `ban`, `value`, `source_ref` and `set_at`), answered by
  `PlacesProfileResponse` with `outcome` `ProfileOutcome` `written` or `unchanged`. A key other
  than `ban` keeps one row, which the write replaces; `ban` keeps one row per value. `ErrorCode`
  `invalid_request` also covers a refused `places_profile`.

## C7 0.10.0
- The places bed's writes (additive, ADR-0080 section 7): `x-methods` gains `places_write`
  (`PlacesWriteRequest` with 1 to 100 `PlaceWrite`s, each named by exactly one of `id` and
  `name`, with `status`, `source` and up to 50 `PlaceObservation`s: `key`, `value`,
  `checked_at`, `source`, optional `confidence`), answered by `PlacesWriteResponse` with one
  `PlaceWritten` per place (`outcome` `PlaceOutcome` `created`, `adopted`, `found` or `refused`,
  `id`, `refused` and `message`, and one `AttrWritten` per attribute with `key`, `outcome`
  `AttrOutcome` `written`, `replaced`, `history`, `unchanged` or `refused`, `refused` and
  `message`); `PlaceRefusal` names the broken rule. The batch is one unit of work: a store
  failure writes nothing, a refused place or attribute writes nothing of its own and the rest
  is written. `places_verdict` (`PlacesVerdictRequest` with `place`, `verdict` -1 to 1, `note`,
  `said_at`, `source_ref`, `better_than`) is answered by `PlacesVerdictResponse` with `outcome`
  `VerdictOutcome` `written` or `repeated` and `seq`. `ErrorCode` `invalid_request` also covers
  a `places_write` place with both or neither of `id` and `name` and a refused
  `places_verdict`, and `not_found` a `places_verdict` of a `place` or `better_than` that is no
  place of the bed.

## C7 0.9.0
- The places bed's search (additive, ADR-0080 section 5): `x-methods` gains `places_search`
  (`PlacesSearchRequest` with optional `filters`, `near`, `open_now`, `local_time`, `vibe` and
  `limit` 1 to 50, default 10; `PlacesFilters` with `cuisine`, `max_distance_m`, `min_rating`,
  `max_price_for_two` and `status`; `GeoPoint`; `PlaceStatus` `candidate`, `active`, `gone`),
  answered by `PlacesSearchResponse` with at most 50 `PlaceHit`s best first: `id`, `name`,
  `status`, `distance_m`, `maps_url` on every place, `attrs` (`PlaceAttr` with `key`, `value`,
  `checked_at`, `source`, `confidence`, `age_days`, `stale`), the owner's latest `verdict`
  (`PlaceVerdict` with `verdict` -1 to 1, `note`, `said_at`, `source_ref`, `better_than`),
  `open` (`OpenState` `open`, `probably_open`, `unknown`) and `marks` (`PlaceMark` of
  `PlaceMarkKind` `stale` with `key` and `age_days`, `ban_risk` with `terms`,
  `possible_duplicate` with `other`). `ErrorCode` `invalid_request` also covers a
  `places_search` with `open_now` and no `local_time` or with `max_distance_m` and no point.

## C7 0.8.0
- The night stage's walk and the forget queue (additive, ADR-0078): `SeedbedTakeRequest` gains
  optional `after`, a ref `seed:<seq>`, so a night that leaves a batch for the next night takes
  the records after it. `x-methods` gains `forget_take` (`ForgetTakeRequest` with `limit` 1 to
  100, default 50, and `after`, a row id; answered by `ForgetTakeResponse` with the untaken
  `ForgetRow`s `id`, `fact_id`, `ref`, `marked_at`, `reason`, `queued_at`, by `id`) and
  `forget_done` (`ForgetDoneRequest` with 1 to 100 `ids`; answered by `ForgetDoneResponse` with
  one `ForgetCleared` per id: `id`, `fact_id`, `closed`). `forget_done` closes a queued fact that
  is still current on the transaction axis with no new version and marks the row taken, each row
  in its own unit of work; an unknown id is `not_found` before any row.

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

## C8 0.10.0
- `PruneRearm` (additive, ADR-0098): at most one per session, the rough tokens its live set must
  reach before the next proactive prune of old tool results, written with the live set of the
  prune that set it and removed by a compaction. A 0.9.0 journal reads without one; an engine
  of 0.9.0 opens a 0.10.0 journal and ignores it.

## C8 0.9.0
- `DefaultRoute` (additive, ADR-0094): at most one per journal, the route alias a global C6
  `set_route` made the engine's default; a session that stores no route runs on it instead of
  the configured `engine.default_route`. A 0.8.0 journal migrates without one.

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

## C9 0.15.0
- Keys `engine.compression.proactive_prune_tokens` (default 120000, 0 never prunes),
  `engine.compression.proactive_prune_min_result_chars` (default 8000, at least 200) and
  `engine.compression.proactive_prune_min_reclaim_tokens` (default 4096), each an `integer`
  (additive, ADR-0098): the proactive prune of old tool results between compactions.

## C9 0.14.0
- Keys `engine.spill_result_chars` (default 100000), `engine.spill_mcp_result_chars` (default
  50000), `engine.spill_step_chars` (default 200000) and `engine.spill_preview_chars` (default
  1500), each an `integer` above zero (additive, ADR-0016 section 3): the thresholds of saving
  tool results too large for the context to files under `engine.terminal_output`.

## C9 0.13.0
- Keys `night.judge_max_usd` (an `integer` or `string`, a decimal, default `"0.5"`) and
  `night.judge_batch` (an `integer`, 1 to 100, default 25) of the night's stage d (additive,
  ADR-0096). Description of `garden.edge.reconcile_min_cosine`: no longer read; the daemon accepts
  it with one stderr line and the next release removes it.

## C9 0.12.0
- Key `engine.trace_file` (additive, a `string`): the debug trace of the engine's turns, one
  JSON object per line (ADR-0092). Unset, the engine writes no trace.

## C9 0.11.0
- Key `garden.schedule.verify_interval` (additive): an `integer`, seconds between two content
  checks of the encoder engine's weight file by `ulei-garden-embed verify`, the first at start;
  default 86400 (ADR-0087 section 8). Description of `garden.embedder.enabled`: a helper that
  refuses a corrupted weight file of the encoder engine, or a store whose last check of that
  file failed, starts the daemon lexical-only with `dense` down until a check passes; every
  other helper that does not get ready still stops the start (amends ADR-0047 section 2).

## C9 0.10.0
- Keys `telegram.guest_gate_url` and `telegram.guest_gate_token_file` (additive, each a
  `string`, both or neither): the guest gate that gets the owner's protocol updates before the
  ingress (ADR-0083). Unset, `ulei-tg` calls no gate.

## C9 0.9.1
- Description of `night.seed_batch`: default 20 instead of 50, so a batch's answer fits
  `night.max_tokens` (ADR-0078 amendment).

## C9 0.9.0
- Section `night` of `ulei-engine night` (additive, ADR-0078): `route`, `state`, `sources`,
  `separator` and `roles` (each a `string`; `sources` and `roles` comma-separated), `max_usd`,
  `price_input`, `price_output`, `price_cache_read` and `price_cache_write` (decimals, an
  `integer` or a `string`), `max_prompt_tokens`, `max_tokens`, `timeout`, `seed_batch` and
  `message_chars` (each an `integer`).

## C9 0.8.0
- Section `hive_mcp` of `ulei-hive-mcp` (additive, ADR-0071): `listen`, `token_file`,
  `hive_command`, `hive_config` and `actor` (each a `string`), `timeout` (an `integer`, seconds,
  default 120) and `hold` (a `boolean`, default false).

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

## C10 0.6.0
- The forest (additive, ADR-0079): `x-methods` `forest_put`, `forest_search` and
  `forest_forget`. `ForestPutRequest` (`source`, `session`, `text` of at most 4,000 characters,
  optional `from` and `to`, 1 to 1,000 `links` each a `ForestLink` of an entry's `seq` and item
  `id`, and the `Producer`: the night run's `run` and the model `route`) answered by
  `ForestPutResponse` (`id`, a `ForestId`; `status` `stored` or `duplicate`; `replaced`, the
  records of the same session sharing a link that the put superseded). `ForestSearchRequest`
  (`q`, `source`, `since`, `until` over the record's span, `limit` 1 to 20, default 10) answered
  by `ForestSearchResponse` with `ForestHit`s (the record, a `snippet()`, its `put_at`, producer
  and its `links` as `ForestLinked` with each entry's `external_id`); superseded and forgotten
  records are not found; a page ends before 786,432 bytes of body. `ForestForgetRequest` (`ids`,
  `reason`, `provenance`, `undo`) answered by `ForestForgetResponse` with one `ForestForgotten`
  per id. `Health` gains optional `forest`, the count of current records. `ErrorCode` names the
  new `not_found` cases; no code is added.

## C10 0.5.0
- `x-methods` gains `list` (additive, ADR-0078): `ListRequest` with `source`, `after` (a `seq`,
  default 0), `limit` (1 to 500, default 100) and `text_chars` (1 to 16,384, default 4,000),
  answered by `ListResponse` with `entries`, each `Listed` (`seq`, `id`, `external_id`, `who`,
  `at`, `kind`, `put_at`, the head of the item's `text` and `text_truncated`), by `seq`, and
  `more`. A page ends early before 786,432 bytes of body. `seq` is the entry's place in the
  order the archive stored the source's entries, given once and never reused.

## C10 0.4.1
- Descriptions name the ADR of the parts as ADR-0076, its number after a clash; no change of
  the schema's shape.

## C10 0.4.0
- Originals over the 1 MiB body go in parts (additive, ADR-0076): `x-methods` `put_begin`
  (`PutBeginRequest`: an item's fields with `len` and `id` instead of the bytes; answered by
  `PutBeginResponse` with an `upload` to fill or a settled `status`), `put_part`
  (`PutPartRequest`: `upload`, `index` from 0, 1 to 524,288 bytes as `bytes_b64`; answered by
  `PutPartResponse`) and `put_end` (`PutEndRequest`; the SHA-256 and length are checked, then
  `PutResponse`). Of a `put_begin` text the first 262,144 bytes are indexed; an upload expires
  3,600 s after its last request. `ReadRequest` gains optional `offset` and `length` (at most
  524,288) and `ReadResponse` optional `offset` for a range; an original put in parts is read by
  range only, a whole read of it is `too_large`. `ErrorCode` names the new cases; no code is
  added.

## C10 0.3.0
- `Hit` gains optional `text` and `text_truncated` (additive, ADR-0051 section 2): the first
  1,200 characters of the text the putter gave for the index, and whether the text is longer.

## C10 0.2.0
- `Health` gains optional `skipped` (additive, ADR-0051 section 3): every `Skipped` reference
  (`source`, `external_id`, `len`, `skipped_at` required; `at`) a pull kept for a record the
  engine served as C6 `RawSkipped`, oldest first, without a cap.

## C10 0.1.0
- Draft archive served by `ulei-memory serve` (ADR-0051 section 2): `x-methods` `put`,
  `search`, `read`, `delete`, `sync` and `health` over HTTP/1.1 on a unix socket, one request
  per connection, request bodies at most 1 MiB; an item is identified by the lowercase hex
  SHA-256 of its original bytes (`ItemId`) and named by one `Entry` per `(source, external_id)`;
  `put` answers `stored` or `duplicate`, `search` answers hits with their matching entries and
  an FTS5 snippet and whether the dialogue pull succeeded (`synced`), `delete` answers its
  audit row `Deletion`; one error body `Error` with `ErrorCode`.

## C11 0.2.0
- `RunRecord` gains optional `connect_ports` (additive, ADR-0056 section 5.5): under
  `network = "direct"` the TCP ports Landlock let the run connect to, the ports of
  `source.addresses` and `allow`; absent when its connect ports were not limited (no `direct`,
  Landlock below ABI 4 or `landlock = "off"`).

## C11 0.1.0
- Draft source connector (ADR-0056 section 2): the manifest `connector.json` (`Manifest`: a
  `name` other than `dialogue`, `version`, `contract`, `transport` `stdio` or `mcp`, `command`,
  `schedule.every` 60 to 604800 s, `timeout`, `source.addresses`, `key`; no isolation field);
  `Request`; `Item`, the fields of C10 `put` without `source`; the stdio `Line` (`item`,
  `checkpoint`, `done`, `error`) of at most 1 MiB plus 4 KiB (`x-stdio`) and the MCP tool
  `ulei_pull` answering a `Page` (`x-mcp`); `Error` with `ErrorCode`; the broker's
  `BrokerRequest` and `BrokerReply`; the run log line `RunRecord` with `Grants`, which keeps
  every `too_large` id.
