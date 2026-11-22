# ADR-0059: The connector contract C11 0.1.0 and the `[connectors]` keys of C9

Status: accepted
Date: 2026-11-22
Affects: contract:C11, contract:C9, crates/contracts

## Context

ADR-0056 decides the source connectors: the contract C11 (section 2), its boundary with the
archive C10 (section 3), the runner and its log (section 4) and the owner's records (section 5.5).
Its card 1 writes the contract and the config keys and carries the record the ADR gate asks for a
new contract id. Every `too_large` id of a run is kept, with no cap.

## Decision

1. C11 0.1.0 is `contracts/c11-connector.schema.json`. Where ADR-0056 leaves a shape open, C11
   takes the smallest one that fits:
   - `Manifest` is closed: it carries no isolation field, so `network`, `landlock`, `allow`,
     `read` and `write` exist only as the owner's keys. `name` refuses `dialogue`; the program,
     `command[0]`, is relative without a `..` segment or under `/usr/`.
   - `Request` carries all five fields on every run, `cursor` and `key_file` as null when unset,
     and `limit` as a number the runner always sends.
   - `Line` is flat: `type` with the fields of its variant inline, the `Item` fields for `item`
     and the `Error` fields for `error`; `allOf` limits each variant to its fields.
   - `Page.cursor` is null while the source gave none; `Error.retry_after` is whole seconds.
   - `x-stdio` and `x-mcp` describe the transports; `x-stdio.max_line_bytes` is 1,052,672, the
     1 MiB plus 4 KiB the runner reads of a line.
   - `BrokerReply` is one of `ok` and `error`.
   - `RunRecord` names the counts, `too_large_ids` with no cap, `conflict_ids` capped at 16,
     `cursor_moved`, `landlock_abi` (absent without Landlock), `grants` (the records in effect,
     lists split), `duration_ms` and `stderr`; `unit` and `code` are absent when there is none.
2. C9 0.7.0 adds the section `connectors` with `dir`, `state`, `log`, `run_dir`, `enabled` and
   `sync_every` beside the item section `connectors.<name>` with `key_file`, `allow`, `read`,
   `write`, `network` and `landlock`. Lists are comma-separated strings, as `engine.mcp_servers`.
   `network` and `landlock` take one value each, `direct` and `off`.
3. A C9 section may hold keys and an item section at once. Its item pattern is `ITEM_NAME`
   without the names of the keys beside it, which `cargo xtask gen-contracts` requires, and the
   decoder of `ulei.toml` takes no section header that names a key.

## Consequences

- Cards 2 to 12 of ADR-0056 build against C11 0.1.0 and C9 0.7.0.
- A connector cannot be named like a key of `connectors`, such as `log` or `enabled`.
- A path in `read` or `write` cannot contain a comma.
