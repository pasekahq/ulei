# ADR-0092: Debug trace of the engine's turns

Status: accepted
Date: 2026-11-26
Affects: contract:C9, crates/engine

## Context

The journal (C8, ADR-0014) keeps what the model is sent again on later turns: the system
prompt, the messages and per step the request id, the route and the usage. It deliberately
leaves out what belongs to one turn only: the turn context appended to the sent copy of the
input (C6 `context`, ADR-0031; the garden's pointers, ADR-0077; the `turn-reminder` text,
ADR-0022) and the owner's images (ADR-0085). It keeps no tool definitions and no time but the
end of each turn. The qb usage ledger has each model call's duration and tokens, but no text.

Debugging the owner's assistant (a context that grows past what it should, a slow or repeated
tool, a garden that misses its 150 ms deadline, a turn context that says the wrong thing) needs
exactly the parts no store keeps. The owner asked for them on 2026-11-26, for a developer
console outside this repository that joins them with the journal and the ledger.

The trace is not agent behaviour: the model never sees it and no request changes by a byte.
Like the night gardener (ADR-0078), it is an owner-designed addition, not a port.

## Decision

1. C9 0.12.0 (additive): `engine.trace_file`, a path; `ULEI_ENGINE_TRACE_FILE` wins over it.
   Unset, the default, nothing is written.
2. `crates/engine/src/trace.rs` appends one JSON object per line to that file, created with mode
   0600. The appends of one process are serialized (the C6 server runs turns on threads). Once
   the file reaches 64 MiB it is renamed to `<file>.1`, replacing an older one, before the next
   line. A failed write is one stderr line per process and never fails the turn.
3. Every line has `v` (the format version, 1), `at` (Unix seconds with milliseconds), `kind`,
   `session` (the journal's session id) and `turn` (its turn number). The kinds:

   | `kind` | When | Fields |
   |---|---|---|
   | `turn` | after the garden's `pointers` call, before the loop | `key`, `route`, `sender`, `channel`, `caller_context_chars`, `input_chars`, `images`, `garden`: `{ms, pointers, error}`, or `null` when the turn made no call |
   | `context` | at the start of the loop | `origin`, `text`: exactly what the sent copy of the input carries after the stored input (the whole turn context), `chars`, `images` |
   | `tools` | the first request of a tool set in this process | `hash` (16 hex digits of the SHA-256 of the set's JSON), `count`, `bytes`, `tools`: the C4G definitions as sent |
   | `request` | each model request, the summary request included | `request_id`, `start`, `end`, `ms` (the overloaded retries and the resend without thinking blocks inside), `messages`, `tools_hash`, `usage`, `route`, `stop` or `error` (debug text, at most 300 characters) |
   | `tool` | each tool call, after its batch | `request_id` of its step, `id`, `name`, `start`, `end`, `ms`, `result_chars`, `is_error` |
   | `compaction` | each compaction | `trigger` (`threshold` or `context_too_long`), `start`, `end`, `ms`, `messages_before`, `messages_after`, `ok` |
   | `turn_end` | after the loop, before the journal append | `steps`, `ended` (debug text, at most 300 characters) |

4. No message, tool argument, tool result or system prompt text: the journal has them, and a
   reader joins the trace with it by `session`, `turn` and `request_id`.
5. Tool calls are timed whether or not the key is set (two clock reads per call in
   `agent/dispatch.rs`); only the writing depends on it.

## Consequences

- The file holds the turn contexts, that is the owner's garden pointers and channel context. It
  belongs next to the journal in the engine's state directory, the one its unit may write, and
  takes the journal's care.
- Size: a few hundred bytes per request and per tool call, plus each tool set once per process
  start (tens of KiB). With a hundred owner turns a day that is a few MiB a week; the rotation
  bounds the trace at 128 MiB.
- No budget changes: no new dependency (`serde_json` and `sha2` are already the engine's).
  Unset, a turn costs two clock reads per tool call more.
- The line format is a debug format, not a contract: a reader checks `v` and skips the lines of
  another version. A change of a field bumps `v`.
