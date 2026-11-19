# ADR-0036: owner inbox delivery: the core's loop, quiet inbox turns and the ack (C6 0.5.0)

Status: accepted
Date: 2026-11-19
Affects: crates/core, crates/engine, crates/channels, crates/runtime-adapters, contract:C6

## Context

The owner inbox (C1, ADR-0007) holds events for the owner's session; something must take each
event into that session, let the agent answer the owner and settle the event. The reference is
the owner inbox patch of the reference agent (`THIRD_PARTY_NOTICES.md#ref-agent`, port map R12):
`gateway/owner_inbox.py` runs a consumer inside the gateway process (`start`, `pump_once`,
`Bridge.tick`, `Bridge.blocked`), hands the event to the gateway as an internal message, follows
it with a receipt (`Receipt.begin`, `Receipt.agent_result`, `no_model_text`, `Receipt.invoke`,
`Receipt.complete`) and keeps the turn quiet (`quiet_config`, `inbox_turn_active`,
`owns_recovery`), with hunks in `gateway/run.py`, `gateway/platforms/base.py`,
`gateway/session.py` and `gateway/response_filters.py`. Its store, `gateway/owner_inbox_store.py`,
is ported as `crates/core/src/inbox.rs` with its defaults.

In ulei the store lives in `ulei-inbox` (`crates/core`), the agent in `ulei-engine serve`
(ADR-0028) and the Telegram channel in `ulei-tg` (ADR-0032 to ADR-0035), three processes joined
by C6 (ADR-0008). This record fixes how the reference loop runs across them.

## Decision

### 1. Place: a thread of `ulei-inbox serve`

- `ulei-inbox serve --root DIR --socket PATH --deliver C6_SOCKET` starts the delivery loop on a
  `deliver` thread next to the C1 server thread; without `--deliver` the daemon serves C1 only,
  as now. The loop opens its own store connection on the same root (two connections of one
  process on one `events.sqlite3`, `BEGIN IMMEDIATE` in `claim`, the store's 10 s busy timeout).
- Enable gate, as `start`: at start the loop runs only when `<root>/ENABLED` is a file; before
  every claim it checks the file again, and a missing file stops new claims only: a turn in
  flight is followed to its ack (`pump_once`).
- Single consumer, as `start`: at start the loop opens `<root>/consumer.lock` for append (mode
  0600) and takes an exclusive non-blocking `flock` (`File::try_lock`), held for the life of the
  process. A held lock means another consumer, the reference's included, runs on this root: the
  daemon serves C1 without the loop. Without `ENABLED` or without the lock the daemon writes one
  fixed stderr line, `ulei-inbox: owner inbox delivery off; ENABLED is missing` or `ulei-inbox:
  owner inbox delivery off; consumer.lock is held by another consumer`, and serves on. The
  `deliver` thread starts before the ready line.
- A failed step (a store failure, a failed `inject`, a `start` that changes no row) writes the
  fixed stderr line `ulei-inbox: owner inbox tick failed; durable event retained`, with no
  payload, lease or error text, and the loop goes on 2 s later (`start`'s `pump`). A failed
  `is_idle` or `status` is not a failed step (section 2).
- The loop's C6 client is a copy of the framing and checks of `ulei_runtime_adapters::client`
  (ADR-0008) inside `crates/core`, with the same error classes, its `Io` telling a failed
  connect from a later failure for PLUGIN.md section 6: a module crate may depend only on
  `ulei-contracts` (ADR-0001), as ADR-0033 section 1 found for `ulei-tg`. `crates/core` declares
  `C6`. Its call timeout is 10 s, above the engine's 5 s request timeout, and its connect timeout
  2 s, the values of `ulei-tg` (ADR-0033 section 2); every call it makes answers without waiting
  for a turn.

### 2. One step

The order of `Bridge.tick`: no claim unless the engine is reachable and the owner session free.

1. `recover(now)`.
2. Nothing claimable (no `pending` row with `available <= now`), or no `ENABLED`: wait
   (section 3).
3. Busy check: C6 `is_idle`. A failed call (any client error) or `idle: false` claims nothing;
   the loop checks again 2 s later.
4. `claim(now, LEASE_TTL)`; no row: wait.
5. `inject` with the row's `id`, the packet's `kind`, the model text of section 4 as `prompt`,
   the claim's `lease` and the row's `attempts` as `attempt`.
6. `accepted: true`: C1 `start(id, lease, session_id)`; the turn is in flight. `accepted: false`
   (the session became busy after the check, or the engine lost its channel): `retry(id, lease,
   now, RETRY_DELAY)`, 15 s, then go on at once from step 1. A failed `inject` follows the error
   table of `crates/runtime-adapters/PLUGIN.md` section 6, whose `retry` is the same 15 s; a
   replay runs 2 s later, after a `renew`, whether or not `ENABLED` is still there.
7. In flight, every 2 s: while the turn has not been seen ended, `renew(id, lease, now,
   LEASE_TTL)`; then `status` with the session and turn of the accept.
   - `queued` or `running`, or a failed call: next tick.
   - `ended` with `TurnReport.delivery`: `ack` by section 7, then go on at once from step 1.
   - `ended` without `delivery`: no more renewing; next tick. The channel reports the delivery
     of an ended turn within the lease left, at least 118 s; when the lease runs out first, the
     loop drops the turn and the store's `recover` marks the row `uncertain`
     (`expired-processing`).
   - `dropped` or `unknown`: `ack` `uncertain`.

The results of `renew` and `ack` are not checked, as in the reference. A restart of
`ulei-inbox` forgets the turn in flight; its `processing` row expires the same way.

### 3. Wake-ups

- Idle (nothing claimable and no turn in flight): the `deliver` thread blocks on a channel with
  no timeout, or with a timeout at the nearest due time when one exists: the smallest
  `available` of a `pending` or `deferred` row in the future and the smallest `lease_until` of a
  `leased` row. 0 wake-ups while the queue holds nothing due.
- The C1 server thread signals the channel after every committed `publish`; the loop then runs a
  step at once.
- The 2 s tick runs only while a turn is in flight, an `inject` waits for its replay, a
  claimable event waits for a free session (step 3), or after a failed step; a publish does not
  shorten it. It renews only until the turn has ended. `LEASE_TTL` is 120 s, the reference's
  claim and renew ttl.

### 4. The model text

The core builds the text the model sees, as `Bridge.tick` builds it from the claimed row; the
engine runs it unchanged as the event turn's prompt. The source is the `policy` and `text`
expressions of `Bridge.tick` in `gateway/owner_inbox.py`, ported byte for byte as built-in
constants:

- the header line `[INTERNAL BACKGROUND EVENT` ... `]` with its U+2014 dash, then a line feed;
- `Reason in this existing conversation using its current context. `;
- the policy: the weekly briefing text for kind `weekly`, the task status text for every other
  kind, each ending in a space;
- the fixed sentences from `Do not send a duplicate text via tools.` to
  `No automatic follow-up event.`, then a line feed;
- the JSON tail: Python `json.dumps(..., ensure_ascii=False)` of an object with the keys, in this
  order, `event_id` (the row's `id`), `kind` (the packet's `kind`), `source_ts` (the packet's
  `source_ts`: an integer stays an integer, a float prints as ADR-0007 section 5 rule 3 states),
  `payload_ref` (`sha256:` and the row's `digest`), `trust` (`untrusted-source-data`) and
  `source_text` (the packet's `payload`). Separators are `, ` and `: `; strings are escaped by
  ADR-0007 section 5 rule 1.

The mentions of `owner-channel-etiquette` and `tg-react.sh` stay as text; that skill and that
script are not ported. A 65,536-byte payload of control characters makes a prompt of about
395 KB and an `InjectRequest` body of about 460 KB, inside the 512 KiB C6 body limit; the body
therefore carries the prompt instead of the payload (section 8).

### 5. The injected turn in the engine

- `inject` and `is_idle` see the owner session free when no turn runs, none is queued, a
  channel holds `inbox_turns` for it, and the owner key has a session in the journal; an
  `inject` that finds it not free answers `accepted: false` with `retry_after_s` 15
  (`serve::RETRY_AFTER_S`, the C1 retry delay). The journal check ports `Bridge.tick`'s refusal
  to create a conversation: an owner key without a session gets no inbox turn.
- An accepted inject runs `prompt` as an `Event`-sender turn in the key's current session and
  never starts a new one, as now. The turn is an inbox turn: its report can carry a delivery.
- `inbox_turns` (held, as `turn_events` of ADR-0031 section 2): `session` must be the owner
  session, else `400 bad_request`. The answer names the oldest inbox turn not yet handed over,
  at once when there is one, else when `inject` starts one. A turn is handed over when its answer
  is written; an answer that fails to write leaves it for the next request. A new request
  releases a held one at once with no turn. The wait has no timer.
- `report_delivery` stores the channel's `Delivery` on the report of a handed-over inbox turn
  that has ended, once: `ok` true when stored, false for any other turn or a second report.
  `status` shows it as `TurnReport.delivery`, kept as long as the report.

### 6. The quiet turn in `ulei-tg`

- An `inbox` thread holds `inbox_turns` for the direct-chat key of each `telegram.owner_ids` id
  (ADR-0033 section 3), so only the owner's direct chat can take an inbox turn. One stderr line
  ends the hold of a key, and the thread moves to the next owner id, when the engine refuses the
  key with a 4xx error, the answer is malformed, the engine runs as another uid, or a newer
  request released the hold and the answer has no `turn`. The held call has no read
  timeout: on a Unix socket a gone engine is an end of stream. A failed connect, a lost hold or
  an `internal` error is tried again 2 s later, the reference pump's period; nothing else repeats
  the call.
- It reads the handed-over turn with `turn_events` as ADR-0034 section 1 reads, and renders it
  quietly (`quiet_config` and the `base.py` hunks): no typing on `start`, no `interim` message,
  no processing reaction, no failure notice or quota line, and the `final` held until `end`.
- At `end` it classifies the answer and reports it with `report_delivery`:

| The turn | Sent | `Delivery` |
|---|---|---|
| Not consumed (section 7) | Nothing | `no_answer` |
| Consumed, `final` exactly `NO_REPLY_DONE` after trimming | Nothing | `silent_done` |
| Consumed, `final` another silence marker (ADR-0034 section 2) | Nothing | `silent` |
| Consumed, any other `final` | Once, as `Outbox::echo` sends (one attempt per part, no delivery retry, no failure notice) but ringing, replying to nothing | `sent` when Telegram answered every part with a message id, else `send_failed` |

- A read that ends without `end` (engine gone, `unknown`) sends and reports nothing. After the
  report the thread holds `inbox_turns` again, so the engine sees the session busy while the
  answer is sent, as the reference holds the session until its send returns.
- `NO_REPLY_DONE` joins the silence markers of ADR-0034 section 2 for every turn, as the
  reference adds it to `LIVE_GATEWAY_SILENT_MARKERS`.

### 7. Consumption and the ack

A turn consumed its event (`Receipt.agent_result`, `no_model_text`) when its `end` outcome is
`completed` or `halted`, it has a `final`, and that text after Python-style trimming is not
empty, not `(empty)` and starts with none of `⚠️ The model returned no response`,
`⚠️ The model produced only internal reasoning` and `❌ Model returned no content`. Against the
reference result:

| Reference | ulei |
|---|---|
| `api_calls` 0 | No `final`: a `final` follows a model reply |
| `failed`, `error` | Outcome `failed`, `context_exhausted` or `quota_exhausted` |
| `partial` | No engine counterpart (ADR-0028 section 1) |
| `interrupted` | Outcome `interrupted`; such a turn has no `final` (ADR-0031 section 3) |
| `completed` false, `turn_exit_reason` `max_iterations_reached` | Outcome `max_steps` |
| `turn_exit_reason` `empty_response_exhausted`, `all_retries_exhausted_no_response`; `_empty_terminal_sentinel` | The `(empty)` `final` of the engine's empty-response guard |
| `final_response` equal to the `_normalize_empty_agent_response` hint | No counterpart: the engine never writes a hint into `final` (ADR-0034 section 2) |

The ack, as `Receipt.invoke` and `Receipt.complete`, with `kind` from the claimed packet:

| C6 facts | C1 call |
|---|---|
| `inject` not accepted, or failed before an accept (PLUGIN.md section 6) | `retry`, 15 s |
| `status` turn `dropped` or `unknown` | `ack` `uncertain` |
| `ended`, `delivery` `no_answer` or `send_failed` | `ack` `uncertain` |
| `ended`, `delivery` `sent` | `ack` `success` |
| `ended`, `delivery` `silent_done`, kind not `weekly` | `ack` `silent` |
| `ended`, `delivery` `silent`, kind not `weekly` | `ack` `deferred` (re-offered after 3,600 s, `deferred-exhausted` after 3 offers) |
| `ended`, `delivery` `silent` or `silent_done`, kind `weekly` | `ack` `silent` |
| `ended`, no `delivery` before the lease runs out | None; `recover` marks it `uncertain` (`expired-processing`) |

`success` therefore needs a send that Telegram confirmed; every other path is `silent`,
`deferred` or `uncertain`. The reference's `uncertain` for a blank consumed weekly answer has no
case here: a consumed answer is never blank.

### 8. C6 0.5.0

Breaking within 0.x, so a minor bump:

- `InjectRequest.payload` is removed; `InjectRequest.prompt` (required, at least 1 character) is
  the model text of section 4, untrusted data. `kind`, `lease`, `attempt` and `event_id` stay.
- `IdleStatus` and `InjectRequest` descriptions: the owner session is free when no turn runs,
  none is queued and the runtime can deliver the turn's answer to the owner; for the engine,
  section 5.
- Method `inbox_turns`: `InboxTurnsRequest` `{session: SessionKey}` to `InboxTurns`
  `{turn?: TurnId}`; `turn` absent only when a newer request released the hold.
- Method `report_delivery`: `DeliveryReport` `{session: SessionKey, turn: TurnId, delivery:
  Delivery}` to `Ack`.
- `Delivery`: `sent`, `send_failed`, `silent`, `silent_done`, `no_answer`.
- `TurnReport.delivery` (optional `Delivery`): present only on an `ended` inbox turn the channel
  reported. The client checks refuse it on any other state.
- A runtime adapter answers `inbox_turns` and `report_delivery` `404 not_found` and fills
  `TurnReport.delivery` itself for the turns `inject` started, by section 6.
- Fixtures for the new definitions, the `InjectRequest` fixtures moved to `prompt`, a
  `CHANGELOG.md` entry.

### 9. Idle budgets

- `ulei-inbox` (`ULEI_INBOX_MAX_IDLE_WAKEUPS`, 0): the `deliver` thread waits with no timeout
  when nothing is due (section 3). `ci/inbox_load.py` measures every thread of `serve --deliver`
  on a root with `ENABLED` and nothing queued, against a fake engine socket the loop never
  calls.
- `ulei-engine` (`ULEI_ENGINE_MAX_IDLE_WAKEUPS`, 0): a held `inbox_turns` waits on a condition
  variable with no timer. `ci/engine_idle.py` holds one during the `serve` idle window.
- `ulei-tg` (`ULEI_TG_MAX_IDLE_WAKEUPS`, 0): the `inbox` thread blocks in the read of a held
  `inbox_turns`. `ci/tg_idle.py` serves a fake engine socket that holds `inbox_turns`
  unanswered; the 2 s retry of section 6 runs only while the engine cannot be reached. A
  running inbox turn adds the `turn_events` re-call of ADR-0034 section 1, not an idle wake-up.

### 10. The reference behaviour table

One row per row of the behaviour table of the reference's R12 copy:

| Behaviour | Reference | ulei |
|---|---|---|
| Single consumer, `ENABLED` gate, pump period 2 s, one fixed log line on a failed tick | `start`, `pump_once` | Section 1; the pump period is a deviation (section 11) |
| One step: in-flight turn only renews; else recover, session lookup and identity checks, busy check, claim, event, session reservation, with no await between the last busy check, the claim and the reservation | `Bridge.tick` | Section 2; the engine decides free and starts the turn under one lock in `inject` (section 5); identity checks: section 11 |
| Busy conditions; no claim while busy, so a busy session spends no offer | `Bridge.blocked` | `is_idle` before `claim` (section 2, step 3); the conditions without a counterpart: section 11 |
| Text given to the model: header line, policy by kind, JSON tail | `Bridge.tick` | Section 4, built in the core |
| `start` at the moment the turn begins | `Receipt.begin`; `run.py` hunk `@@ -19239` | C1 `start` right after the accept (section 2, step 6); deviation in section 11 |
| Was the event consumed: API calls made, no failure, error, partial or interrupt, visible model text | `Receipt.agent_result`, `no_model_text`; `run.py` hunk `@@ -20337` | Section 7, judged by `ulei-tg` from `final` and the `end` outcome |
| Turn not started after a claim: `retry`; failure or cancel after start: ack `uncertain` | `Receipt.invoke`, `Receipt.complete`; `base.py` hunks `@@ -6338`, `@@ -6368` | Section 7, first rows; an interrupted turn ends `interrupted` and is `no_answer` |
| Ack mapping `success`, `silent`, `deferred`, `uncertain`; `success` needs a confirmed send of the final answer | `Receipt.complete`; `base.py` hunks `@@ -5452`, `@@ -6609` | Section 7; the confirmed send travels as `Delivery` `sent` (sections 5, 6) |
| Quiet turn: no streaming, tool progress, thinking progress, interim messages, live status, typing or reactions; final answer sent once without the retry wrapper; no delivery obligation row | `quiet_config`; `run.py` hunks `@@ -27762`, `@@ -28169`; `base.py` hunks `@@ -5452`, `@@ -6312`, `@@ -6576`, `@@ -6609` | Section 6. `ulei-tg` has no streaming, tool or thinking progress, live status, processing reactions or delivery ledger for any turn (ADR-0019, ADR-0034), so those hold already |
| An inbox turn does not take the owner's queued message into itself | `inbox_turn_active`; `run.py` hunk `@@ -29370` | Holds: the engine runs every queued prompt as its own turn (ADR-0028 section 2); section 11 |
| Inbox turns are excluded from the restart auto-resume | `owns_recovery`; `session.py` hunk `@@ -3326` | Holds: the engine has no restart auto-resume; section 11 |
| `NO_REPLY_DONE` is a silence marker | `response_filters.py` hunk | `ulei-tg` silence markers (section 6) and the `silent` ack branch (section 7) |
| Store defaults: claim and renew ttl 120 s, retry delay 15 s, `DEFER_SECONDS` 3600, `MAX_OFFERS` 3 | `owner_inbox_store.py` | Unchanged in `crates/core/src/inbox.rs` (`LEASE_TTL`, `RETRY_DELAY`, `DEFER_SECONDS`, `MAX_OFFERS`); the loop passes `LEASE_TTL` and `RETRY_DELAY` |

The `session.py` hunk `@@ -3589` and the `hermes_state.py` hunk of the same patch store the
inbound platform message id; they are not part of the delivery loop and this record does not
port them.

### 11. Deviations from the reference

| Behaviour | Reference | Why |
|---|---|---|
| No timer while nothing is due: the loop wakes on a publish or the nearest due time; the 2 s tick runs only with a turn in flight or a claimable event waiting | `start` sleeps 2 s between every `pump_once` | 0 idle wake-ups (ADR-0001). |
| `ENABLED` is checked at every wake, so a restored file takes effect at the next publish or due time | `pump_once` checks it every 2 s | The same idle rule. |
| The lease is renewed every 2 s only until the turn has ended; the delivery must be reported within the lease left | `Bridge.tick` renews on every tick until `Receipt.complete` ran, the send included | The loop's timer runs only while a turn runs. The lease left is at least 118 s, and one quiet send can outlast it: a part makes up to three `sendMessage` attempts, and after a markup refusal three more as plain text, each bounded by `telegram.connect_timeout` twice (resolve, connect with TLS), `telegram.write_timeout` per write and `telegram.read_timeout` twice (answer head, body), 80 s at the defaults; between attempts come 1 s and 2 s back-offs, a flood-control `retry_after` the code does not cap, and the 1 s per-chat pace; the parts of an answer are not capped. Without `retry_after` one part takes at most 6 x 80 + 6 + 6 = 492 s at the defaults. A delivery reported after the lease ran out leaves the row to `recover` (`uncertain`, `expired-processing`). |
| Consumer, agent and Telegram send are three processes: the facts of `Receipt` travel as C6 `TurnReport` outcome and `delivery`, and a lost `inject` answer is replayed with the same `attempt` (PLUGIN.md section 6) | `Bridge.tick` hands the event to `adapter._start_session_processing` and the receipt is called back in the same process | The process model of ADR-0008, ADR-0028 and ADR-0031. |
| The turn starts at the `inject` accept and C1 `start` follows; a `start` that fails leaves the turn running, and the row is offered again after its lease (at least once) | `Receipt.begin` marks the row `processing` before the agent runs and stops the turn when the lease is no longer owned | `inject` names the session only in its answer. With one consumer and a 120 s lease, `start` fails only on a store failure. |
| Identity checks: the owner session is `engine.owner_session`; `ulei-tg` holds `inbox_turns` only for the direct chats of `telegram.owner_ids` and the engine refuses other keys; no multi-profile refusal, thread or profile check | `Bridge.tick` checks `multiplex_profiles`, the entry's origin (telegram, owner chat and user, direct chat, no thread, profile), the session key and `_is_user_authorized` | One engine serves one owner session from its configuration; `ulei-tg` has no topics or profiles, and its owner ids are its authorization (ADR-0033). |
| Owner input that `ulei-tg` still holds (the turn coalescer's window, up to `telegram.coalesce_max`; an owner turn's final reply being sent) does not block a claim | `Bridge.blocked` checks `_text_debounce_buffers` and `_active_sessions` | The engine does not see the channel's buffers. Such input reaches the engine as a prompt queued behind the inbox turn and runs as its own turn right after it. |
| No drain, startup-restore, emergency-stop or session-cap condition | `Bridge.blocked` checks `_draining`, `_external_drain_active`, `_startup_restore_in_progress`, `agent.estop.is_engaged` and `_active_session_limit_message` | The engine has no drain or restore (ADR-0028 section 8, ADR-0029 section 3), no emergency-stop switch (the owner's emergency path is `interrupt`, port map R4) and no session cap; a stopping engine refuses connections, which step 3 treats as busy. |
| `inbox_turn_active` has no code of its own | `inbox_turn_active`, through which the `run.py` hunk `@@ -29370` skips `_dequeue_pending_event` in an inbox turn | The engine never merges a queued prompt into a running turn (ADR-0028 section 2). |
| `owns_recovery` has no code of its own | `owns_recovery`, through which the `session.py` hunk `@@ -3326` excludes inbox turns from the `restart_interrupted` resume | The engine resumes nothing after a restart (ADR-0028 section 8); the C1 `recover` marks an unsettled turn `expired-processing`. |
| The root comes from `--root`; no profile-home check | `start` runs only in one profile's home | ulei has no profiles; the deployment names the root. |

### 12. Implementation cards, in order

Each card merges alone with the workspace green.

1. C6 0.5.0 (section 8): schema, fixtures, changelog, generated types; `inject` of the engine
   runs `prompt`, `ulei_runtime_adapters::client` and its tests follow the rename;
   `crates/runtime-adapters/PLUGIN.md` sections 1, 4, 5 and 7 for the new fields and methods.
   The engine answers `inbox_turns` and `report_delivery` `404 not_found` until card 2.
2. Engine (section 5): inbox turns, `inbox_turns`, `report_delivery`, `TurnReport.delivery`, the
   free condition of `is_idle` and `inject`, `RETRY_AFTER_S` 15; `ci/engine_idle.py` holds an
   `inbox_turns` during the `serve` idle window.
3. `ulei-tg` (section 6): `NO_REPLY_DONE`, the `inbox` thread, quiet rendering, the delivery
   classification and report; `ci/tg_idle.py` serves a fake engine that holds `inbox_turns`.
4. Core (sections 1 to 4 and 7): the `deliver` thread of `ulei-inbox serve`, `ENABLED` and
   `consumer.lock`, the publish signal and due-time wait, the C6 client copy, the model text
   checked against golden vectors made by the reference expression over synthetic rows (both
   policies, an integer and a float `source_ts`, non-ASCII and control characters), and the ack
   mapping against a fake C6 server.

## Consequences

- An inbox event reaches the owner only while `ulei-tg` holds `inbox_turns`; with the channel
  down, events wait in the queue unclaimed.
- `ulei-tg` gains an `inbox` thread; `ulei-inbox serve` gains a `deliver` thread.
- The load test of the loop and its budget line are separate work; the idle budgets of section 9
  stay at 0.
