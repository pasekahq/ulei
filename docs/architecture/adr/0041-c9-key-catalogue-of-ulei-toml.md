# ADR-0041: C9 0.1.0, the key catalogue of `ulei.toml`

Status: accepted
Date: 2026-11-20
Affects: contract:C9

## Context

ADR-0040 section 1 decides a config contract C9 for `ulei.toml` and exempts it from the
generated types and the fixture round trip of ADR-0005. The ADR gate needs an added ADR naming
`contract:C9` for the new index entry; this record states how the tooling carries C9.

## Decision

- `contracts/c9-ulei-toml.schema.json`, C9 0.1.0, status `draft`: the sections and keys of
  ADR-0040 section 3, every section closed, every key with a kind of ADR-0040 section 2, the
  item sections `engine.routes.<alias>` and `engine.mcp.<name>` as `patternProperties` of
  `^[A-Za-z0-9_-]+$`, the bare TOML key. No key holds a secret value.
- Its index entry carries `generate: keys` and `example: docs/ulei.example.toml`.
  `cargo xtask gen-contracts` writes `ulei_contracts::generated::c9` with `Kind` and `KEYS`, the
  dotted key patterns (`*` for one item name) with their kinds, and no fixture test.
- Its fixtures are TOML, parsed by `xtask`'s `toml` into the JSON document the schema checks;
  `cargo xtask contracts` checks `docs/ulei.example.toml` the same way.
- The catalogue walk accepts only closed sections, the one item pattern, `$ref` to `$defs` and
  key schemas of `type` alone; any other construct fails generation, so `KEYS` never says less
  than the schema.

## Consequences

- A new key is a C9 change with its changelog entry, fixtures and example line.
- The decoder that reads `KEYS` is ADR-0040 section 8 card 2; no binary changes here.
