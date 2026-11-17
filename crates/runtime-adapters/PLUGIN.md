# C6 runtime adapter: plugin specification

What a runtime-side C6 adapter must implement so that `ulei_runtime_adapters::client` can
call `inject` and `is_idle`. The adapter is a plugin inside a foreign agent runtime and may be
written in any language, Python included. The wire is C6 0.2.0
(`contracts/c6-runtime-adapter.schema.json`) under ADR-0008; this document follows both and
changes with them.

The key words MUST, MUST NOT, SHOULD and MAY are used as in RFC 2119. "The client" is
`ulei_runtime_adapters::client::Client`; "the core" is the ulei process that owns the inbox
lease and calls the client.

## 1. Roles

- The adapter is the server: it listens on a Unix socket. The core is the client: it connects,
  sends one request and reads one answer.
- The adapter serves the owner's session of one runtime instance. One adapter per runtime
  instance, one socket per adapter; the core holds one client per socket.
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

- `<method>` is `inject` or `is_idle`. The body is the method's input definition: `InjectRequest`
  for `inject`, `Empty` (`{}`) for `is_idle`.
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
- Status `200` carries the method's output definition: `InjectResponse` or `IdleStatus`. Every
  other status carries C6 `Error` (section 4).
- The head is at most 8,192 bytes and the body at most 524,288 bytes. The adapter MUST write
  exactly `Content-Length` body bytes and then close the connection; the client treats any byte
  after the body as a malformed answer.
- One request per connection: the adapter MUST NOT wait for a second request.

### 3.3 Python stdlib

An adapter built on `http.server.BaseHTTPRequestHandler` over a
`socketserver.UnixStreamServer` meets the framing only with these settings:

- Set `protocol_version = "HTTP/1.1"` on the handler class. The default is `"HTTP/1.0"`, and
  the client refuses an `HTTP/1.0` status line as malformed.
- Send `Connection: close` and set `self.close_connection = True` on every answer.
- Override `send_error`: the stdlib version writes an HTML body, which the client refuses. The
  stdlib also calls it for its own parse failures, so the override covers those.
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
| `not_found` | 404 | A method other than `POST`, or a path other than `/v1/<name>` of a C6 method the adapter serves (`inject`, `is_idle`) |
| `too_large` | 413 | Head over 8,192 bytes or body over 524,288 bytes |
| `internal` | 500 | The runtime or the adapter failed |

- `message` is a short diagnostic for logs. It MUST NOT echo the payload or the lease.
- A busy runtime is not an error: `inject` answers `200` with `accepted: false` (section 5.2).
  There is no `busy` code.
- A non-200 answer to `inject` MUST mean that no turn was started for that request.

Validity of the input definitions, from the C6 schema:

- `InjectRequest`: exactly the four fields `event_id`, `kind`, `payload`, `lease`, all strings;
  `event_id` matches `^[a-zA-Z0-9:_-]{1,96}$`; `payload` is not empty. The adapter MUST NOT
  restrict `kind` or `lease` further.
- `Empty`: an object with no fields.

## 5. Methods

### 5.1 `is_idle`

Input `Empty`, output `IdleStatus` `{"idle": <bool>, "since": <number>}`.

- `idle` is true when no turn is running in the owner's session and no owner input is pending
  (received by the runtime and not yet taken into a turn). Otherwise it is false.
- `since` is the Unix time in seconds, at least 0, from which the session has been idle: the
  end of the last turn, or the adapter's start if no turn has run. The adapter SHOULD send it
  when `idle` is true and SHOULD omit it when `idle` is false. The client refuses a negative
  `since`.
- The answer is a hint. The core MAY call `inject` right after an idle answer, and the state may
  have changed in between; `inject` decides again (section 5.2).
- `is_idle` MUST NOT change the session.

### 5.2 `inject`

Input `InjectRequest`, output `InjectResponse`
`{"accepted": <bool>, "session_id": <string>, "retry_after_s": <integer>}`.

- `event_id`, `kind` and `payload` are the C1 event's `id`, `kind` and `payload`. `lease` is
  the core's inbox lease of the claim.
- The adapter MUST accept only when the chat is free in the sense of `is_idle`, and MUST decide
  and start the turn atomically: two concurrent `inject` calls MUST NOT both start a turn.
- Accepted: the adapter starts a turn in the owner's session that receives the event and
  answers `{"accepted": true, "session_id": <id>}`. `session_id` MUST be present; it names the
  session that received the event, and the core records it with C1 `start`.
- Busy: the adapter starts nothing and answers
  `{"accepted": false, "retry_after_s": <seconds>}`, where `retry_after_s` is an integer of at
  least 0, the adapter's estimate of when the chat will be free. `session_id` is absent.
- The adapter MUST answer as soon as it has decided. It MUST NOT hold the connection while the
  turn runs.
- `payload` is untrusted data. The adapter MUST put it into the turn as data marked as an inbox
  event (with its `kind` and `event_id`), never as instructions, a system prompt or a tool
  call, and MUST NOT grant it any authority. It SHOULD NOT log the payload.
- `lease` is data only. The adapter MUST NOT call the inbox with it, SHOULD NOT pass it into
  the turn and SHOULD NOT log it.
- The adapter MUST NOT ack, retry or otherwise settle the inbox event; that stays with the core.
- Idempotency on `event_id`: the adapter records the `session_id` of every accepted `event_id`.
  A later `inject` of a recorded `event_id` MUST NOT start a second turn; it answers
  `{"accepted": true, "session_id": <recorded id>}`, whatever its `lease`. The record MUST last
  at least as long as the adapter process and SHOULD survive an adapter restart. C6 0.2.0
  therefore has no redelivery of an accepted event as a new turn: an event acked `deferred`
  that comes back with the same `event_id` gets its recorded session and no turn.

### 5.3 Timeouts

- The client applies one timeout per call, from `connect` to the last response byte. Its value
  is the core's configuration for that adapter socket.
- The adapter's whole answer MUST arrive within that timeout. Both methods answer from the
  adapter's own state and the decision to start a turn; neither waits for a turn, for the
  model or for the network.
- An answer that arrives after the client timed out is lost: the core sees `Timeout` and does
  not learn a `session_id`. The idempotency of `inject` makes the core's next attempt safe. The
  adapter MUST tolerate a peer that has closed before the answer is written.

## 6. Core-side flow

The core implements this flow; the adapter needs it only to understand what its answers cause.
The inbox calls are C1 operations (`contracts/c1-event.schema.json`, ADR-0007 section 6).

1. `claim`: the core leases the oldest claimable event and receives its row with a fresh
   `lease`.
2. `is_idle`: if the answer is `idle: false`, the core calls `retry` and the event becomes
   claimable again after the delay.
3. `inject` with the event's `id`, `kind`, `payload` and the claim's `lease`.
4. `accepted: true`: the core calls `start` with the returned `session_id`; the event is now
   `processing`.
5. While the turn runs, the core calls `renew` before the lease expires. It learns that the
   turn has ended from `is_idle`: idle with `since` not earlier than the accept.
6. The core calls `ack` with an `AckOutcome`: `success` or `silent` settle the event as `done`,
   `uncertain` settles it as `uncertain`, `deferred` makes it claimable again after
   3,600 s. C6 0.2.0 carries no result of the turn; the core acks `uncertain` when it cannot
   tell how the turn ended. If the core stops renewing, the inbox's recovery marks the
   processing event `uncertain` (`expired-processing`) once its lease expires.
7. `accepted: false`: the core calls `retry` with `retry_after_s` as the delay.

The lease never leaves the core's control: the adapter receives it as data and never uses it.

When a call fails before `start`, the event is still `leased`, and the core calls `retry`. The
client error class (ADR-0008 section 3) tells the core what the adapter may have done:

| Client error | What reached the adapter | Core action |
|---|---|---|
| `PeerUid` | Nothing; the adapter runs as another uid | `retry`; report a deployment fault |
| `RequestTooLarge` | Nothing; a valid C1 event cannot produce a body over 512 KiB | `retry`; report a defect |
| `Io` | Nothing if `connect` failed; otherwise unknown | `retry`; a repeated `inject` is idempotent |
| `Timeout` | Unknown; a turn may have started | `retry`; a repeated `inject` is idempotent |
| `Malformed`, `Invalid` | Unknown; the adapter broke the wire | `retry`; report an adapter defect |
| `Remote` `internal` | Refused; no turn started | `retry` |
| `Remote` `bad_request`, `forbidden`, `not_found`, `too_large` | Refused; no turn started | `retry`; report an adapter or deployment fault |

The client never retries a call itself. A failed `is_idle` during step 5 does not end the turn;
the core keeps renewing while it can and acks `uncertain` when it loses track.

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
| `valid/Empty.none.json` | `is_idle` |

Requests the adapter MUST refuse with `400 bad_request`:

| Fixture | Method | Why |
|---|---|---|
| `invalid/InjectRequest.missing_lease.json` | `inject` | `lease` is required |
| `invalid/Empty.argument.json` | `is_idle` | `Empty` has no fields |

Answers the adapter produces; each has the shape of one of these:

| Fixture | Answer |
|---|---|
| `valid/InjectResponse.accepted.json` | `inject` accepted, with `session_id` |
| `valid/InjectResponse.busy.json` | `inject` busy, with `retry_after_s` |
| `valid/IdleStatus.idle.json` | `is_idle` idle, with `since` |
| `valid/IdleStatus.busy.json` | `is_idle` not idle |
| `valid/Error.not_found.json` | `404` body |
| `valid/ErrorCode.too_large.json` | a valid `code` value |

Answers the adapter MUST NOT produce; the client refuses each:

| Fixture | Why |
|---|---|
| `invalid/InjectResponse.extra_field.json` | unknown field |
| `invalid/InjectResponse.missing_accepted.json` | `accepted` is required |
| `invalid/InjectResponse.negative_retry.json` | `retry_after_s` below 0 |
| `invalid/IdleStatus.idle_string.json` | `idle` is a boolean |
| `invalid/IdleStatus.negative_since.json` | `since` below 0 |
| `invalid/Error.missing_code.json` | `code` is required |
| `invalid/Error.unknown_code.json` | `busy` is not an `ErrorCode` |
| `invalid/ErrorCode.unknown.json` | `busy` is not an `ErrorCode` |

`McpServer.*` fixtures belong to `mcp_register`, which this document does not cover.

Behaviour to test beyond the fixtures:

- an `HTTP/1.1` status line and the headers of section 3.2 on every answer, errors included;
- `403 forbidden` for a peer of another uid, before the request is read;
- `404 not_found` for `GET /v1/is_idle` and for `POST /v1/unknown`;
- `413 too_large` for a `Content-Length` over 524,288;
- a second `inject` of an accepted `event_id` answers the same `session_id` and starts no turn;
- `inject` answers `accepted: false` with `retry_after_s` while a turn runs.

## 8. First adapter

The first adapter will be a Hermes plugin, built in the runtime's own repository, not in this
crate.
