# ADR-0031: C6 0.4.0: a turn's reply to its caller and the turn's context

Status: accepted
Date: 2026-11-19
Affects: contract:C6, crates/engine

## Context

The owner's channel (`crates/channels`) submits a turn with C6 `submit_turn` (ADR-0028) and must
show the owner what the turn says: the assistant messages while it runs, its answer, and when it
ends. C6 0.3.1 returns a turn id and, through `status`, an outcome and a line; nothing carries the
turn's text back. The channel's `Turn` also holds a `context` that reaches only the copy of the
user message sent to the model (ADR-0018), and `SubmitTurnRequest` has no field for it.

The reference (`THIRD_PARTY_NOTICES.md#ref-agent`, pinned 76f6ba3) runs the agent inside its
gateway process and sends to the platform directly: `gateway/run.py` `_interim_assistant_cb`
sends each interim assistant message that `run_agent.py` `interim_assistant_callback` delivers,
marked by `_interim_metadata` as not the turn's final; the final-response path
(`_run_agent_inner`, `_handle_message`, then the adapter's `_process_message_background`) sends
the final answer unless it is empty or a stream already delivered it (`response_previewed`);
typing runs from the start of processing to the answer
(`gateway/platforms/base.py` `_keep_typing`, `_stop_typing_refresh`, `_pause_typing_before_finalize`).
In ulei the channel and the engine are separate processes on the C6 wire (ADR-0008).

## Decision

### 1. C6 0.4.0, no separate contract

The reply is part of C6, version 0.4.0, not a new C9 "turn" contract. The reply belongs to a
turn that C6 `submit_turn` or `inject` accepted and names by its `TurnId`; it is served by the
same `ulei-engine serve` on the same socket and wire, with the same `SessionKey`, `TurnId`,
`TurnState` and `TurnOutcome`. A separate contract would duplicate those definitions and a
second server for one turn. The change is additive: one method, four definitions, one optional
field.

### 2. Wire: a held answer per call, read by cursor

The wire stays ADR-0008: one `POST /v1/<name>` per connection, `Content-Length` bodies,
`Transfer-Encoding` refused, the same limits, peer check and error table.

- `turn_events` (`TurnEventsRequest` to `TurnEventsPage`) reads one turn's events after the
  cursor `after`, the `seq` of the last event the caller holds (0 for none). Events of a turn are
  numbered from 1 without gaps.
- The server answers at once when the turn has an event after `after`, or when it will have no
  more: `ended`, `dropped` or `unknown` (then `events` is empty when none is after `after`).
  Otherwise it holds the answer, the connection open, until the turn's thread appends an event,
  and answers then. The page carries the events after `after` in order, as many as fit the
  512 KiB body limit and at least one. One event's text is one model reply, bounded by the
  route's `max_tokens` (16384 by default), far below the limit; an event that does not fit
  alone is a server failure (`500 internal`) and leaves the turn running.
- A caller follows its turn by calling again with the `seq` of the last event it got, until it
  gets `end` or `dropped`, or reads `unknown`. A caller that bounds its own wait closes and calls
  again with the same `after`; a new request for a turn whose earlier request is still held
  releases the earlier one at once with the events it has, possibly none, so a turn holds at
  most one answer.
- Zero idle wake-ups (ADR-0001): a held answer waits on a condition the turn's thread signals
  when it appends an event, with no timer, and the caller blocks in its read. When no turn runs,
  no request is held: a caller follows only a turn it was given, and every followed turn ends in
  `end`, `dropped` or `unknown`.
- A runtime adapter does not serve `turn_events` and answers it `404 not_found`, as it answers
  `submit_turn` (ADR-0008).

### 3. Events of a turn

`TurnEvent` has `seq`, a `TurnEventType` and the payload of that type.

| `type` | When | Payload |
|---|---|---|
| `start` | The turn starts running: at once when the session is idle, else when it leaves the queue | none |
| `interim` | An assistant message with tool calls is complete, before its calls run | `text` |
| `final` | The turn ended `completed`, `max_steps` or `halted` and the text of its last assistant message is not empty | `text` |
| `end` | The turn ended and its report holds its outcome | `outcome`; `line` as `TurnReport.line` |
| `dropped` | The turn left the queue through `interrupt` without running | none |

- Order: a turn that runs has `start`, any number of `interim`, at most one `final`, then `end`;
  `final` and `end` follow each other with nothing between. A dropped turn has `dropped` alone.
  A queued turn has no event until it starts or is dropped.
- `interim` text is the visible text of the assistant message, as `run_agent.py`
  `_interim_assistant_visible_text` takes it: the text blocks without thinking, trimmed. It is
  not sent when it is empty, when it is `(empty)`, or when the same text with runs of whitespace
  made one space was already sent in this turn (`_normalize_interim_visible_text`,
  `_interim_text_was_delivered`, `_record_delivered_interim_text`).
- `final` text is the text of the message that ended the turn, as the loop returns it: the answer
  for `completed`, the summary for `max_steps`, the policy's answer for `halted`; the `(empty)`
  stand-in of an empty reply is text and is sent. A reply a policy held back and continued
  (ADR-0023 `on_stop`) did not end the turn and is neither `interim` nor `final`. `final` is sent
  even when an `interim` carried the same text.
- `end` carries the turn's `TurnOutcome` and, for `quota_exhausted`, `context_exhausted` and
  `failed`, its `line` exactly as `TurnReport.line` holds it (the quota line of port map R5 as
  is); those turns have no `final`.
- Interrupted turn (ADR-0028 section 3): once its interrupt handle fires, no `interim` and no
  `final` is appended; `end` comes with `interrupted` when the turn is journaled. The queue head
  that `interrupt` drops gets `dropped`.
- A caller that disconnects, times out or never asks changes nothing for the turn: the turn runs
  on, is journaled and appends its events. The server learns of a gone caller only when its
  answer fails to write, and drops that answer, never an event.
- The events of a turn are kept with its report (ADR-0028 section 1): every queued and running
  turn and at least the last 64 ended or dropped turns of each session. Any other turn, and every
  turn after an engine restart, reads `unknown` with no events.
- Every accepted turn has events, `inject` turns included.

### 4. `SubmitTurnRequest.context`

- Optional, at least one character. It is the context of that turn only: appended to the copy
  of the prompt every request of the turn sends, the summary request included, and never
  journaled, never part of `prompt` and never sent on a later turn.
- It joins the turn contexts of the engine's policies (ADR-0022) as one more `pre_llm_call`
  context: the joined context is `context`, then the policies' contexts in chain order, joined
  with `\n\n`, and appended as `compose_user_api_content` appends it:
  `content + "\n\n" + joined`. An input that is not one text block is sent as it is, as
  ADR-0022 states.

### 5. Mapping to the reference

| C6 0.4.0 | Reference | Rule kept |
|---|---|---|
| `start` | `gateway/platforms/base.py` `_process_message_background` starting `_keep_typing` when the handler takes the message | The channel starts its typing refresh (ADR-0019 `TypingRefresh`) on `start` |
| `interim` | `run_agent.py` `interim_assistant_callback` through `_emit_interim_assistant_message`, called after the assistant message with tool calls is appended (`agent/conversation_loop.py`); `gateway/run.py` `_interim_assistant_cb` without a stream consumer | Blank text is not sent; nothing is sent once the run is no longer current (`_run_still_current`); dedup per turn as section 3 |
| `TurnEventType` `interim` against `final` | `gateway/run.py` `_interim_metadata`, the `_interim_send` marker | A mid-turn message is never taken for the turn's answer |
| `final` | The final-response path: `gateway/run.py` `_run_agent_inner` (`final_response`, the empty-sentinel check, `response_previewed` with `_stream_confirmed_final_delivery`) and `_handle_message`, then `gateway/platforms/base.py` `_process_message_background`, which sends nothing for an empty response | Not sent when empty. Without a stream consumer `_stream_confirmed_final_delivery` is false, so `response_previewed` never suppresses the final: the final is always sent |
| Typing stops before the answer is sent | `gateway/run.py` `_pause_typing_before_finalize`, and the typing stop in `_handle_message_with_agent` after `_run_agent` returns | The channel stops typing on `final` before it sends, and on `end` |
| `end` | `gateway/platforms/base.py` `_stop_typing_refresh` after the handler returns; outcome and line of ADR-0028 section 1 | Typing ends with the turn whatever its outcome |
| `dropped` | `gateway/run.py` `_interrupt_and_clear_session` discarding the queue head (`get_pending_message`) | The dropped prompt gets no answer |
| Interrupted turn: no `interim` and no `final` after the interrupt | `gateway/run.py` `_is_session_run_current` false after `_invalidate_session_run_generation`: `_interim_assistant_cb` returns early and the stale result is discarded | Nothing of an interrupted run reaches the owner after the stop |
| `context` | `agent/turn_context.py` `compose_user_api_content` and `build_turn_context` (ADR-0022) | API copy only, appended after a blank line |

The texts the reference gateway builds from a finished result before it sends (the silence
markers of `gateway/response_filters.py` `is_intentional_silence_agent_result`, the `(empty)`
notice and `_normalize_empty_agent_response`) are owner-facing texts of the channel, made from
`final`, `outcome` and `line`; this contract carries those inputs unchanged.

### 6. Deviations from the reference

| Behaviour | Upstream | Why |
|---|---|---|
| The caller pulls the turn's events from the runtime by cursor; the runtime never sends to the platform | `gateway/run.py` `_interim_assistant_cb` and the final-response path send through the platform adapter of the same process | The channel and the engine are separate processes and C6 makes the runtime the server (ADR-0008); a cursor lets a channel that restarts mid-turn read what it missed. |
| A send the caller missed is read again; nothing is lost while the turn's report is kept | `gateway/platforms/base.py` send failures are logged and the message is gone | A caller disconnect must not lose the turn's answer. |
| An `interim` reaches the caller before its step is journaled | `agent/conversation_loop.py` emits interim commentary only after the canonical session append | The engine journals a turn once, at its end (ADR-0014); waiting for that would hold every interim to the end of the turn. |
| A failed turn's answer is `line` in `end`, not a `final` text | `gateway/run.py` `_handle_message_with_agent` through `_normalize_empty_agent_response` writes the failure into the final response | ADR-0028 already carries the one line of `quota_exhausted`, `context_exhausted` and `failed`; the owner-facing wording belongs to the channel. |
| No notice for a turn interrupted before its first model call, nor for a turn that never ran | `gateway/run.py` `_normalize_empty_agent_response` notices for zero `api_calls` | Fix of known reference failures: they cover an interrupt flag left from an earlier stop that kills the next message, and a race of a new message with a stale run generation. The engine's interrupt handle belongs to one turn and its queue reports a turn it does not run as `dropped`. |
| The channel's `context` is the first of the turn's contexts | `agent/turn_context.py` `build_turn_context` joins `pre_llm_call` results in plugin load order | The channel's context stands for a profile plugin, whose load order is deployment data; ulei fixes one order. |
| No stream deltas, segment breaks or streamed-final suppression | `gateway/stream_consumer.py` `GatewayStreamConsumer`, `already_streamed`, `response_previewed` | Streaming edits are not ported (ADR-0019); the reference without a stream consumer behaves as section 5 states. |
| No long-running heartbeat notices | `gateway/run.py` `long_running_notifications` (on for Telegram in `gateway/display_config.py`) | No port-map row names them; a later C6 may add an event type. |

## Consequences

- `ulei-engine serve` serves `turn_events` and appends `context` in the next change; until then
  it answers `turn_events` `404 not_found` and a `submit_turn` with `context` `400 bad_request`,
  as C6 0.3.1 did. Today it serves one connection at a time; a held answer needs a thread of its
  own so it never delays another method.
- The agent loop gets a hook for `interim` and the server keeps each turn's events with its
  report; the budgets of `ulei-engine` (ADR-0010) bound the cost.
- The channel's C6 client follows each turn it submitted with `turn_events` and renders by
  section 5.
