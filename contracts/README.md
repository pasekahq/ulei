# Contracts

Language-neutral contracts between modules and with external products. `index.json` lists
them; `CHANGELOG.md` records every version; `fixtures/` holds golden examples.

- Kinds: JSON Schema 2020-12 (`*.schema.json`, `c2-mcp-tools.json`) and OpenAPI 3.1 (`*.openapi.json`).
- Fixtures: `fixtures/<cN>/valid/<Definition>.<case>.json` must validate,
  `fixtures/<cN>/invalid/...` must not. `root` means the document root.
- Rust types are generated into `crates/contracts/src/generated/` (`cargo xtask gen-contracts`).
  Supported subset: objects (`properties`, `required`, `additionalProperties: false`),
  free-form objects, string enums, string/integer/number/boolean, arrays, `$ref` to named
  definitions or to `#` (the typed document root). Validation keywords are checked by the schema, not encoded in types.
- Versioning and compatibility rules: ADR-0005. Vendored contracts (C4) change only together
  with the pin in `ecosystem.lock`.
