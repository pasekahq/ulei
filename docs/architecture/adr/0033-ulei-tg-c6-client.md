# ADR-0033: the C6 client of `ulei-tg`: wire, timeouts, session key and owner commands

Status: accepted
Date: 2026-11-19
Affects: crates/channels, dep:rustix

## Context

`ulei-tg` (ADR-0032) hands every ingress `Output` to one seam on its loop thread. Turns and the
owner commands `/new`, `/stop`, `/status`, `/model` must reach `ulei-engine serve` over C6
(ADR-0028, ADR-0031 section 4), whose texts the ingress already ports (`run_command`,
ADR-0018). The reference (`THIRD_PARTY_NOTICES.md#ref-agent`) runs the gateway and the agent in
one process: it keys sessions in `gateway/session.py` `build_session_key`, queues a busy
session's input with `busy_input_mode: queue` in `gateway/run.py`, and answers the commands in
`gateway/slash_commands.py` (`_handle_reset_command`, `_handle_stop_command`,
`_handle_status_command`, `_handle_model_command`). A C6 client already exists in
`crates/runtime-adapters`, which `ulei-channels` may not depend on (ADR-0001).

## Decision

### 1. Client

`ulei_channels::c6::Client` speaks the ADR-0008 wire on the generated `ulei-contracts` C6
types: one `POST /v1/<method>` per connection, canonical JSON, the 8 KiB head and 512 KiB body
limits, `Content-Length` framing read to the end of the stream, and the `SO_PEERCRED` check that
the engine runs as the client's effective uid before a byte is sent. It serves `submit_turn`,
`interrupt`, `status` and `set_route`; a non-200 answer is its C6 `Error`.

It is a copy of the framing of `ulei_runtime_adapters::client`, not a shared crate: a module
crate may depend only on `ulei-contracts`, which holds no logic (ADR-0001), and a third crate
for about 250 lines of framing costs more than the copy. The copy differs in its two timeouts
and in its answer checks, which cover only what the channel reads.

### 2. Timeouts

| Key | Default | Bounds |
|---|---|---|
| `telegram.engine_connect_timeout` | 2 s | The connect, as the socket's send timeout before `connect`: a Unix connect waits only while the listen backlog is full. |
| `telegram.engine_call_timeout` | 10 s | The whole call from connect to the end of the answer, as the socket's read and write timeouts. |

The call timeout is above the engine's 5 s request timeout, which also bounds its journal reads
and writes for `status` and `set_route`; every C6 method the channel calls answers without
waiting for a turn. A timed-out `submit_turn` is not repeated (ADR-0028 section 2).

### 3. Session key and channel

The owner chat's session key is the reference's key of a direct chat in the default namespace,
`agent:main:telegram:dm:<chat id>` (`build_session_key`). Every turn carries `channel`
`telegram`, the reference's platform name; a deployment that wants the chat's turns to be
top-level owner turns sets `engine.main_channel` to it (ADR-0022).

### 4. Turns and commands

| Input | C6 | Owner sees |
|---|---|---|
| A turn | `submit_turn` with the turn's text as `prompt`, its `context` (absent when empty) and `new` after `/new` | Nothing here; replies are `turn_events` (ADR-0031) |
| A turn while the session runs one | The same `submit_turn`; the engine queues it (`busy_input_mode: queue`, ADR-0028 section 2) | Nothing; the client never drops a turn |
| `/new` | `status`; `interrupt` when a turn runs; the next accepted turn carries `new` true | The reset banner when the key has a running, queued or ended turn, else the new-session banner |
| `/stop` | `interrupt` | The stopped text when a turn was interrupted, else the nothing-to-stop text |
| `/status` | `status` | Session key, last activity (`since`), route, running, queue depth |
| `/model` | `status` | The route |
| `/model <alias>` | `set_route` | The switched text, or `Ошибка: ` and the engine's C6 error message |

Every reply goes out through `Outbox::deliver` as the final reply to the command message.

### 5. Failures

An engine that cannot be reached (no socket, connection refused, timeout, wrong peer uid), a
malformed answer, a C6 error answer, or a refused turn (a full queue) gives the owner one notice,
the reference's answer to a failed agent run (`gateway/run.py` `_handle_message_with_agent`)
without a status hint, as the final reply to the message: `Sorry, I encountered an unexpected
error.` and `Try again or use /reset to start a fresh session.` on a second line. The cause goes
to stderr. Nothing is retried; the process keeps running and the next message calls the engine
again. The one exception is the C6 error answer of `set_route`, which `/model <alias>` shows as
the reference shows a failed switch.

### 6. Threads

`tg::Link` queues turns and commands for its `engine` thread (ADR-0032 section 2); the loop
thread never waits for a C6 call. Attachment downloads and speech to text stay inside
`Ingress::accept` on the loop thread, and this meets the rule that blocking I/O must not hold
accepting or polling:

- `getUpdates` runs on the poller thread and the engine calls on the `engine` thread; neither
  waits for the loop thread beyond the one batch the poller hands over, and Telegram keeps the
  updates queued meanwhile, so nothing is lost.
- The reference handles one update at a time: its application is built without concurrent
  updates, and `_handle_media_message` awaits the download before the next update is processed.
  The text of a turn depends on that order (coalescing, media groups, forward labels above
  transcripts), so a download or a transcript belongs before the next update of the chat.
- Each download is bounded by the media read timeout and the media cap (ADR-0017); the STT
  client is bounded by its own timeout when it plugs in as the `Transcriber`.

### 7. Dependency

| Crate | Features | Why |
|---|---|---|
| `rustix` (workspace) | `std`, `net` | The socket created with its send timeout before `connect` (`net::socket_with`, `net::sockopt::set_socket_timeout`, `net::connect`), which `UnixStream::connect` cannot do, and the `SO_PEERCRED` uid of the engine and of a socketpair peer (`net::sockopt::socket_peercred`); as ADR-0008 section 4 admits it for `crates/runtime-adapters`, already in the lock. |

### 8. Differences from the reference

| Reference (module) | Here | Why |
|---|---|---|
| `/new` rotates the session at once (`_handle_reset_command`, `reset_session`) | The new session starts with the chat's next accepted turn; a `/model` before that turn is stored with the old session, and a restart of `ulei-tg` in between forgets the `/new` | C6 0.4.0 starts a session only through `submit_turn` `new` (ADR-0028 section 1). |
| `/new` clears the conversation's queued follow-ups (`_clear_conversation_scope`) | `interrupt` drops only the queue head; the other queued turns run in the old session first | C6 0.4.0 has no method that clears a queue. |
| The `/new` banner names the model and its context (`_reset_notice_session_info`) | Header only | C6 `status` tells neither whether its route is stored or the default nor the context window. |
| `/status` shows the session id, title, creation time, context use and billed tokens (`_handle_status_command`) | The session key and the `SessionStatus` fields; the other lines are left out | C6 0.4.0 `SessionStatus` carries none of them. |
| `/stop` answers that the agent had not started yet (`_handle_stop_command`, pending sentinel) | Never | C6 `interrupt` does not tell a pending turn apart. |
| `/model <alias>` checks the model and shows its context (`_handle_model_command`) | The engine stores any valid alias; no context line | ADR-0028 section 5: qb refuses an unknown alias at the next turn. |
| A prompt over the queue cap is dropped with a log warning (`_queue_or_replace_pending_event`) | The owner gets the failure notice | ADR-0028 section 8: the caller learns that the prompt will not run, and the caller here is the owner. |
| The failure answer adds a hint by the provider's HTTP status (`_handle_message_with_agent`) | No hint | The engine reaches models through qb (port map R1); the channel sees only the C6 failure. |

## Consequences

- The `turn_events` reader takes the `TurnId` of `submit_turn` to deliver a turn's texts and
  drives the typing indicator around it.
- A photo-only turn has an empty text, which `submit_turn` refuses: C6 0.4.0 carries no
  attachments.
