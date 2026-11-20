# ADR-0012: The qb gateway client of `ulei-engine` and its dependencies

Status: accepted
Date: 2026-11-17
Affects: crates/engine, dep:serde_json, dep:rustix

## Context

ADR-0009 gives the engine exactly one model provider, the qb gateway, whose contract C4G is
vendored from qb at the pin (ADR-0038). The engine needs a synchronous client of that wire, and its tests need a scripted
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
- qb may answer from the head alone (401, 411, 413, 429) and close before it reads the body.
  When the write of a request fails with a broken pipe or a reset, the client still reads the
  answer the gateway sent and takes it as after a full write: a rejection keeps its class, so a
  429 `overloaded` with `retry_after` reaches the overloaded retry of the loop. Only when no
  complete answer can be read is the result `Io`, with the failed write.
- `Turn::cancel_handle()` gives a `Send + Clone` handle. `stop()` shuts the turn's socket down
  locally, so the next item is `Cancelled` even while the gateway keeps the stream open, and qb
  cancels a turn whose stream closes. `cancel()` stops, then posts `POST /v1/model/cancel` with
  the turn's `request_id` and returns `CancelAnswer`; qb answers once the turn has ended or
  after its own 5 s wait, always `cancelled: true`, and 404 for a turn that is unknown or has
  ended. The post waits at
  most `CANCEL_TIMEOUT`, 2 s, for its connect and its answer (ADR-0028 section 3).
- Limits: head 8 KiB, `Content-Length` body 512 KiB, one event line 4 MiB, chunk-size line
  64 bytes.
- Timeouts: the connect timeout bounds the connect, which on a Unix socket waits only while the
  listen backlog is full (a non-blocking connect retried on `EAGAIN`); the idle timeout bounds
  every single read and write, so a gateway that stops sending ends the turn within it.

Errors, one typed variant per class, never a panic and never a retry:

| Condition | Error |
|---|---|
| Connect or read fails, or a write fails and no complete answer can be read | `Io` |
| Connect timeout or idle timeout passes | `Timeout` |
| Non-200 answer with a C4G `Error` body | `Rejected` with the status and the `Error` |
| Status line or headers malformed, head over its limit, body framing wrong or not valid for its definition, bad chunk size, chunk data without CRLF, event line over its limit, not JSON, not a `StreamEvent`, or a payload other than the one of its `type` | `Malformed` with its kind |
| The stream ends without a terminal event | `Cut` |
| The turn was cancelled through its handle | `Cancelled` |

### 2. Fake gateway

`crates/engine/tests/fake_gateway/` is a test module that any test crate of the engine
includes with `mod fake_gateway;`. It listens on a Unix socket in a fresh 0700 temp directory,
answers each turn from a script (events, a pre-stream rejection, events then a silent open
stream, raw chunks, raw bytes) or, after reading only its head, with given bytes before it
closes, serves routes and cancel, and records every request byte for
byte. It is not part of the library, so no product code path can reach it.

### 3. Dependencies

Both are workspace dependencies already admitted by ADR-0007 and used by
`crates/runtime-adapters` (ADR-0008); the engine adds the `process` and `fs` features of `rustix`. No
crate is new to the workspace.

| Crate | Features | Why |
|---|---|---|
| `serde_json` | `std`, `float_roundtrip` | Request bodies from the generated C4G types; answer bodies and event lines parsed into them. |
| `rustix` | `std`, `net`, `process`, `fs` | A non-blocking Unix `connect` for the connect timeout, std connects only blocking; killing and waiting on a tool's process group; opening a script the `terminal` guard reads with `O_NONBLOCK`, which std's `File::open` cannot pass, so a FIFO cannot block the guard. |

Kept on std: Unix sockets, socket timeouts, HTTP/1.1 and chunked framing, threads in tests.

### 4. Measured size

`x86_64-unknown-linux-musl`, release profile of ADR-0001, rustc 1.100.0, measured as in
ADR-0010 section 3:

| Build | Static size |
|---|---|
| `ulei-engine` with the client in its library (static-pie, stripped) | 381,624 B |

## Consequences

- The tool loop, the session journal and `run` reach models only through this client, and
  their tests drive the fake gateway.
- The framing repeats the C6 client of `crates/runtime-adapters` (ADR-0008 section 5); module
  crates share no code but `ulei-contracts`.
