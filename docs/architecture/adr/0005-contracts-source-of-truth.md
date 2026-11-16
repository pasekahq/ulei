# ADR-0005: contracts/ is the single source of truth, semver per contract

Status: accepted
Date: 2026-11-16
Affects: contract:C1, contract:C2, contract:C3, contract:C4, contract:C5, contract:C6, contract:C7

## Decision

- `contracts/index.json` lists every contract: id, file, kind (`json-schema` | `openapi`),
  version, status (`stable` | `draft` | `vendored`), title, source.
- Schemas are JSON Schema 2020-12 or OpenAPI 3.1 (JSON). They stay language-neutral: Python
  components validate against the same files.
- Golden fixtures live in `contracts/fixtures/<cN>/{valid,invalid}/<Definition>.<case>.json`
  (`root` = the document root). `cargo xtask contracts` checks the metaschema, that valid
  fixtures are accepted and invalid ones rejected by the full schema, and that every version
  has a `contracts/CHANGELOG.md` entry.
- Rust types are generated (`cargo xtask gen-contracts`); the generated round-trip test makes
  every valid fixture deserialize and serialize back unchanged.
- Each contract has its own semver. Any schema change bumps the version. A breaking change
  (removed field/definition/enum value/operation, newly required field, changed type or
  validation keyword, `additionalProperties` becoming `false`) needs a major bump (minor
  while `0.x`) and an ADR with `Affects: contract:<id>`. CI checks it against the base revision.
- C1 starts at 1.0.0 because it describes the packet already stored in production.
