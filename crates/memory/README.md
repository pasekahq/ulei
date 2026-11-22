# ulei-memory

Memory below the garden: the archive of raw items of every source as they came, immutable and
stored without loss, indexed by FTS5 and metadata (ADR-0051 section 2, contract C10), and later
the forest made from it. Rules for coding agents: [`AGENTS.md`](AGENTS.md).

The library `archive::Archive` holds the store over one SQLite file; the binary `ulei-memory`
serves it as the daemon `ulei-memory serve` and removes from it by the owner's command
`ulei-memory delete`. The pull of the engine's dialogue and the engine's tools are the next cards
of ADR-0051 section 8.

## The archive file

`memory.archive` is one SQLite file in WAL mode, created 0600 in a directory created 0700. An
existing file or directory that grants any permission to group or others is refused at open, so
neither the daemon nor `delete` starts on it.

| Table | Holds |
|---|---|
| `blobs` | One row per item: `id` (lowercase hex SHA-256 of the original), `len`, `codec`, `body` |
| `entries` | One row per `(source, external_id)`: the item `id`, `who`, `at`, `kind`, `put_at` |
| `item_fts` | FTS5 over the putter's text, tokenizer `unicode61 remove_diacritics 2`, rowid the item's `blobs` row; it keeps its text for `snippet()` |
| `cursors` | The position of each pulled source |
| `deletions` | The audit: one row per delete with its time, `id`, `len`, the removed entries, the reason and the caller's pid and uid |

The body is the raw deflate stream of the original at level 6 (`codec` `deflate`), or the
original itself (`none`) when deflate does not make it shorter. A read inflates the body and
checks its length and SHA-256 against the item before it answers; a mismatch is refused as a
corrupt item.

## Operations

| Operation | Does |
|---|---|
| `put` | Stores the bytes as a new item unless their id is stored, and a new entry unless its `(source, external_id)` is stored; one transaction. A stored pair with the same bytes answers `duplicate` and changes nothing; with other bytes it is refused as a conflict. A new pair with stored bytes adds only an entry. |
| `search` | FTS5 over the text with the filters `source`, `who`, `kind`, `since`, `until` on the entries (an entry without `at` passes no time filter), `limit` 1 to 50 (default 10), best first, each hit with its matching entries and a `snippet()` marked `>>>` and `<<<`. The query is sanitized as the engine's history search does it; a query FTS5 still rejects finds nothing. |
| `read` | The original bytes, the first entry's `kind`, the length and every entry, oldest first. |
| `delete` | Removes the item, its FTS5 row and its entries and writes the `deletions` row in the same transaction. |
| `deletions`, `items` | The audit rows, oldest first; the count of items. |

Only `delete` removes rows. Triggers refuse any update of `blobs`, `entries` and `deletions`
and any delete from `deletions`. Inputs follow the C10 method inputs; an `external_id` over 512
bytes of UTF-8 is refused. Each error maps to its C10 `ErrorCode`.

## The daemon

`ulei-memory serve [--config PATH]` reads `memory.archive` and `memory.socket` from `ulei.toml`
(the flag, else `ULEI_CONFIG`, `XDG_CONFIG_HOME`, `HOME`), opens the archive, binds the socket
at mode 0600 in a directory of its own uid that no other user can reach (created 0700 when
missing), prints `ulei-memory ready <socket>` on stdout and serves until `accept` fails. Every
failure to start is one stderr line and exit 1.

C10 `x-transport`: `POST /v1/<method>` with a JSON body over HTTP/1.1, one request per
connection, served one at a time, every connection's reads and writes within 5 s, bodies at most
1 MiB, headers at most 8 KiB. A peer whose `SO_PEERCRED` uid is not the daemon's is answered
`403 forbidden` before anything is read.

| Method | Answers |
|---|---|
| `put` | `bytes_b64` decoded as standard base64 with padding, then the archive's `put` |
| `search` | the archive's hits with `synced: true` |
| `read` | the original as `bytes_b64`, its `kind`, `len` and entries |
| `sync` | `put: 0`, `synced: true`: the dialogue pull does not exist yet |
| `health` | `version`, `items`; no `last_sync_at` and no `sync_error` |

`delete` is not served on the socket: a request for it is `404 unknown_method`. Errors are C10
`Error` bodies: `invalid_request` 400 (not the method input, `bytes_b64` that does not decode,
an input the archive refuses), `forbidden` 403, `unknown_method` 404, `not_found` 404,
`conflict` 409, `too_large` 413, `store_failed` 500. The daemon has no timer: it sleeps in
`accept` or in a connection's read until I/O.

## The owner's delete

`ulei-memory delete <id> --reason <text> [--config PATH]` opens the archive named by the same
config, refuses one that does not exist rather than creating it, and runs the archive's
`delete` with this process's pid and real uid as the caller. It prints one stderr line: the id
with the removed length and entry count and exit 0, or why nothing was removed and exit 1. An
unknown id writes nothing.
