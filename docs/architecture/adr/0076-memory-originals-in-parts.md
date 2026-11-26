# ADR-0076: The archive takes originals over 1 MiB in parts

Status: accepted
Date: 2026-11-26
Affects: crates/memory, crates/engine, contract:C10, ci/units_run.py
Amended by: ADR-0081 (the unit's `MemoryMax` is 256M).

## Context

C10 `put` (ADR-0051 section 2) carries an item's original as `bytes_b64` in one JSON body, and
`ulei-memory serve` caps a body at 1 MiB, so an original over about 768 KiB, or one whose body
with its text passes 1 MiB, can never enter the archive. The history the archive must hold
(ADR-0070) has such items: records of the previous runtime whose original or whose body with
its text is over 1 MiB, and a document library with several hundred files over 1 MiB, about
3 GB together, the largest several hundred MB (PDF, video, archives). The archive must hold each
of them whole, verifiable by its SHA-256, and `read` must give it back.

What decides the design:

- The daemon's unit has `MemoryMax=32M` (256M since ADR-0081) and `TasksMax=2` (ADR-0070 section 2). An original of
  hundreds of MB can be in memory as one buffer on neither side, nor be one SQLite value
  (`SQLITE_MAX_LENGTH` is 10^9, and SQLite reads a row's value whole).
- An item's id stays the lowercase hex SHA-256 of all its original bytes; dedup and the
  `(source, external_id)` entries keep their meaning.
- Archives written by the schema before stay readable without a rewrite.

## Decision

### 1. Why not a raised cap

A higher `max_body_bytes` moves the limit and keeps its cause. A body is read whole, decoded
whole, deflated whole and stored as one value: memory grows with the original, so a cap that
fits `MemoryMax=32M` with the decoded and deflated copies is still a few MiB, far under the
largest original, and no cap reaches past `SQLITE_MAX_LENGTH`. A 5 s deadline per connection
also bounds what one request can carry. Parts keep every request, every buffer and every
stored value bounded whatever the original's size.

### 2. Upload in parts (C10 0.4.0, additive)

Three methods beside `put`, each one request of at most 1 MiB:

| Method | Input | Does |
|---|---|---|
| `put_begin` | `source`, `external_id`, `who`, `at`, `kind`, `text`, `len` (1 to 16 GiB), `id` | Removes every expired upload. A stored `(source, external_id)` answers `status` `duplicate` with the same id and is refused `409 conflict` with another, as `put` does; a stored id adds the entry at once and answers `status` `stored`; otherwise it opens an upload and answers its number `upload`. |
| `put_part` | `upload`, `index`, `bytes_b64` of 1 to 524,288 bytes | Stores the next part: `index` must be the number of parts the upload holds, and the part must not pass `len`; answers `parts` and `received`. |
| `put_end` | `upload` | Inflates the parts one at a time and hashes them; `len` bytes hashing to `id` store the item, its index row and its entry in one transaction and answer `PutResponse`; missing bytes or another SHA-256 are refused `400 invalid_request` and remove the upload with its parts, so nothing is stored. |

An upload not touched by a `put_begin` or `put_part` for 3,600 s expires; the next `put_begin`
removes it with its parts, and a `put_part` or `put_end` of it answers `404 not_found`. The
daemon keeps no timer for it (0 wake-ups when idle). Uploads are independent: an abandoned one
blocks nothing, a new one of the same record runs beside it, and of two uploads of one record
the one that ends second answers `duplicate` and drops its parts. An upload's number is a row
number with `AUTOINCREMENT`, never given twice by one archive. The uploads and their parts are
rows of the archive file, so an upload survives a restart of the daemon.

Of the text of `put_begin` the archive keeps and indexes the first 262,144 bytes of UTF-8, cut
at a character boundary: the putter extracts the text, and a quarter MiB is far more than a
hit shows (1,200 characters) and keeps the `put_begin` body under the 1 MiB cap. `put` keeps its
text whole as before.

`put_begin` trusts the `id` the caller names when that id is stored and adds the entry without
parts. The caller is the daemon's own uid (the peer check), the same that may open the file;
the alternative, sending hundreds of MB again to learn that they are stored, makes a rerun of
an import cost its whole size.

### 3. Storage (schema version 3)

Version 3 adds three tables and rewrites no row; a file of version 1 or 2 gains them at open.

| Table | Holds |
|---|---|
| `uploads` | One row per open upload: the entry's fields, the kept text, `len`, `id`, the parts and bytes received, the time it was last touched |
| `parts` | One row per part: `owner` (the upload's row), `seq`, `start` (offset in the original), `len`, the part's own SHA-256, `codec` and `body`, deflated when that is shorter |
| `chunked` | One row per item stored in parts: its `id` and the `owner` of its parts |

An item in parts has its `blobs` row with its `len`, codec `none` and an empty body, so
`entries`, `item_fts`, `items` and `delete` treat it as any other item. `put_end` links the
upload's parts to the item through `chunked` and removes the `uploads` row; the parts are never
copied. Triggers refuse every update of `parts` and `chunked`; `delete` removes the item's
`chunked` row and its parts with the rest, in its transaction.

### 4. `read` by range

`ReadRequest` gains `offset` and `length` (at most 524,288); with either, `read` answers that
range cut at the end of the original, with `len` the whole length and `offset` echoed. The parts
the range touches are inflated one at a time and each is checked against its stored SHA-256; a
range of an item stored whole checks the whole original as before. An item in parts is read
by range only: a whole `read` of it is refused `413 too_large`, so a client that knows only the
whole read gets a clear refusal instead of a truncated original. A `read` without `offset` and
`length` of an item stored whole does not change. A reader streams an item with ranges of
524,288 bytes from offset 0 until an empty range, hashes the bytes and compares them with the id.

### 5. `ulei-memory import`

- A line is a C10 `PutRequest`, or one with `bytes_path` (a local file the import client reads)
  instead of `bytes_b64`, each with an optional `id`; exactly one of `bytes_b64` and `bytes_path`.
  The client reads the file once to hash it, checks the hash against `id` when the line names
  one, and refuses the line `invalid_request` with the reason before any request when it does
  not match or the file cannot be read. The daemon never opens a client's path.
- The client never holds a line whole. It scans each line as it reads it: the members but
  `bytes_b64` are kept as they stand, at most 4 MiB together (more refuses the line
  `too_large`); the `bytes_b64` string is decoded as it is read, held in memory up to 1 MiB and
  past that written to an unlinked temporary file of the user's temporary directory.
- When the `put` body of the line, with the original in base64, fits 1 MiB, the line goes as one
  `put` as before; otherwise it goes as an upload in parts of 524,288 bytes, whatever form it came
  in, with the text cut as `put_begin` keeps it. A transient fault (transport, `store_failed`,
  an upload that expired) retries the whole line from `put_begin` with the pauses of ADR-0070;
  the parts of the failed try expire.
- Each refused line stays one stderr line, `import: line <n>: refused <code>`, with `: <reason>`
  added for a refusal before any request; the counts and exit codes keep their meaning.

### 6. Memory

The daemon holds at most one request body, one part decoded and deflated, and for a range one
part inflated and the answer: a few MiB whatever the original's size. `ci/units_run.py` puts a
40 MiB original through `ulei-memory import` into `ulei-memory.service` under its `MemoryMax`,
reads it back by ranges, and checks the SHA-256, the process's VmHWM, `oom_kill` 0 and no
restart.

## Consequences

- C10 0.4.0: three methods and optional fields, no breaking change; old clients keep working,
  and an old client's whole `read` of an item in parts is refused `too_large`.
- The engine's `archive_read` reads an item whole, so for an item in parts it answers that the
  archive refused it as `too_large`; reading the head of such an item is left to a later change.
- An archive of schema version 3 is not opened by an older `ulei-memory`, which refuses an
  unknown version at open.
- An upload takes the disk of its parts until it ends or expires; an item stored in parts is
  read part by part, so a short range still inflates every part it touches whole.
- Supersedes the 1 MiB line limit of `ulei-memory import` in ADR-0070 section 3; the rest of that
  section stands.
