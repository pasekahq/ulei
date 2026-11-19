# C6 runtime adapter: plugin specification

What a runtime-side C6 adapter must implement so that the core can deliver inbox events into
a foreign agent runtime and follow their turns. The adapter is a plugin inside that runtime and
may be written in any language, Python included. The wire is C6 0.3.1
(`contracts/c6-runtime-adapter.schema.json`) under ADR-0008, with the session and turn
semantics of ADR-0028; this document follows them and changes with them.

The key words MUST, MUST NOT, SHOULD and MAY are used as in RFC 2119. "The client" is the C6
client with the framing, checks and error classes of `ulei_runtime_adapters::client::Client`
(ADR-0008); "the core" is the ulei process that owns the inbox lease and calls the client: the
delivery loop of `ulei-inbox serve` (ADR-0036).

## 1. Roles

- The adapter is the server: it listens on a Unix socket. The core is the client: it connects,
  sends one request and reads one answer.
- The adapter serves the owner's session of one runtime instance. One adapter per runtime
  instance, one socket per adapter; the core holds one client per socket.
- Methods: an adapter MUST serve `inject`, `is_idle` and `status`. `submit_turn`,
  `turn_events`, `interrupt` and `set_route` are served by the engine `ulei-engine` only; an
  adapter MUST answer them `404 not_found`, as any other method it does not serve. `skills_dir`, `mcp_register`,
  `raw_sessions` and `owner_channel` are outside this document.
- The adapter never talks to the inbox (C1). It does not claim, start, renew, retry or ack
  events; the core does all of that (section 6).

## 2. Socket

- The socket path comes from deployment configuration and is the same value the core's client
  is built with. It is at most 107 bytes (the `sun_path` limit).
- The adapter creates the socket: `SOCK_STREAM`, `AF_UNIX`, bound at that path.
- The socket's directory MUST be owned by the adapter's uid and MUST NOT be accessible to other
  users (mode 0700 or stricter). The socket MUST have mode 0600.
- The adapter MUST run as the same uid as the core. The client reads the adapter's uid with
  `SO_PEERCRED` after `connect` and refuses any other uid before it sends a byte.

## 3. Framing

### 3.1 Request

Every call is one HTTP/1.1 request on its own connection:

```text
POST /v1/<method> HTTP/1.1\r\n
Host: localhost\r\n
Content-Type: application/json\r\n
Content-Length: <n>\r\n
Connection: close\r\n
\r\n
<n bytes of JSON>
```

- `<method>` is `inject`, `is_idle` or `status`. The body is the method's input definition:
  `InjectRequest` for `inject`, `Empty` (`{}`) for `is_idle`, `StatusRequest` for `status`.
- The client sends exactly these headers and the body as canonical JSON (ADR-0007 section 5,
  rule 1: UTF-8, keys sorted, no spaces, non-ASCII raw). The adapter MUST parse any valid JSON
  and MUST NOT depend on key order or whitespace.
- The adapter MUST read exactly `Content-Length` body bytes. It MUST refuse a request without
  `Content-Length` or with `Transfer-Encoding` (`400 bad_request`).
- Limits: the request line, header lines and the empty line together at most 8,192 bytes; the
  body at most 524,288 bytes. Over either limit the adapter MUST answer `413 too_large`; it MAY
  answer from `Content-Length` before reading the body.
- The adapter MUST close a connection that has not sent its whole request within 5 s of
  `accept`.

### 3.2 Response

```text
HTTP/1.1 <status> <reason>\r\n
Content-Type: application/json\r\n
Content-Length: <n>\r\n
Connection: close\r\n
\r\n
<n bytes of JSON>
```

- The status line MUST be `HTTP/1.1`, a three-digit status and a reason phrase. The client
  refuses any other protocol version, `HTTP/1.0` included.
- The answer MUST carry `Content-Type: application/json`, exactly one `Content-Length` and
  `Connection: close`, and MUST NOT carry `Transfer-Encoding`. Other headers (`Date`, `Server`)
  are allowed; every header line is `<name>: <value>` with a non-empty name of visible ASCII.
- Status `200` carries the method's output definition: `InjectResponse`, `IdleStatus` or
  `SessionStatus`. Every other status carries C6 `Error` (section 4).
- The head is at most 8,192 bytes and the body at most 524,288 bytes. The adapter MUST write
  exactly `Content-Length` body bytes and then close the connection. The client reads until the
  end of the stream: any byte after the body is a malformed answer, and an adapter that keeps
  the connection open runs the call into its timeout.
- One request per connection: the adapter MUST NOT wait for a second request.

### 3.3 Python stdlib

An adapter built on `http.server.BaseHTTPRequestHandler` over a
`socketserver.UnixStreamServer` meets the framing only with these settings:

- Set `protocol_version = "HTTP/1.1"` on the handler class. The default is `"HTTP/1.0"`, and
  the client refuses an `HTTP/1.0` status line as malformed.
- Send `Connection: close` and set `self.close_connection = True` on every answer.
- Override `send_error`: the stdlib version writes an HTML body, which the client refuses. The
  stdlib also calls it for its own parse failures, so the override covers those and answers
  each with the C6 status and code of section 4: 400 and 505 (a request line, header or
  version it cannot parse) as `400 bad_request`; 501 (a method without a `do_<METHOD>`
  handler, such as `GET`) as `404 not_found`; 414 (request line over 65,536 bytes) and 431
  (a header line too long or over 100 header lines) as `413 too_large`.
- Enforce the 8,192-byte head limit yourself; the stdlib allows a 65,536-byte request line and
  100 header lines.
- On a Unix socket `client_address` is not a `(host, port)` pair; override `address_string`
  (or `log_message`), which the default request logging calls.
- Bound the whole request by a 5 s deadline; the handler's `timeout` attribute bounds each
  read, not the request.
- Read the peer uid with
  `struct.unpack("3i", sock.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i")))[1]`
  and compare it with `os.geteuid()`.

## 4. Peer check and errors

After `accept`, before it reads the request, the adapter MUST read the peer's `SO_PEERCRED` uid
and compare it with its own effective uid. If they differ it MUST answer `403 forbidden` and
close without reading the request.

Every non-200 answer has a C6 `Error` body, `{"code": <ErrorCode>, "message": <string>}`, and
the status fixed by its code. The client refuses an `Error` whose status is not the status of
its code.

| `code` | Status | When |
|---|---|---|
| `bad_request` | 400 | Malformed request line or headers, no `Content-Length`, `Transfer-Encoding`, body not JSON, body not valid for the input definition |
| `forbidden` | 403 | The peer's uid differs from the adapter's effective uid |
| `not_found` | 404 | A method other than `POST`, or a path other than `/v1/<name>` of a C6 method the adapter serves (`inject`, `is_idle`, `status`); `submit_turn`, `turn_events`, `interrupt` and `set_route` included |
| `too_large` | 413 | Head over 8,192 bytes or body over 524,288 bytes |
| `internal` | 500 | The runtime or the adapter failed |

- `message` is a short diagnostic for logs. It MUST NOT echo the payload or the lease.
- A busy runtime is not an error: `inject` answers `200` with `accepted: false` (section 5.2).
  There is no `busy` code.
- A non-200 answer to `inject` MUST mean that no turn was started for that request.

Validity of the input definitions, from the C6 schema:

- `InjectRequest`: exactly the five fields `event_id`, `kind`, `payload`, `lease`, all
  strings, and `attempt`, an integer of at least 1; `event_id` matches
  `^[a-zA-Z0-9:_-]{1,96}$`; `payload` is not empty. The adapter MUST NOT restrict `kind` or
  `lease` further.
- `Empty`: an object with no fields.
- `StatusRequest`: `session`, a string of 1 to 128 characters, and optionally `turn`, a string
  matching `^[a-zA-Z0-9:._-]{1,128}$`; no other field.

## 5. Methods

### 5.1 `is_idle`

Input `Empty`, output `IdleStatus` `{"idle": <bool>, "since": <number>}`.

- `idle` is true when no turn is running in the owner's session, none is queued and no owner
  input is pending (received by the runtime and not yet taken into a turn). Otherwise it is
  false.
- `since` is the Unix time in seconds, at least 0, from which the session has been idle: the
  end of the last turn, or the adapter's start if no turn has run. The adapter SHOULD send it
  when `idle` is true and SHOULD omit it when `idle` is false. The client refuses a negative
  `since`.
- The answer is a hint. The core MAY call `inject` right after an idle answer, and the state may
  have changed in between; `inject` decides again (section 5.2).
- `is_idle` MUST NOT change the session.

### 5.2 `inject`

Input `InjectRequest`, output `InjectResponse`
`{"accepted": <bool>, "session_id": <string>, "turn": <string>, "retry_after_s": <integer>}`.

- `event_id`, `kind` and `payload` are the C1 event's `id`, `kind` and `payload`. `lease` is
  the core's inbox lease of the claim and `attempt` the claim's C1 `Row.attempts`.
- The adapter MUST accept only when the chat is free in the sense of `is_idle`, and MUST decide
  and start the turn atomically: two concurrent `inject` calls MUST NOT both start a turn.
- Accepted: the adapter starts a turn in the owner's session that receives the event and
  answers `{"accepted": true, "session_id": <id>, "turn": <turn id>}`. `session_id` MUST be
  present; it names the session that received the event, the core records it with C1 `start`
  and passes it to `status`. `turn` MUST be present; it names the turn for `status`
  (section 5.3) and is never given to another turn of the runtime, across restarts.
- Busy: the adapter starts nothing and answers
  `{"accepted": false, "retry_after_s": <seconds>}`, where `retry_after_s` is an integer of at
  least 0, the adapter's estimate of when the chat will be free. `session_id` and `turn` are
  absent.
- The adapter MUST answer as soon as it has decided. It MUST NOT hold the connection while the
  turn runs.
- `payload` is untrusted data. The adapter MUST start the turn with the payload as its input,
  as the engine passes it verbatim as the prompt of an event turn (ADR-0028 section 6): the
  turn's sender is an inbox event, not the owner, and the payload MUST NOT be granted the
  owner's authority or be put into the system prompt or a tool call. It SHOULD NOT log the
  payload.
- `lease` is data only. The adapter MUST NOT call the inbox with it, SHOULD NOT pass it into
  the turn and SHOULD NOT log it.
- The adapter MUST NOT ack, retry or otherwise settle the inbox event; that stays with the core.
- Idempotency on `event_id` and `attempt` together (ADR-0028 section 7, gap 2): the adapter
  records the highest accepted `attempt` of an `event_id` with its `session_id` and `turn`. A
  later `inject` of a recorded `event_id` with the same or a lower `attempt` MUST NOT start a
  turn; it answers `{"accepted": true, "session_id": <recorded id>, "turn": <recorded turn>}`
  of the record, whatever its `lease`. A higher `attempt` is a new delivery: the adapter
  decides as for a new event and, when it accepts, starts a new turn and records it. An event
  acked `deferred` comes back on a later claim with a higher `attempt` and so becomes a new
  turn. The adapter MUST keep the records of at least the last 64 delivered events, in memory
  at least: a replay comes while the core holds the claim's lease, before it calls `start`.
  Records lost at an adapter restart let a replay start the turn again, which is the
  at-least-once delivery of C1.

### 5.3 `status`

Input `StatusRequest` `{"session": <session_id>, "turn": <turn id>}`, output `SessionStatus`.

- `session` is a `session_id` the adapter answered on `inject`. A session the adapter does not
  know reads idle, with `queue_depth` 0 and no turn reports; it is not an error.
- `running` and `since`: whether a turn of the session runs, and the Unix time since when it
  runs or is idle, as `is_idle` (section 5.1).
- `queue_depth`: turns or inputs the runtime holds for the session and has not started; 0 for
  a runtime without a queue.
- `route`: the route alias, or for a runtime without aliases its model name as a lowercase
  alias of at most 64 characters, that the session's next turn runs on.
- `last_turn`: the report of the turn that ended last; absent before the first.
- `turn`: when the request names a turn, its report. `state` is `running` until the turn ends,
  then `ended` with `outcome` and `ended`; a turn the adapter no longer holds reads `unknown`.
  The adapter MUST hold the report of an accepted turn until at least 64 later turns of the
  session have ended, in memory at least.
- `outcome` is one of `completed` (the model answered), `max_steps`, `halted`, `interrupted`,
  `context_exhausted`, `quota_exhausted` and `failed` (ADR-0028 section 1). `line` carries one
  line for `quota_exhausted`, `context_exhausted` and `failed`; it MUST NOT echo the payload or
  the lease.
- `status` MUST NOT change the session.

### 5.4 Timeouts

- The client applies one timeout per call, from `connect` to the end of the stream; a
  `connect` that waits on a full listen backlog counts against it. Its value is the core's
  configuration for that adapter socket.
- The adapter's whole answer MUST arrive within that timeout. Every method answers from the
  adapter's own state and the decision to start a turn; none waits for a turn, for the model
  or for the network.
- An answer that arrives after the client timed out is lost: the core sees `Timeout` and does
  not learn a `session_id` or `turn`. The idempotency of `inject` makes the core's replay with
  the same `attempt` safe and gives it the recorded turn. The adapter MUST tolerate a peer that
  has closed before the answer is written.

## 6. Core-side flow

The core implements this flow (ADR-0036 sections 2 and 3); the adapter needs it only to
understand what its answers cause. The inbox calls are C1 operations
(`contracts/c1-event.schema.json`, ADR-0007 section 6).

1. The core acts only while an event is claimable and the inbox root holds `ENABLED`; it holds
   the root's `consumer.lock`, so it is the root's one consumer.
2. `is_idle`: on `idle: false` or a failed call the core claims nothing and asks again 2 s
   later. An idle check spends no offer of the event.
3. `claim`: the core leases the oldest claimable event and receives its row with a fresh
   `lease`; the claim raises the row's `attempts`.
4. `inject` with the event's `id` and `kind`, the model text the core built from the event
   (ADR-0036 section 4), the claim's `lease` and the row's `attempts` as `attempt`.
5. `accepted: true`: the core calls `start` with the returned `session_id`; the event is now
   `processing`. `accepted: false`: the core calls `retry` with the inbox's retry delay of
   15 s (`RETRY_DELAY`), whatever `retry_after_s` says.
6. Every 2 s while the event is `processing`, the core calls `status` with the `session_id` and
   `turn` of the accept, and calls `renew` before that until it has seen the turn `ended`.
7. The core calls `ack` once the turn reads `ended` with `TurnReport.delivery`, or reads
   `dropped` or `unknown`; the outcome follows ADR-0036 section 7. A turn that ended without a
   `delivery` is no longer renewed: if its lease runs out first, the inbox's recovery marks the
   event `uncertain` (`expired-processing`), as it does for a core that stopped renewing.

The lease never leaves the core's control: the adapter receives it as data and never uses it.

When an `inject` call fails before `start`, the event is still `leased`. The client error
class (ADR-0008 section 3) tells the core what the adapter may have done; every `retry` here has
the 15 s delay of step 5:

| Client error | What reached the adapter | Core action |
|---|---|---|
| `PeerUid` | Nothing; the adapter runs as another uid | `retry`; report a deployment fault |
| `RequestTooLarge` | Nothing; a valid C1 event cannot produce a body over 512 KiB | `retry`; report a defect |
| `Io` | Nothing if `connect` failed; otherwise unknown | `retry` when `connect` failed; otherwise `renew` and replay `inject` with the same `attempt` |
| `Timeout` | Unknown; a turn may have started | `renew` and replay `inject` with the same `attempt`; the replay answers the recorded turn |
| `Malformed`, `Invalid` | Unknown; the adapter broke the wire | `renew` and replay `inject` with the same `attempt`; report an adapter defect |
| `Remote` `internal` | Refused; no turn started | `retry` |
| `Remote` `bad_request`, `forbidden`, `not_found`, `too_large` | Refused; no turn started | `retry`; report an adapter or deployment fault |

The client never retries a call itself. The core replays while it holds the lease; when the
lease is lost before an answer, the next claim has a higher `attempt` and may start the turn a
second time, so delivery stays at least once, as C1 states. A failed `status` during step 6
does not end the turn: the core keeps renewing until it has seen the turn ended and asks again
on the next tick.

## 7. Conformance

The fixtures are in `contracts/fixtures/c6/`. An adapter can be tested against them without
the Rust code: send each request fixture to the adapter as the client does (the canonical JSON
of the fixture, `json.dumps(v, ensure_ascii=False, sort_keys=True, separators=(",", ":"))`
in Python, framed as in section 3.1) and check the answer.

Requests the adapter MUST accept with `200`:

| Fixture | Method |
|---|---|
| `valid/InjectRequest.basic.json` | `inject` |
| `valid/InjectRequest.escapes.json` | `inject` (non-ASCII, `\n`, `\t`, `"` and `/` in `payload`) |
| `valid/InjectRequest.redelivery.json` | `inject` (a later `attempt`) |
| `valid/Empty.none.json` | `is_idle` |
| `valid/StatusRequest.session.json` | `status` |
| `valid/StatusRequest.turn.json` | `status` with a turn |

Requests the adapter MUST refuse with `400 bad_request`:

| Fixture | Method | Why |
|---|---|---|
| `invalid/InjectRequest.missing_lease.json` | `inject` | `lease` is required |
| `invalid/InjectRequest.missing_attempt.json` | `inject` | `attempt` is required |
| `invalid/InjectRequest.attempt_zero.json` | `inject` | `attempt` is at least 1 |
| `invalid/Empty.argument.json` | `is_idle` | `Empty` has no fields |
| `invalid/StatusRequest.missing_session.json` | `status` | `session` is required |

Answers the adapter produces; each has the shape of one of these:

| Fixture | Answer |
|---|---|
| `valid/InjectResponse.accepted.json` | `inject` accepted, with `session_id` and `turn` |
| `valid/InjectResponse.busy.json` | `inject` busy, with `retry_after_s` |
| `valid/IdleStatus.idle.json` | `is_idle` idle, with `since` |
| `valid/IdleStatus.busy.json` | `is_idle` not idle |
| `valid/SessionStatus.fresh.json` | `status` of a session without turns |
| `valid/SessionStatus.quota_exhausted.json` | `status` whose last turn ended on quota, with its line |
| `valid/SessionStatus.running_with_turn.json` | `status` of a running session with a named turn |
| `valid/TurnReport.completed.json` | `turn` or `last_turn` of an ended turn |
| `valid/TurnReport.running.json` | `turn` of a running turn |
| `valid/TurnReport.unknown.json` | `turn` of a turn the adapter does not hold |
| `valid/Error.not_found.json` | `404` body |
| `valid/ErrorCode.too_large.json` | a valid `code` value |

Answers the adapter MUST NOT produce; the client refuses each:

| Fixture | Why |
|---|---|
| `invalid/InjectResponse.extra_field.json` | unknown field |
| `invalid/InjectResponse.missing_accepted.json` | `accepted` is required |
| `invalid/InjectResponse.negative_retry.json` | `retry_after_s` below 0 |
| `invalid/InjectResponse.turn_not_string.json` | `turn` is a string |
| `invalid/SessionStatus.missing_route.json` | `route` is required |
| `invalid/SessionStatus.queue_over_cap.json` | `queue_depth` over 32 |
| `invalid/SessionKey.empty.json` | `session` has at least 1 character |
| `invalid/RouteAlias.uppercase.json` | `route` matches `^[a-z][a-z0-9_-]{0,63}$` |
| `invalid/TurnReport.missing_state.json` | `state` of a `turn` or `last_turn` is required |
| `invalid/TurnId.space.json` | a turn id matches `^[a-zA-Z0-9:._-]{1,128}$` |
| `invalid/TurnState.finished.json` | `finished` is not a `TurnState` |
| `invalid/TurnOutcome.busy.json` | `busy` is not a `TurnOutcome` |
| `invalid/IdleStatus.idle_string.json` | `idle` is a boolean |
| `invalid/IdleStatus.negative_since.json` | `since` below 0 |
| `invalid/Error.missing_code.json` | `code` is required |
| `invalid/Error.unknown_code.json` | `busy` is not an `ErrorCode` |
| `invalid/ErrorCode.unknown.json` | `busy` is not an `ErrorCode` |

`McpServer.*` fixtures belong to `mcp_register`, which this document does not cover; the
`SubmitTurn*`, `Interrupt*`, `SetRouteRequest` and `RouteSet` fixtures belong to methods only
the engine serves.

Behaviour to test beyond the fixtures:

- an `HTTP/1.1` status line and the headers of section 3.2 on every answer, errors included;
- `403 forbidden` for a peer of another uid, before the request is read;
- `404 not_found` for `GET /v1/is_idle` and for `POST /v1/unknown`;
- `413 too_large` for a `Content-Length` over 524,288;
- a second `inject` of an accepted `event_id` and `attempt` answers the same `session_id` and
  `turn` and starts no turn;
- an `inject` of an accepted `event_id` with a higher `attempt` starts a new turn when the
  session is free;
- `status` with the accepted `turn` reads `running`, then `ended` with an `outcome`;
- `404 not_found` for `POST /v1/submit_turn`, `/v1/turn_events`, `/v1/interrupt` and
  `/v1/set_route`;
- `inject` answers `accepted: false` with `retry_after_s` while a turn runs.

## 8. First adapter

The first adapter will be a Hermes plugin, built in the runtime's own repository, not in this
crate.
