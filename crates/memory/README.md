# ulei-memory

Memory below the garden: the archive of raw items of every source as they came, immutable and
stored without loss, indexed by FTS5 and metadata (ADR-0051 section 2, contract C10), and later
the forest made from it. Rules for coding agents: [`AGENTS.md`](AGENTS.md).

The library `archive::Archive` holds the store over one SQLite file; `pull` pulls the engine's
dialogue into it through C6 `raw_sessions` (ADR-0051 section 3); the binary `ulei-memory` serves
it as the daemon `ulei-memory serve`, removes from it by the owner's command
`ulei-memory delete`, and fills it from a stream of items with `ulei-memory import`. The binary
is released with the others and runs under the user unit `deploy/systemd/ulei-memory.service`
(ADR-0070); setting it up and importing a former runtime's history:
[`docs/runbooks/memory-archive.md`](../../docs/runbooks/memory-archive.md).

## The archive file

`memory.archive` is one SQLite file in WAL mode, created 0600 in a directory created 0700, opened
only by the daemon and by `delete`. An existing file or directory that another uid owns or that
grants any permission to group or others is refused at open, so neither the daemon nor `delete`
starts on it. A file of schema version 1 gains the table `skipped` at open, a file of
version 1 or 2 the tables `uploads`, `parts` and `chunked` (version 3, ADR-0076), and a file of
version 1 to 3 the table `entry_seq` (version 4, ADR-0078), its entries numbered in the order
`read` lists them, by `put_at` then by row; no stored row is rewritten.

| Table | Holds |
|---|---|
| `blobs` | One row per item: `id` (lowercase hex SHA-256 of the original), `len`, `codec`, `body` |
| `entries` | One row per `(source, external_id)`: the item `id`, `who`, `at`, `kind`, `put_at` |
| `entry_seq` | One row per entry: `seq`, its place in the order the archive stored the entries, given once by `AUTOINCREMENT` in the transaction of the entry's insert, and the entry's `source` and `external_id` |
| `item_fts` | FTS5 over the putter's text, tokenizer `unicode61 remove_diacritics 2`, rowid the item's `blobs` row; it keeps its text for `snippet()` and a hit's text |
| `cursors` | The position of each pulled source after its last page that gave one |
| `skipped` | One row per record a source named without its bytes: `source`, `external_id`, `len`, `at`, `skipped_at`; never updated or removed |
| `deletions` | The audit: one row per delete with its time, `id`, `len`, the removed entries, the reason and the caller's pid and uid |
| `uploads` | One row per open upload of an original in parts: the entry's fields, the kept text, `len`, `id`, the parts and bytes received, when it was last touched |
| `parts` | One row per part of an upload or of an item in parts: `owner` (the upload's row), `seq`, `start`, `len`, the part's SHA-256, `codec`, `body` |
| `chunked` | One row per item stored in parts: its `id` and the `owner` of its parts |

The body is the raw deflate stream of the original at level 6 (`codec` `deflate`), or the
original itself (`none`) when deflate does not make it shorter. A read inflates the body and
checks its length and SHA-256 against the item before it answers; a mismatch is refused as a
corrupt item.

An original whose `put` body would pass 1 MiB goes in parts (ADR-0076): its `blobs` row has its
`len`, codec `none` and an empty body, and its bytes are its `parts`, each at most 512 KiB,
deflated as a body is, with its own SHA-256. `end` hashes the parts one at a time before the
item is stored; a range read inflates only the parts it touches and checks each against its
SHA-256. Neither side ever holds the whole original.

## Operations

| Operation | Does |
|---|---|
| `put` | Stores the bytes as a new item unless their id is stored, and a new entry unless its `(source, external_id)` is stored; one transaction. A stored pair with the same bytes answers `duplicate` and changes nothing; with other bytes it is refused as a conflict. A new pair with stored bytes adds only an entry. |
| `search` | FTS5 over the text with the filters `source`, `who`, `kind`, `since`, `until` on the entries (an entry without `at` passes no time filter), `limit` 1 to 50 (default 10), best first, each hit with its matching entries, a `snippet()` marked `>>>` and `<<<`, and its text cut to the first 1,200 characters with `text_truncated` true when the text is longer. The query is sanitized as the engine's history search does it; a query FTS5 still rejects finds nothing. |
| `list` | The entries of one `source` after the `seq` `after` (default 0), by `seq`, at most `limit` 1 to 500 (default 100), each with its item's id, its entry's fields and its item's text cut to the first `text_chars` 1 to 16,384 characters (default 4,000), `text_truncated` true when cut. The page ends before an entry that would take its body past 786,432 bytes, but holds one entry when one follows; `more` says whether entries follow it. Reads only. |
| `read` | The original bytes, the first entry's `kind`, the length and every entry, oldest first. An item in parts is refused `too_large`. |
| `read_range` | At most 512 KiB of the original from an offset, cut at its end, with the same fields; each part it touches is checked against its SHA-256, an item stored whole is checked whole. |
| `begin` | Opens an upload of an original in parts (`len` 1 to 16 GiB, its `id`) after removing the expired ones; settles it at once as `put` would when the `(source, external_id)` or the id is stored. Keeps the first 256 KiB of the text, cut at a character. |
| `part` | Stores the next part of an open upload, in order, 1 to 512 KiB, not past `len`. |
| `end` | Checks that the parts make `len` bytes hashing to `id`, then stores the item over them, its index row and its entry in one transaction; missing bytes or another hash remove the upload and store nothing. An upload expires 3,600 s after its last `begin` or `part`. |
| `delete` | Removes the item, its FTS5 row, its entries with their `seq` and its parts and writes the `deletions` row in the same transaction. |
| `pull` | Stores one page of a pulled source in one transaction: its items as `put` does them, its skipped references unless kept, and its cursor when the page gives one; answers the items stored. A failure stores nothing of the page. |
| `cursor`, `skipped` | The stored position of a source; every skipped reference, oldest first. |
| `deletions`, `items` | The audit rows, oldest first; the count of items. |

Only `delete` removes stored rows; `begin` and `end` remove expired or refused uploads with
their parts. Triggers refuse any update of `blobs`, `entries`, `entry_seq`, `parts`, `chunked`,
`skipped` and `deletions` and any delete from `skipped` and `deletions`; others give each new
entry its `seq` and drop the `seq` of a deleted one, which is never given again. Inputs follow the C10 method inputs; an
`external_id` over 512 bytes of UTF-8 is refused. Each error maps to its C10 `ErrorCode`.

## The daemon

`ulei-memory serve [--config PATH]` reads `memory.archive`, `memory.socket` and
`engine.c6_socket` from `ulei.toml` (the flag, else `ULEI_CONFIG`, `XDG_CONFIG_HOME`, `HOME`),
opens the archive for its own uid, binds the socket at mode 0600 in a directory of its own uid
that no other user can reach (created 0700 when missing), pulls the dialogue once, prints
`ulei-memory ready <socket>` on stdout and serves until `accept` fails. Every failure to start
is one stderr line: exit 2 when the config is refused (no restart fixes it), else exit 1; a
failed pull is not one, `health` names it.

C10 `x-transport`: `POST /v1/<method>` with a JSON body over HTTP/1.1, one request per
connection, served one at a time, every connection's reads and writes within 5 s, bodies at most
1 MiB, headers at most 8 KiB. A peer whose `SO_PEERCRED` uid is not the daemon's is answered
`403 forbidden` before anything is read.

| Method | Answers |
|---|---|
| `put` | `bytes_b64` decoded as standard base64 with padding, then the archive's `put` |
| `put_begin` | the archive's `begin`: `upload`, or `status` when it is settled |
| `put_part` | `bytes_b64` decoded, then the archive's `part`: `parts` and `received` |
| `put_end` | the archive's `end`: `id` and `status` |
| `search` | pulls, then the archive's hits with `synced` true when the pull reached the engine's last page |
| `list` | the archive's `list`, without a pull |
| `read` | the original as `bytes_b64`, its `kind`, `len` and entries; with `offset` or `length` (default 0 and 512 KiB) the range and its `offset` |
| `sync` | pulls; `put`, the dialogue items it stored, and `synced` |
| `health` | `version`, `items`, `last_sync_at` of the last pull that reached the engine's last page, `sync_error` of the last pull when it failed, every skipped reference as `skipped` |

`delete` is not served on the socket: a request for it is `404 unknown_method`. Errors are C10
`Error` bodies: `invalid_request` 400 (not the method input, `bytes_b64` that does not decode,
an input the archive refuses, an upload refused at its end), `forbidden` 403, `unknown_method`
404, `not_found` 404 (also an upload not open), `conflict` 409, `too_large` 413 (also a whole
`read` of an item in parts), `store_failed` 500. The daemon has no timer: it sleeps in
`accept` or in a connection's read until I/O.

## The dialogue pull

A pull asks the engine's socket for C6 `raw_sessions` pages of at most 100 messages after the
cursor stored for source `dialogue`, each exchange within 5 s and its answer at most 512 KiB,
until a page carries no cursor. Each message becomes an item: `source` `dialogue`,
`external_id` `<session_key>/<session_id>/<seq>`, `who` its role, `at` its `ts`, `kind`
`application/vnd.ulei.c4g-message+json`, the bytes of `message` and the text of `content`. Each
`skipped` message of the page becomes a reference in `skipped` under the same external id, with
its `len` and `ts`. The page's items, references and cursor are stored in one transaction, so a
pull cut between pages repeats at most one page, which dedup answers `duplicate`. The engine's
last page carries no cursor, so the next pull reads that page again and stores nothing of it
twice. A message without `session_key`, `seq` or `message`, a cursor that is not a number above
the one sent, a non-200 answer, an archive failure or a missed deadline stops the pull; the pages
before it stay stored.

## The owner's delete

`ulei-memory delete <id> --reason <text> [--config PATH]` opens the archive named by the same
config, refuses one that does not exist rather than creating it, and runs the archive's
`delete` with this process's pid and real uid as the caller. It prints one stderr line: the id
with the removed length and entry count and exit 0, or why nothing was removed and exit 1. An
unknown id writes nothing.

## The import

`ulei-memory import [--skip N] [--config PATH]` reads JSON Lines on stdin and puts each line
through the daemon at `memory.socket`, the transport the connectors' runner uses; it opens no
archive file and reads no other key. Compressed input is decompressed before it:
`zcat history.jsonl.gz | ulei-memory import`. A line is a C10 `PutRequest` (`source`,
`external_id`, `who`, `at`, `kind`, `text`, `bytes_b64`), or one with `bytes_path`, the path of
a local file the client reads, instead of `bytes_b64`; either may add `id`, the original's
lowercase hex SHA-256, which the client checks before it sends anything. The daemon never opens
a client's path.

A line is never held whole: the client scans it as it reads, keeps its members but `bytes_b64`
(at most 4 MiB together) and decodes `bytes_b64` on the way, into memory up to 1 MiB and past
that into an unlinked temporary file in `$TMPDIR` (default `/tmp`), which needs room for the
largest such original. A line whose `put` body fits 1 MiB goes as one `put`; any other goes in
parts of 512 KiB (`put_begin`, `put_part`, `put_end`, ADR-0076), with its text cut to the first
256 KiB, the part the archive indexes. A repeated item is settled by `put_begin` before any part
is sent.

| Line | Counted | stderr |
|---|---|---|
| answered `stored` | `stored` | |
| answered `duplicate` (the same `(source, external_id)` and bytes are stored) | `duplicate` | |
| not an import line: not JSON, an unknown member, bad base64, both or neither of `bytes_b64` and `bytes_path` (not sent) | `refused` | `import: line <n>: refused invalid_request` |
| `bytes_path` not a readable regular file, or an original whose SHA-256 is not `id` (not sent) | `refused` | `import: line <n>: refused invalid_request: <reason>` |
| members but `bytes_b64` over 4 MiB (not sent) | `refused` | `import: line <n>: refused too_large` |
| answered `invalid_request` (an upload whose parts do not hash to its id included), `conflict` or `too_large` | `refused` | `import: line <n>: refused <code>` |

Lines are numbered from 1; `--skip N` reads and drops the first `N`. A transport failure
(connect, write, read, a 30 s deadline, no answer), `store_failed` or an upload that expired is
tried again, the whole line from its start, after 1, 2 and 4 s; when the fourth try fails too,
or the daemon answers anything else (`forbidden`, `unknown_method`, a malformed answer), the
import stops with
`import: stopped at line <n>: <reason>; lines 1 to <m> are done, resume with --skip <m>` and
exit 1; a failure to write the temporary file stops it too. A rerun from any line is safe: what
is stored answers `duplicate`. Every 10,000 lines it writes `import: at line <n>: <counts>`, and
last `import: <lines> lines, <stored> stored, <duplicate> duplicate, <refused> refused`, with
`<lines>` the lines after the skipped ones. Exit 0 when stdin was read to its end, refused lines
or not; 1 on a stop or a refused config; 2 on a usage error.
