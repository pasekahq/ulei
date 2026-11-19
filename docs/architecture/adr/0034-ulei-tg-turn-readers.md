# ADR-0034: `ulei-tg` reads its turns: readers, rendering, stop and the typing thread

Status: accepted
Date: 2026-11-19
Affects: crates/channels

## Context

`ulei-tg` submits each owner turn with C6 `submit_turn` on its `engine` thread (ADR-0033) and
must show the owner what the turn says. C6 0.4.0 serves a turn's events by cursor with a held
answer (ADR-0031 sections 2 and 3); `tg::Outbox` delivers texts and drives the typing indicator
(ADR-0032). The reference (`THIRD_PARTY_NOTICES.md#ref-agent`) renders a turn inside its gateway:
`gateway/run.py` `_interim_assistant_cb` sends interim assistant messages
(`interim_assistant_messages`, on for Telegram in `gateway/display_config.py`), the final-response
path (`_handle_message_with_agent`, `_normalize_empty_agent_response`, then
`gateway/platforms/base.py` `_process_message_background`) sends the answer, and `_keep_typing`
refreshes the indicator concurrently with every send, `_send_with_retry` back-off included.

## Decision

### 1. Readers

- Every turn the engine accepts goes to the `follow` thread of its chat, started with the chat's
  first accepted turn. The thread reads its chat's turns one at a time in the order they were
  accepted, which is the order the session runs them, so a turn's `start` never overtakes the
  previous turn's `end`. With no turn to read it sleeps in its channel's receive.
- The `engine` thread only hands the `TurnId` over; `/stop`, `/status` and the next submit never
  wait for a held `turn_events`.
- A reader calls `turn_events` with `after` 0, then with the `seq` of the last event it got,
  until `end` or `dropped`, or an empty page in `ended`, `dropped` or `unknown`. A request is
  held only for a turn the reader was given, so none is held when no turn runs.
- Wait bound: `telegram.engine_call_timeout` (ADR-0033 section 2, default 10 s) bounds each
  `turn_events` call like every other C6 call. A call that reaches it, at connect or while held,
  is closed and made again with the same `after` (ADR-0031 section 2); while a turn runs that is
  one wake-up per bound, next to the typing cadence's one per 2 s.

### 2. Rendering

| Event | Owner sees |
|---|---|
| `start` | `Outbox::turn_started`: typing from now on |
| `interim` | The text through `Outbox::deliver` as an interim: no reply target, silent, `typing` again after it (`_interim_assistant_cb` without a stream consumer, `interim_assistant_messages` on) |
| `final` | `Outbox::turn_ended`, then the text as the final reply to the turn's message; a silence marker sends nothing; the `(empty)` stand-in becomes the reference's no-response notice (`gateway/run.py`, the `"(empty)"` sentinel check) |
| `end` `completed`, `max_steps`, `halted` | `turn_ended`; the answer was the `final`, and without one nothing is sent |
| `end` `interrupted` | `turn_ended`; nothing (an interrupted run's silence is intentional in `_normalize_empty_agent_response`; ADR-0031 section 6 for a run without model calls) |
| `end` `quota_exhausted` | `turn_ended`; `line` as is, one final reply (port map R5); the turn is not submitted again |
| `end` `failed`, `context_exhausted` | `turn_ended`; the failed branch of `_normalize_empty_agent_response` over `line`: the session-storage texts when it names session storage, the context-window text when it names context, tokens, size, length, excess or payload, else `The request failed: ` with its first 300 characters and `Try again or use /reset to start a fresh session.` |
| `dropped` | Nothing |

Every reply goes through `Outbox::deliver` with its retries (ADR-0032 section 7); no event is
read again to resend it.

A silence marker is a `final` text that is exactly `[SILENT]`, `SILENT`, `NO_REPLY` or
`NO REPLY` after trimming, upper-casing and collapsing whitespace, either as it is or without the
Unicode punctuation (general category `P`) at its ends, square brackets excepted; a text over 64
characters never is one (`gateway/response_filters.py` `is_intentional_silence_response`). The
owner gets no message and no other line for it, and typing ends as for any `final`. A blank text
is not a marker, and prose that mentions one is delivered. The reference checks it on the
completed-turn path of `_handle_message_with_agent` and before sending the first response of a
queued follow-up; both are this `final`, as every turn here is read to its own `final`. The
journal keeps the marker.

### 3. Engine gone

Any `turn_events` failure but the wait bound (no socket, refused, reset, a closed connection, a
malformed answer, a C6 error), and a read of `unknown` (the engine restarted and forgot the turn),
ends the read: typing ends and the owner gets the ADR-0033 section 5 notice once, as the final
reply to the turn's message. The reader then reads the chat's next turn.

### 4. Stop

`Link::finish` waits until every queued C6 call is made, then cancels every reader: the held
connection is shut down, the reader ends typing for its turn and ends, and its thread is
joined. A cancelled reader drops its `Outbox` clone, so `Outbound::finish` returns once the
queued messages are sent. A turn the engine still runs is not read again after the next start.

### 5. Typing thread (amends ADR-0032 sections 2 and 4)

The typing cadence runs on a `typing` thread of its own instead of the `outbound` thread, on
the `Clock` of the shared `Sender`, so a delivery that sleeps in `_send_with_retry` back-off or
flood pacing holds no `typing` back, as in the reference. The thread owns the per-chat
`TypingRefresh` state. The `typing` after an interim reply is handed to it by the `outbound`
thread, which waits until it is sent or skipped, so it never follows the next message. In the
same way `Outbox::turn_ended` returns only once the thread has stopped the turn's refresh, and
the reader delivers the turn's final reply after it, so no `typing` of the turn reaches Telegram
after that reply.

### 6. Differences from the reference

| Reference (module) | Here | Why |
|---|---|---|
| The agent runs in the gateway process; there is nothing to wait for | A held `turn_events` is bounded by the call timeout and called again | The engine is another process (ADR-0031 section 2); the bound keeps a stuck socket from holding the reader for good. |
| `gateway/response_filters.py` `is_autonomous_silence_response`, the loose marker match of cron and webhook lanes | Not ported | `ulei-tg` has no such lane. |
| `gateway/run.py` `_sanitize_gateway_final_response` removes lone UTF-16 surrogates | Not ported | No counterpart: a Rust `String` cannot hold one. |
| `_sanitize_gateway_final_response` suppresses the interrupt sentinel | Not ported | No counterpart: an interrupted turn has no `final` (ADR-0031). |
| `_sanitize_gateway_final_response` redacts secrets and reduces provider failures to short categories | Not ported: the `final` text and the failure `line` go out as section 2 states | Deferred to issue:fdf938e33c5a37fd24b1, until the gateway contract is re-vendored and the real failure texts are known. |
| `_normalize_empty_agent_response` sends `Processing completed but no response was generated` for an empty answer after model calls | A turn without `final` sends nothing | The engine's empty-response guard turns every empty answer into the `(empty)` `final` (ADR-0031 section 3), which shows the reference's notice; a turn left without `final` is the empty response `_process_message_background` does not send. |
| The context branch also matches `400` with over 50 history messages | Only the wording | The channel does not know the history length. |
| A turn the gateway loses has no answer; the gateway is the agent | `unknown` gives the failure notice | A restarted engine forgets the turn (ADR-0031 section 3); the owner must learn that the message got no answer. |
| A gateway stop ends the agents running in it | A stop leaves the engine's turn running and unread | The engine outlives `ulei-tg` restarts; reading a missed turn after a start needs the followed turns kept across restarts, which no state of `ulei-tg` holds. |

## Consequences

- `ulei-tg` threads: `poller`, the loop, `signals`, `engine`, `outbound`, `typing`, and one
  `follow` thread per owner chat that sent a turn.
- `inject` turns of the owner session are not read by `ulei-tg`: it follows only the turns it
  submitted.
