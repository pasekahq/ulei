# ADR-0014: C8 session journal in SQLite

Status: accepted
Date: 2026-11-17
Affects: contract:C8, dep:rusqlite

## Context

`ulei-engine` sessions must survive a restart. The loop builds every request only from its
template, the history, the appended messages, the tool definitions and `request_id(step)`
(`agent::Agent::run`), so a history read back from disk and request ids that do not depend on the
process give the next request the bytes an engine that never stopped would send. ADR-0010
lets each dependency into the engine only with its own record.

## Decision

### 1. Contract C8

`contracts/c8-session-journal.schema.json`, JSON Schema 2020-12, version 0.2.0, draft. It
defines the journal's records:

- `Session`: `id` (from 1, unique in the journal), `key`, the `--session` name, 1 to 128
  characters like the C4G `session_key`, and the optional `system_prompt`. The session with
  the highest `id` of a key is its current one. The session's first turn builds the system
  prompt and stores it; every later turn sends it unchanged, also after a restart. A session
  without one, or with an empty one, gets it built and stored by its next turn.
- `Message`: `session`, `seq` (position from 0, no gaps), `turn` (from 1) and `message`.
- `Step`: `session`, `turn`, `step` (from 0), `request_id`, optional `route` and `usage`.

Codegen cannot `$ref` another contract. Each C4G shape inside a C8 record (`message` a C4G
`Message`, `route` a C4G `RouteInfo`, `usage` a C4G `Usage`) is a free-form object documented
by its C4G name and not restated, so C4G stays its only definition. The journal writes these
values from the C4G types and decodes them back into the C4G types, so a stored value that is
not its C4G shape is an error on read.

### 2. Storage layout

One SQLite file at a path the caller gives; `PRAGMA user_version` holds the schema version, 2.
`Journal::open` creates the schema in one transaction on a file with version 0, migrates a
file of version 1 in one transaction (`ALTER TABLE sessions ADD COLUMN system_prompt TEXT`,
the existing sessions without a prompt) and refuses any other version.

```sql
CREATE TABLE sessions (id INTEGER PRIMARY KEY, key TEXT NOT NULL, system_prompt TEXT);
CREATE INDEX sessions_key ON sessions (key, id);
CREATE TABLE messages (
  session INTEGER NOT NULL REFERENCES sessions (id), seq INTEGER NOT NULL,
  turn INTEGER NOT NULL, message TEXT NOT NULL, PRIMARY KEY (session, seq)) WITHOUT ROWID;
CREATE TABLE steps (
  session INTEGER NOT NULL REFERENCES sessions (id), turn INTEGER NOT NULL,
  step INTEGER NOT NULL, request_id TEXT NOT NULL UNIQUE, route TEXT, usage TEXT,
  PRIMARY KEY (session, turn, step)) WITHOUT ROWID;
```

- `message`, `route` and `usage` hold the canonical C4G serialization: compact `serde_json`
  of the generated C4G type, fields in the order of that type. Reading a message back and serializing it
  gives the same bytes.
- A turn's next number is one past the highest `turn` of the session's messages and steps.
- `append` writes a turn in one `BEGIN IMMEDIATE` transaction: the messages the turn started
  with (the user prompt), then the `Outcome` messages and steps. It refuses a turn that is not
  the session's next one. On any failure nothing of the turn is stored.
- `new_session(key)` inserts an empty session without a system prompt that becomes the key's
  current one; older sessions keep their rows and stay readable by id. Nothing is deleted.
- `store_system_prompt` writes a session's prompt before the turn that built it runs, so a
  turn that fails afterwards still leaves the prompt for the next one.

Pragmas and settings are those of the `ulei-inbox` store (ADR-0007 section 4) plus foreign
keys: `journal_mode=DELETE`, `synchronous=FULL`, `cache_size=-512`, `foreign_keys=ON`, a busy
timeout of 10 s, opened read-write and create with `SQLITE_OPEN_NOFOLLOW`. `Journal::open`
creates a missing directory 0700 and a missing file 0600 (`O_EXCL`, so never through a
symlink) before SQLite opens it, so the history is readable by its owner only.

### 3. Request ids

Step `n` of turn `t` of session `s` sends `request_id` `s<s>-t<t>-<n>`, at most 64 bytes in
the C4G `^[A-Za-z0-9_-]{1,64}$`, unique per journal. No clock or randomness enters a request.
A turn whose process dies before its append stores nothing, so the next turn takes its number
and sends its request ids again.

### 4. Dependency

| Crate | Features | Why |
|---|---|---|
| `rusqlite` 0.41.1 (`libsqlite3-sys` 0.39.1, SQLite 3.54.1), workspace dependency of ADR-0007 | `bundled`, no default features | SQLite in the static binary, the same library and version as `ulei-inbox`. |

The engine gains the transitive crates `rusqlite` already brings to `ulei-core`:
`libsqlite3-sys`, `fallible-iterator`, `fallible-streaming-iterator`, `smallvec`, and the
build-only `cc`, `shlex`, `find-msvc-tools`, `pkg-config`, `vcpkg`. A musl build of the
engine needs `x86_64-linux-musl-gcc`, as the inbox does.

### 5. Measured size

Release profile of ADR-0001, rustc 1.100.0, `x86_64-unknown-linux-gnu` (dynamically linked
to glibc, stripped): the musl build of bundled SQLite needs `x86_64-linux-musl-gcc`, which
the measuring host lacks. Two throwaway `main`s, neither committed: one runs `Agent::run` with
`Registry::builtin()` on a history from its arguments; the other also opens a journal, loads
the session's history, appends the outcome and reads the steps.

| Build | Size |
|---|---|
| `ulei-engine` as committed (`--version` only; the linker drops the journal) | 282,896 B |
| throwaway `main`: loop and tools | 674,408 B |
| throwaway `main`: loop, tools and journal | 1,785,336 B |

The journal with SQLite adds 1,110,928 B on gnu, close to the 1,122,208 B that bundled SQLite
and the store SQL add to `ulei-inbox` on musl (ADR-0007 section 3). With the musl loop and
tools at 676,648 B (ADR-0013), the engine with its journal is expected near 1.8 MB, under the
2,621,440 B static budget of ADR-0010.

## Consequences

- Building `ulei-engine` compiles SQLite: glibc builds need `cc`, musl builds need `musl-gcc`.
- About 0.8 MB of the static budget of ADR-0010 is left after the journal.
- A request after a restart, after `/new`, or after an interrupted or quota-exhausted turn is
  built from the journal alone.
