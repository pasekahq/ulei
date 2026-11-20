# Contracts

Language-neutral contracts between modules and with external products. `index.json` lists
them; `CHANGELOG.md` records every version; `fixtures/` holds golden examples.

- Kinds: JSON Schema 2020-12 (`*.schema.json`, `c2-mcp-tools.json`) and OpenAPI 3.1 (`*.openapi.json`).
- Fixtures: `fixtures/<cN>/valid/<Definition>.<case>.json` must validate,
  `fixtures/<cN>/invalid/...` must not. `root` means the document root. A case named `extra_*`
  carries a field its definition does not declare: valid, its open type parses it and drops the
  field; invalid, its closed type refuses it.
- Rust types are generated into `crates/contracts/src/generated/` (`cargo xtask gen-contracts`).
  Supported subset: objects (`properties`, `required`, `additionalProperties: false`),
  free-form objects, string enums, string/integer/number/boolean, arrays, a required property
  typed `[<type>, "null"]`, `$ref` to named definitions or to `#` (the typed document root).
  Validation keywords are checked by the schema, not encoded in types. A type rejects unknown
  fields exactly where its schema says `additionalProperties: false`.
- Versioning and compatibility rules: ADR-0005. Vendored contracts (C4, C4G) change only
  together with the qb pin in `ecosystem.lock` (ADR-0038): C4 is transcribed from qb's
  `crates/qb-proto`, C4G is qb's `docs/openapi-gateway.json` byte for byte with qb's version.
