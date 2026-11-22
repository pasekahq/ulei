# ulei-memory

Memory below the garden: the archive of raw items of every source as they came, immutable and
stored without loss, indexed by FTS5 and metadata (ADR-0051 section 2, contract C10), and later
the forest made from it. Rules for coding agents: [`AGENTS.md`](AGENTS.md).

The crate is a library today: `archive::Archive` over one SQLite file. The daemon
`ulei-memory serve` with the C10 socket, the owner's `ulei-memory delete`, the pull of the
engine's dialogue and the engine's tools are the next cards of ADR-0051 section 8.

## The archive file

`memory.archive` is one SQLite file in WAL mode, created 0600 in a directory created 0700.

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
