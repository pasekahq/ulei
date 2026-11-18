# ADR-0008: C6 wire and the runtime adapters client

Status: accepted
Date: 2026-11-17
Affects: crates/runtime-adapters, dep:serde_json, dep:rustix, contract:C6

## Context

C6 names the operations between the core and an agent runtime adapter but not their transport.
The core needs `inject` and `is_idle` to deliver inbox events into the owner's session. The
adapter is a runtime plugin and may be Python. The C1 wire of ADR-0007 section 6 already
runs HTTP/1.1 JSON over a Unix socket in `ulei-core`; C6 uses the same shape so one framing
and one error discipline serve both.

## Decision

### 1. Transport

The runtime is the server and listens: a runtime adapter or the engine `ulei-engine`
(ADR-0028). The core and other local callers are clients and connect. C6 states the same in
`x-transport` and `x-error`.

- Transport: HTTP/1.1 with JSON bodies over a `SOCK_STREAM` Unix socket. The socket path
  comes from configuration (at most 107 bytes, the `sun_path` limit); the server creates it
  with mode 0600 in an owner-only directory.
- Operations: C6 `x-methods`; each is `POST /v1/<name>` with the input definition as the body
  and the output definition as the `200` body.
- Request: `POST /v1/<name> HTTP/1.1`, CRLF header lines, an empty line, then exactly
  `Content-Length` bytes of canonical JSON (ADR-0007 section 5, rule 1). The client sends
  `Host: localhost` (HTTP/1.1 requires one; on a Unix socket it carries no meaning),
  `Content-Type: application/json`, `Content-Length` and `Connection: close`.
  `Transfer-Encoding` is refused in both directions.
- Limits, in both directions: the status or request line, header lines and empty line at most
  8 KiB together; the body at most 512 KiB. The client refuses a request body over the limit
  before it connects.
- One request per connection: the answer carries `Connection: close` and the server closes.
  The client reads the status line, the headers and exactly `Content-Length` body bytes, then
  reads on until the end of the stream; a byte past the body, in the same read or a later one,
  is a malformed response. A server that keeps the connection open after its answer runs the
  call into its timeout.
- Response: `HTTP/1.1 <status> <reason>`, `Content-Type: application/json`, `Content-Length`,
  `Connection: close`, JSON body. `200` carries the output definition; every other status
  carries C6 `Error`.
- Timeouts: the server closes a connection that has not sent its whole request within 5 s.
  The client takes one timeout per call, from connect to the end of the stream, applied as
  socket read and write timeouts and, before `connect`, as the socket's send timeout
  (`SO_SNDTIMEO`). A Unix `connect` completes without the server's `accept` unless the listen
  backlog is full; then it waits at most the time left and fails with `EAGAIN`, which the
  client reports as `Timeout`.

### 2. Peer checks

- Server: after `accept`, the `SO_PEERCRED` uid of the peer must equal the server's
  effective uid, else `403 forbidden` before the request is read.
- Client: after `connect`, the `SO_PEERCRED` uid of the server must equal the uid the client
  was built for, by default its own effective uid; otherwise the call fails with a peer uid
  error before any byte is sent. An `inject` request carries owner data (the event payload and
  the inbox lease), so it may reach only a process of the owner's uid; the owner-only
  directory is the first gate and the uid check holds if its mode is ever widened.

### 3. Errors

Every non-200 body is C6 `Error`; its `code` fixes the status:

| Condition at the server | Status | `code` |
|---|---|---|
| Malformed request line or headers, body not JSON, body not valid for the input definition | 400 | `bad_request` |
| Peer uid differs | 403 | `forbidden` |
| Other path or method, or a C6 method the server does not serve | 404 | `not_found` |
| Headers or body over the limits | 413 | `too_large` |
| Runtime or server failure | 500 | `internal` |

A busy runtime is not an error: `inject` answers `200` with `accepted: false` and
`retry_after_s`, and a full turn queue answers `submit_turn` with `200` and `accepted: false`.

The client (`ulei_runtime_adapters::client`) returns one typed error per class and never
retries:

| Condition at the client | Error |
|---|---|
| Connect, write, read or peer credentials fail | `Io` |
| The call timeout passes | `Timeout` |
| Server uid differs | `PeerUid` |
| Request body over 512 KiB | `RequestTooLarge` |
| Status line or headers malformed, no `Content-Length`, `Transfer-Encoding`, head or body over the limits, end of stream before the whole response, bytes past the body, body not JSON | `Malformed` |
| Non-200 with a valid `Error` whose status is the status of its `code` | `Remote` with the code |
| `200` body not valid for the output definition, non-200 body not a valid `Error`, or a status other than that of its `code` | `Invalid` |

The client checks what the generated types do not encode, else `Invalid`: the `TurnId`,
`SessionKey` and `RouteAlias` patterns and lengths; `since` and `ended` at least 0;
`queue_depth` and `position` at most 32; `turn` and `position` of `SubmitTurnResponse` present
exactly when `accepted`; `turn` of `InterruptResponse` present exactly when `interrupted` and
`dropped` only then; `SessionStatus.turn` present exactly when the request names a turn, and
that turn; and the fields a `TurnReport` carries for its `state`: `outcome` and `ended` for
`ended` (both absent in a `last_turn` read back from the journal), `ended` alone for
`dropped`, neither for the other states, and `line` only with `quota_exhausted`,
`context_exhausted` or `failed`. `Client::call` sends any C6 method name and returns the
`200` body unchecked.

### 4. Dependencies of `crates/runtime-adapters`

Both are workspace dependencies already admitted by ADR-0007 section 2, with the same
features; no crate is new to the workspace.

| Crate | Features | Why |
|---|---|---|
| `serde_json` | `std`, `float_roundtrip` | Request bodies as canonical JSON, response bodies parsed into the generated C6 types. |
| `rustix` | `std`, `net` | `SO_PEERCRED` uid of the adapter (`net::sockopt::socket_peercred`) and of a socketpair peer for the own effective uid; the socket created and given its send timeout before `connect` (`net::socket_with`, `net::sockopt::set_socket_timeout`, `net::connect`), which `UnixStream::connect` cannot do; `UnixStream::peer_cred` is unstable and `getsockopt` or `setsockopt` through `libc` needs `unsafe`. |

Kept on std: the connected stream (`std::os::unix::net::UnixStream`), its read and write
timeouts, HTTP/1.1 framing.

### 5. The framing is a small copy

The client's HTTP/1.1 framing (header parsing, `Content-Length`, limits) and canonical JSON
repeat the server code of `crates/core`. Module crates may depend only on `ulei-contracts`
(ADR-0001, `cargo xtask arch`), and `ulei-contracts` holds generated types and no logic, so
there is no crate both may share. The copy is about a hundred lines; the wire tests of each
crate pin the same framing.

## Consequences

- The ulei side of C6 is `ulei_runtime_adapters::client`; a C6 server or runtime plugin
  follows sections 1 to 3. `crates/runtime-adapters/PLUGIN.md` specifies what such a plugin
  implements: `inject`, `is_idle` and `status`.
- A change of the C6 transport, limits or error table changes C6 and this record together.
