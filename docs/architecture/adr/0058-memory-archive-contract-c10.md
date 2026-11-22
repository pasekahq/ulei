# ADR-0058: The archive contract C10 0.1.0 and the `[memory]` keys of C9

Status: accepted
Date: 2026-11-22
Affects: crates/memory, contract:C10, contract:C9

## Context

ADR-0051 decides the archive: its item, its six operations, its transport and errors (section 2),
the dialogue pull through C6 `raw_sessions` (section 3) and the description of `ulei-memory`
(section 1). Its card 6 writes the contract, the config keys and the crate's description, and
carries the record the ADR gate asks for a new contract id. ADR-0056 section 3 is C10's other
client: the connectors' runner puts each item under its manifest name and calls `sync` on its
schedule.

## Decision

1. C10 0.1.0 is `contracts/c10-archive.schema.json`, in the style of C7: `x-methods` `put`,
   `search`, `read`, `delete`, `sync`, `health`, `x-transport` and `Error` as ADR-0051 section 2
   tables them. Where that table leaves a shape open, C10 takes the smallest one that fits:
   - `put` carries `source`, `external_id`, `who`, `at`, `kind`, `text` and `bytes_b64`; the
     archive computes `id` and `put_at`. These are ADR-0056's `Item` fields plus `source`.
   - The 1 MiB cap is `x-transport.max_body_bytes` 1,048,576 on the whole request body, as in
     C7 and as ADR-0056 section 3 measures a `put`; a larger body is `413 too_large`.
   - An item's metadata is a list of `Entry` (`source`, `external_id`, `who`, `at`, `kind`,
     `put_at`), one per `(source, external_id)`; a search hit carries its entries that match the
     filters, `read` and `Deletion` carry all of them. `read`'s `kind` is its first entry's.
   - `search`'s `since` and `until` bound an entry's `at`; an entry without `at` matches no time
     filter.
   - `sync` answers `put`, the count, and `synced`, false when the engine did not answer, as
     `search` does; no error code is added for an engine that does not answer.
   - `health` answers `version`, `items`, `last_sync_at` and `sync_error`.
   - Times are Unix seconds as JSON numbers, as in C6 `RawMessage.ts` and C7.
2. C9 0.6.0 adds the section `memory` with `memory.archive` and `memory.socket`; `ulei-memory
   serve` also reads `engine.c6_socket`, which exists.
3. `ulei-memory` declares C10 (served) and C6 (read) and re-exports their generated types; it
   holds no behaviour until ADR-0051 card 7.

## Consequences

- Cards 7 to 10 of ADR-0051 and cards of ADR-0056 build against C10 0.1.0.
- The largest original a `put` carries is about 768 KiB, since base64 and the other fields share
  the 1 MiB body.
