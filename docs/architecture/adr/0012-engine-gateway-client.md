# ADR-0012: The qb gateway client of `ulei-engine` and its dependencies

Status: accepted
Date: 2026-11-17
Affects: crates/engine, dep:serde_json, dep:rustix

## Context

ADR-0009 gives the engine exactly one model provider, the qb gateway, and ADR-0011 drafts its
contract C4G. The engine needs a synchronous client of that wire, and its tests need a scripted
fake gateway in place of any real one. ADR-0010 lets each dependency of the engine in only with
its own record.

## Decision

### 1. Client

`ulei_engine::gateway::Client` speaks C4G `x-transport`: HTTP/1.1 over a `SOCK_STREAM` Unix
socket, one request per connection, `Connection: close`, `Host: localhost`.

- `routes()` sends `GET /v1/model/routes` and returns `RoutesAnswer`.
- `turn(&TurnRequest)` sends `POST /v1/model/turn` with a `Content-Length` JSON body and
  returns a `Turn`, an iterator of `StreamEvent`s read as their lines arrive. A `200` answer
  must be `Transfer-Encoding: chunked`; NDJSON lines are reassembled across any chunk and read
  boundaries. The iterator ends after the terminal `stop` or `error` event, or after its first
  error.
- `Turn::cancel_handle()` gives a `Send + Clone` handle. `cancel()` shuts the turn's socket
  down locally, so the next item is `Cancelled` even while the gateway keeps the stream open,
  then posts `POST /v1/model/cancel` with the turn's `request_id` and returns `CancelAnswer`.
- Limits: head 8 KiB, `Content-Length` body 512 KiB, one event line 4 MiB, chunk-size line
  64 bytes.
- Timeouts: the connect timeout bounds the connect, which on a Unix socket waits only while the
  listen backlog is full (a non-blocking connect retried on `EAGAIN`); the idle timeout bounds
  every single read and write, so a gateway that stops sending ends the turn within it.

Errors, one typed variant per class, never a panic and never a retry:

| Condition | Error |
|---|---|
| Connect, write or read fails | `Io` |
| Connect timeout or idle timeout passes | `Timeout` |
| Non-200 answer with a C4G `Error` body | `Rejected` with the status and the `Error` |
| Status line or headers malformed, head over its limit, body framing wrong or not valid for its definition, bad chunk size, chunk data without CRLF, event line over its limit, not JSON, not a `StreamEvent`, or a payload other than the one of its `type` | `Malformed` with its kind |
| The stream ends without a terminal event | `Cut` |
| The turn was cancelled through its handle | `Cancelled` |

### 2. Fake gateway

`crates/engine/tests/fake_gateway/` is a test module that any test crate of the engine
includes with `mod fake_gateway;`. It listens on a Unix socket in a fresh 0700 temp directory,
answers each turn from a script (events, a pre-stream rejection, events then a silent open
stream, raw chunks, raw bytes), serves routes and cancel, and records every request byte for
byte. It is not part of the library, so no product code path can reach it.

### 3. Dependencies

Both are workspace dependencies already admitted by ADR-0007 and used by
`crates/runtime-adapters` (ADR-0008), with the same features; no crate is new to the workspace.

| Crate | Features | Why |
|---|---|---|
| `serde_json` | `std`, `float_roundtrip` | Request bodies from the generated C4G types; answer bodies and event lines parsed into them. |
| `rustix` | `std`, `net` | A non-blocking Unix `connect` for the connect timeout; std connects only blocking. |

Kept on std: Unix sockets, socket timeouts, HTTP/1.1 and chunked framing, threads in tests.

### 4. Measured size

`x86_64-unknown-linux-musl`, release profile of ADR-0001, rustc 1.100.0, measured as in
ADR-0010 section 3:

| Build | Static size |
|---|---|
| `ulei-engine` with the client in its library (static-pie, stripped) | 381,624 B |

The binary does not call the client yet, so the linker drops it; the first caller is `run`.

## Consequences

- The tool loop, the session journal and `run` reach models only through this client, and
  their tests drive the fake gateway.
- The framing repeats the C6 client of `crates/runtime-adapters` (ADR-0008 section 5); module
  crates share no code but `ulei-contracts`.
