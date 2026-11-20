# ADR-0028: C6 0.3.0: engine session methods, turn identity and inbox delivery

Status: accepted
Date: 2026-11-18
Affects: contract:C6, contract:C1, contract:C8, crates/engine, dep:rustix
Amended by: ADR-0036 (the inbox delivery: `inject` runs the core's model text, the free owner session, the retry delay, turn-end detection and the ack); ADR-0037 (the durable delivery record of `inject`, C6 0.6.0, C8 0.5.0).

## Context

C6 0.2.0 carries `inject` and `is_idle` from the core to a runtime adapter (ADR-0008). The
engine `ulei-engine` will serve C6 itself, and its callers need more than inbox delivery: run
an owner prompt in a session, stop the running turn at once (port map R4), read the session's
state and switch its model. The reference (`THIRD_PARTY_NOTICES.md#ref-agent`, pinned
76f6ba3) has no wire API for these; its gateway holds them in `gateway/session_state.py`,
`gateway/run.py` and `gateway/slash_commands.py`, and this record maps each C6 method onto
those parts. In ulei a model is a qb route alias (port map R1).

C6 0.2.0 also left six gaps for the inbox path: the `lease` wording, no redelivery of a
deferred event, no turn result, no turn-end rule after a replay, the Python stdlib error
statuses and the C1 description of who consumes.

## Decision

The wire stays ADR-0008: HTTP/1.1 JSON over a Unix socket, one request per connection,
`SO_PEERCRED` on both sides, the same limits and error table. The engine is the server; the
core and other local callers are clients.

### 1. Sessions and turns

- A session is addressed by its key (`SessionKey`), the run CLI's `--session` and C8
  `Session.key`. The key names its current session, the C8 session with the highest `id`. A
  method that runs a turn or stores a route creates the key's session when it has none, as
  the run CLI does; `status` and `interrupt` never create one.
- `submit_turn` with `new` true starts a new C8 session of the key when that turn starts, as
  `--new` does; earlier sessions stay in the journal. A new session has no stored route.
- Each accepted turn gets a `TurnId` when it is accepted, whether it starts or is queued. No
  other turn of the same engine journal ever gets the same id, across restarts. It is the wire
  form of the reference's run generation (`gateway/run.py` `_begin_session_run_generation`,
  `PersistentState.run_generation`); see section 8 for the difference. The engine's id is
  `<start>-<n>`: the engine's start in microseconds since the Unix epoch and the turn's count
  of acceptance since that start. A restart starts at a later microsecond, so its ids are new
  as long as the host clock is not set back past an earlier start.
- `TurnReport` gives a turn's `TurnState` and, once ended, its `TurnOutcome`, end time and line.
  The engine holds the report of every queued and running turn and of at least the last 64
  ended or dropped turns of each session, in memory; any other turn, and every turn accepted
  before an engine restart, reads `unknown` when `status` names it. Section 4 says what
  `last_turn` reads after a restart.

Turn states, against `gateway/session_state.py`:

| `TurnState` | Engine | Reference |
|---|---|---|
| `queued` | In the session's queue | The event in the adapter's `_pending_messages` slot or in `ConversationState.queued_events` |
| `running` | The turn's loop runs, or an interrupted turn unwinds | `TurnState.agent` set, `TurnState.started_ts` |
| `ended` | The loop returned and the turn is journaled | `TurnState.clear` after the run |
| `dropped` | Left the queue through `interrupt` | The slot event `_interrupt_and_clear_session` discards |
| `unknown` | Not held | None |

Turn outcomes, against the engine's `Ended` and the reference run result
(`agent/turn_finalizer.py`):

| `TurnOutcome` | Engine `Ended` | Reference result |
|---|---|---|
| `completed` | `Stopped` | `completed` true |
| `max_steps` | `MaxSteps` | `completed` false after the iteration budget (`turn_exit_reason`) |
| `halted` | `Halted` | `completed` true; the reference has no separate value for a turn a plugin hook ended |
| `interrupted` | `Interrupted` | `interrupted` true |
| `context_exhausted` | `ContextExhausted` | `failed` true, `Context length exceeded: max compression attempts (3) reached.` |
| `quota_exhausted` | `QuotaExhausted` | `failed` true with the one quota line (port map R5) |
| `failed` | `Gateway`, `Transport`, or an engine failure before or after the loop | `failed` true |

The reference's `partial` (invalid tool calls) has no engine counterpart. `line` carries the
one line the run CLI prints on stderr for `quota_exhausted` (the U1 `quota_line`: route and
nearest reset in UTC), `context_exhausted` and `failed`.

### 2. `submit_turn`

`SubmitTurnRequest` to `SubmitTurnResponse`. Reference: `gateway/run.py` busy handling with
`busy_input_mode: queue` (the profile default), `_queue_or_replace_pending_event`,
`_enqueue_fifo`, `_promote_queued_event`, `_queue_depth`, `_dequeue_pending_event`.

- The session is idle: the turn starts; `position` 0.
- The session is busy: the prompt joins the session's FIFO queue as its own full turn, never
  merged with another; `position` is its place from 1. When a turn ends, the queue head
  starts.
- The queue holds at most 32 turns (`_BUSY_QUEUE_MAX_PENDING`). A prompt over the cap is
  refused: `accepted` false, `queue_depth` 32, no turn id.
- `channel` is the run CLI's `--channel`; the sender is the owner, as a run without `--event`.
  The turn runs on the engine's default route until `set_route` stores one (section 5).
- Turns of different sessions run at the same time, each session's turns on a thread of its
  own, as the reference runs one agent per session key. A session never runs two turns at
  once, so two turns never write one session's history together.
- `submit_turn` is not idempotent: a caller whose call timed out reads `status` before it
  submits again.

### 3. `interrupt`

`InterruptRequest` to `InterruptResponse`. Reference: `gateway/run.py`
`_interrupt_and_clear_session` with `_INTERRUPT_REASON_STOP`, reached from
`gateway/slash_commands.py` `_handle_stop_command`; `run_agent.py` `interrupt` with
`hard_cancel` (`hard_interrupt`); `tools/environments/local.py` `_kill_process`; port map R4.

At the pin, after the hard interrupt and the generation bump, the reference function does this
to the pending and queued items:

```python
if adapter and hasattr(adapter, "get_pending_message"):
    adapter.get_pending_message(session_key)  # consume and discard
if _iac_state is not None:
    _iac_state.persistent.pending_command_text = None
```

`get_pending_message` pops the adapter's one `_pending_messages` slot, which in queue mode holds
the queue head (`_enqueue_fifo`); `ConversationState.queued_events`, the rest of the queue, is
not touched. The engine does the same:

- When a turn runs, it is hard-interrupted: the process group of every running tool call gets
  SIGTERM and then SIGKILL as `_kill_process` does (`tools::Interrupt`), and the running
  gateway turn is cancelled. The turn's interrupt handle stays interrupted, so no later step or
  tool call of that turn runs; this is the generation invalidation of
  `_invalidate_session_run_generation`.
- The queue head, the turn that would run next, is dropped and reported in `dropped`. The other
  queued turns stay and run in order after the interrupted turn has ended.
- The answer comes once the interrupt is delivered; it does not wait for the turn to unwind.
  `status` shows the session running until the interrupted turn is journaled with outcome
  `interrupted`.
- An idle session, or one whose running turn is already interrupted, answers `interrupted`
  false and changes nothing; it is not an error (`_handle_stop_command` answers "no active").
  So does a session whose turn has finished: its loop and journal append have returned, the
  server has finished the turn's interrupt handle and the next queued turn has not started
  yet. That turn reports its own outcome and the queue head stays. The server decides the
  outcome it reports as it finishes the handle, under the handle's lock, so an `interrupt`
  that claimed the turn before ends it `interrupted`, whatever its loop returned.
- The session and its C8 history stay, the messages of the interrupted turn included. A later
  `inject` or `submit_turn` runs its turn in the same history, with no restart
  (`_handle_stop_command`: "The session is preserved").
- The interrupt handle shuts the turn's gateway stream down at once, and the closed stream is
  what cancels the turn at qb, with no event. The turn's own thread then posts
  `POST /v1/model/cancel` as it unwinds, so the C6 answer never waits for qb. qb answers the
  post once the turn has ended or its own 5 s wait has passed, always with `cancelled: true`,
  and with 404 for a turn that is unknown or has already ended, the usual answer once the
  closed stream has cancelled it. The engine keeps the post, bounded, as a best-effort end
  signal that holds only when qb ends the turn within `gateway::CANCEL_TIMEOUT`, 2 s: qb
  answers before its 5 s wait only once the turn has ended, so an answer, 200 or 404, inside
  the bound means qb has ended the turn and released its lease, and the next turn of the
  session does not start while qb still runs the interrupted one. Past the bound the engine
  goes on without knowing; the closed stream tells nothing about when the turn ends. The post
  waits at most 2 s for its connect and for qb's answer, so the interrupted turn, and the exit
  of `run`, wait for qb no longer than that. Callers ignore the result; a 404 is not logged.
  `ulei-engine run` interrupts its turn the same way on SIGINT, SIGTERM or SIGHUP (ADR-0029).

### 4. `status`

`StatusRequest` to `SessionStatus`. Reference: `gateway/session_state.py` `TurnState`
(`agent`, `started_ts`), `gateway/run.py` `_queue_depth`, `ConversationState.model_override`
with `_apply_session_model_override`, and the turn result of section 1.

- `running` and `since`: the start of the running turn, else the end of the last turn, else the
  engine's start.
- `queue_depth`: as `_queue_depth`, every queued turn.
- `route`: the alias the next turn runs on, the stored one or the engine's default.
- `last_turn`: the turn that ended last, with its outcome, end time and, for
  `quota_exhausted`, the quota line. After an engine restart, until a turn of the session ends,
  it is the last turn the journal holds for the key: the highest turn of the key's newest
  session that has one, named `s<session>-t<turn>` like the prefix of its steps' request ids,
  with state `ended`. The journal holds neither the outcome nor the end of a turn, so
  `outcome`, `ended` and `line` are absent, and `since` is the engine's start. A key without a
  journaled turn has no `last_turn`.
- `turn`: the report of the turn the request names, so a caller follows one turn to its end.

### 5. `set_route`

`SetRouteRequest` to `RouteSet`. Reference: `gateway/slash_commands.py` `_handle_model_command`
with its default scope `--session`, `ConversationState.model_override`,
`_apply_session_model_override`, and the persisted override that `/new` clears
(`_rehydrate_session_model_override`); `switch_model` becomes `set_route` (port map R1).

- The alias is stored with the key's current session as C8 `Session.route` (C8 0.4.0) and
  survives an engine restart. A new session of the key starts without one and runs on the
  engine's default route.
- Every turn that starts after the call runs on the stored alias, queued turns included; the
  running turn keeps its route. A turn reads the alias when it opens its session at its start,
  as the reference applies the override when it builds the run's agent.
- `set_route` checks only the alias syntax of C4G `TurnRequest.route` (schema, else
  `400 bad_request`). An alias qb does not know is refused at the next turn, as the engine's
  `--route` is today: the gateway answers `bad_request` and that turn ends `failed`; the alias
  stays stored until the next `set_route`.
- `--once` and `--global` are not in C6 0.3.0 (section 8).
- `ulei-engine run --route` runs its one turn on the alias it names, whatever the session
  stores, and leaves the stored alias as it is. A journal without a stored route runs `run`
  as before the field existed.

### 6. `inject` and `is_idle` (amended by ADR-0036)

- `inject` and `is_idle` address the engine's owner session: the session key the engine's
  configuration names for inbox events. `InjectResponse.session_id` is that key.
- `inject` starts a turn only when the owner session is free (ADR-0036 section 5): no turn runs,
  none is queued, a channel holds `inbox_turns` for it and the key has a session in the journal.
  Otherwise it answers `accepted` false with `retry_after_s` and starts nothing; it never joins
  the queue. The turn's sender is an inbox event: the turn is the run CLI's `--event` turn whose
  `--prompt` is `InjectRequest.prompt`, the model text the core built (ADR-0036 section 4),
  without `--channel` or `--new`. `retry_after_s` is 15 (`serve::RETRY_AFTER_S`), the C1 retry
  delay the core uses.
- So `inject` and `submit_turn` share the session's one running turn and its order, but not the
  queue: an event never overtakes a queued owner prompt, and a prompt submitted while an
  injected turn runs is queued behind it. The reference routes every busy input through
  `_queue_or_replace_pending_event`; section 8 records the difference.
- `is_idle` is true when the owner session is free in that sense.

### 7. The C6 gaps of the inbox path

1. `InjectRequest.lease` is data only: the runtime never calls the inbox with it, never acks
   and never settles the event.
2. `InjectRequest.attempt` (required) is the C1 `Row.attempts` of the core's claim.
   Idempotency is on `event_id`: the engine records every accepted event in its journal (C8
   `InboxDelivery`, ADR-0037 section 3) with the highest accepted `attempt`, the turn it
   started and that turn's `Delivery` once reported, committed before the turn starts and kept
   across restarts. An `inject` of a recorded `event_id` answers the recorded `session_id` and
   `turn` and starts nothing, unless its `attempt` is higher and the recorded delivery is
   `silent`: an event acked `deferred` comes back on a later claim with a higher `attempt` and
   becomes a new turn. Every other higher `attempt` follows a lost lease, so the core follows
   and acks the recorded turn. A recorded turn the engine no longer holds reads `unknown`.
3. The turn's outcome and the channel's delivery of its answer reach the core through
   `status`: `InjectResponse.turn` names the turn, and `status` with that `turn` reports its
   `TurnOutcome` and, once the channel reported it, `TurnReport.delivery`. The ack mapping is
   ADR-0036 section 7.
4. Turn-end detection: the core calls `status` with the session and turn of the accept every
   2 s until the turn's state is `ended` with a `delivery`, `dropped` or `unknown`, and renews
   the lease until it has seen the turn ended (ADR-0036 section 2). After a client timeout of
   `inject` the core replays it with the same `event_id` and `attempt` while it holds the lease;
   the replay answers the recorded turn, so no time comparison is needed. When the lease is lost
   before an answer, the next claim has a higher `attempt` and the engine answers it with the
   recorded turn too (item 2), so a lost lease starts no second turn.
5. `crates/runtime-adapters/PLUGIN.md` section 3.3 maps the Python stdlib `send_error` statuses
   to C6 codes: 501 to `404 not_found`, 414 and 431 to `413 too_large`.
6. The C1 description (C1 1.1.1) says the core consumes the inbox and the runtime only
   receives `inject`.

`crates/runtime-adapters/PLUGIN.md` is the specification of the core-side flow and of what a
runtime adapter of a foreign runtime serves: `inject`, `is_idle` and `status`. `submit_turn`,
`interrupt` and `set_route` are served by the engine only; an adapter answers them
`404 not_found`.

### 8. Deviations from the reference

| Behaviour | Upstream | Why |
|---|---|---|
| A prompt over the 32-turn cap is refused in the answer (`accepted` false, `queue_depth` 32) | `gateway/run.py` `_queue_or_replace_pending_event` drops it with a log warning | A wire caller must learn that its prompt will not run. |
| `inject` never queues: a busy session answers `accepted` false | `gateway/run.py` `_queue_or_replace_pending_event`, the entry of every busy input in queue mode | The durable inbox holds the event and offers it again; a queued event would outlive its lease, and the in-memory queue does not survive a restart. |
| A photo or media burst is not merged into the queue head; every item is its own turn | `gateway/run.py` `_queue_or_replace_pending_event` through `merge_pending_message_event` | C6 prompts are text; there are no media events to merge. |
| The queue does not survive an engine restart; its turns read `unknown` | `gateway/run.py` `_queue_during_drain_enabled` keeps follow-ups across a gateway restart in queue mode | The engine has no drain and restart protocol in this contract; a caller sees `unknown` and submits again. |
| The turns left in the queue after `interrupt` run as ordinary turns, in order, before prompts submitted later | `gateway/run.py` `_run_agent_inner` drain (`_dequeue_pending_event`, `_promote_queued_event`) runs them under the invalidated generation, so `_handle_message_with_agent` discards their result as stale and `track_agent` skips their promotion | A fix of a known reference failure: a kept item must run and report like any turn. |
| The session stays busy until the interrupted turn is journaled; a prompt in that window is queued, an `inject` is refused | `gateway/run.py` `_interrupt_and_clear_session` with `release_running_state` frees the slot at once and `_evict_cached_agent` lets the old run unwind beside a new one | Two turns never write one session's history at once. The window is bounded by `_kill_process`'s grace and the gateway cancel. |
| `TurnId` is opaque, given at acceptance, and unique across restarts; the interrupted turn keeps its id | `gateway/run.py` `_begin_session_run_generation`, `_invalidate_session_run_generation`: an in-memory counter bumped at each run start and at each invalidation | The caller polls a turn by id after a restart or a replay; the per-turn interrupt handle does what the bump does inside the engine. |
| `status` reports a named turn and the engine keeps the last 64 ended reports per session | None; the reference reports a turn only into its chat | Gaps 3 and 4: the core needs the outcome of its own turn after later turns ran. |
| An alias qb does not know is stored and refused at the next turn | `gateway/slash_commands.py` `_handle_model_command` through `switch_model` checks the model before it stores the override | qb owns the routes (port map R1); the engine sends `--route` unchecked today and keeps one check, the gateway's. |
| No `--once` scope | `gateway/slash_commands.py` `_handle_model_command` `--once` with `_snapshot_session_model_override` and `_restore_session_model_override` | No caller in this contract switches for one turn; a scope field is an additive change of a later C6. |
| No `--global` scope | `gateway/slash_commands.py` `_handle_model_command` `--global` writes `config.yaml` (`resolve_persist_behavior`) | The default route is deployment configuration, which the engine reads and never writes. |
| The 2 s wait before the step a compaction after `context_too_long` retries ends at once on the turn's interrupt | `agent/conversation_loop.py` sleeps `time.sleep(2)` between compression retries | An interrupt stops the turn at once (port map R4); every other wait of the turn already ends on it. |
| The retry waits of a first MCP connect end at once on the turn's interrupt, and the turn ends `interrupted` | `tools/mcp_tool.py` `MCPServerTask.run` retries the first connect on the MCP event loop, outside any turn | The engine connects at the start of each turn (port map R3), so a wait there would hold an interrupted turn for the whole retry ladder. |
| `run` exits once its interrupted turn is journaled | `cli.py` `_signal_handler_q` waits a fixed grace of 1.5 s after `request_hard_interrupt` and raises `KeyboardInterrupt` | The turn reaches the journal and its tool groups are gone before the process exits; the unwind is bounded by `_kill_process`'s grace and the gateway cancel. |
| `serve` ends at once on SIGINT and SIGTERM; its running turns are neither interrupted nor journaled | `gateway/run.py` `shutdown_signal_handler` schedules `runner.stop()` | The engine has no drain and restart protocol in this contract (the queue row above); ADR-0029 section 3. |
| Steer and redirect are not in this contract | `run_agent.py` `steer`, `redirect`; `agent/agent_runtime_helpers.py` `apply_pending_steer_to_tool_results`; `busy_input_mode: steer` | Not in this delivery; the owner's emergency path is `interrupt` followed by a new turn in the same session (port map R4). |

### 9. Dependencies of the server

The server is `crates/engine/src/serve.rs` behind `ulei-engine serve`; no crate is new to the
engine. It reads the peer uid with `rustix::net::sockopt::socket_peercred`, the `net` feature
of `rustix` that ADR-0008 section 4 admitted for `crates/runtime-adapters` and that the engine
compiles through the workspace features:

| Crate | Features | Why |
|---|---|---|
| `rustix` | `net`, `process` | `SO_PEERCRED` uid of a C6 peer (`net::sockopt::socket_peercred`) and the engine's effective uid (`process::geteuid`); `UnixStream::peer_cred` is unstable and `getsockopt` through `libc` needs `unsafe`. |

## Consequences

- C6 0.3.0 makes `InjectRequest.attempt` required, so an `inject` caller sends the claim's
  attempt count; `ulei_runtime_adapters::client` passes the request through unchanged.
- The engine's C6 server, its per-session queue, the interrupt wiring and the storage of
  `Session.route` implement sections 1 to 6; the core's consumer implements section 7 through
  `crates/runtime-adapters/PLUGIN.md`.
