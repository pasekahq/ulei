# ulei-memory

Memory below the garden: the archive of raw items of every source as they came, immutable and
stored without loss, indexed by FTS5 and metadata (ADR-0051 section 2, contract C10), and later
the forest made from it. Rules for coding agents: [`AGENTS.md`](AGENTS.md).

The library `archive::Archive` holds the store over one SQLite file; `pull` pulls the engine's
dialogue into it through C6 `raw_sessions` (ADR-0051 section 3); the binary `ulei-memory` serves
it as the daemon `ulei-memory serve` and removes from it by the owner's command
`ulei-memory delete`.

## The archive file

`memory.archive` is one SQLite file in WAL mode, created 0600 in a directory created 0700, opened
only by the daemon and by `delete`. An existing file or directory that another uid owns or that
grants any permission to group or others is refused at open, so neither the daemon nor `delete`
starts on it. A file of schema version 1 gains the table `skipped` at open.

| Table | Holds |
|---|---|
| `blobs` | One row per item: `id` (lowercase hex SHA-256 of the original), `len`, `codec`, `body` |
| `entries` | One row per `(source, external_id)`: the item `id`, `who`, `at`, `kind`, `put_at` |
| `item_fts` | FTS5 over the putter's text, tokenizer `unicode61 remove_diacritics 2`, rowid the item's `blobs` row; it keeps its text for `snippet()` |
| `cursors` | The position of each pulled source after its last page that gave one |
| `skipped` | One row per record a source named without its bytes: `source`, `external_id`, `len`, `at`, `skipped_at`; never updated or removed |
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
| `pull` | Stores one page of a pulled source in one transaction: its items as `put` does them, its skipped references unless kept, and its cursor when the page gives one; answers the items stored. A failure stores nothing of the page. |
| `cursor`, `skipped` | The stored position of a source; every skipped reference, oldest first. |
| `deletions`, `items` | The audit rows, oldest first; the count of items. |

Only `delete` removes rows. Triggers refuse any update of `blobs`, `entries`, `skipped` and
`deletions` and any delete from `skipped` and `deletions`. Inputs follow the C10 method inputs; an `external_id` over 512
bytes of UTF-8 is refused. Each error maps to its C10 `ErrorCode`.

## The daemon

`ulei-memory serve [--config PATH]` reads `memory.archive`, `memory.socket` and
`engine.c6_socket` from `ulei.toml` (the flag, else `ULEI_CONFIG`, `XDG_CONFIG_HOME`, `HOME`),
opens the archive for its own uid, binds the socket at mode 0600 in a directory of its own uid
that no other user can reach (created 0700 when missing), pulls the dialogue once, prints
`ulei-memory ready <socket>` on stdout and serves until `accept` fails. Every failure to start
is one stderr line and exit 1; a failed pull is not one, `health` names it.

C10 `x-transport`: `POST /v1/<method>` with a JSON body over HTTP/1.1, one request per
connection, served one at a time, every connection's reads and writes within 5 s, bodies at most
1 MiB, headers at most 8 KiB. A peer whose `SO_PEERCRED` uid is not the daemon's is answered
`403 forbidden` before anything is read.

| Method | Answers |
|---|---|
| `put` | `bytes_b64` decoded as standard base64 with padding, then the archive's `put` |
| `search` | pulls, then the archive's hits with `synced` true when the pull reached the engine's last page |
| `read` | the original as `bytes_b64`, its `kind`, `len` and entries |
| `sync` | pulls; `put`, the dialogue items it stored, and `synced` |
| `health` | `version`, `items`, `last_sync_at` of the last pull that reached the engine's last page, `sync_error` of the last pull when it failed, every skipped reference as `skipped` |

`delete` is not served on the socket: a request for it is `404 unknown_method`. Errors are C10
`Error` bodies: `invalid_request` 400 (not the method input, `bytes_b64` that does not decode,
an input the archive refuses), `forbidden` 403, `unknown_method` 404, `not_found` 404,
`conflict` 409, `too_large` 413, `store_failed` 500. The daemon has no timer: it sleeps in
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
