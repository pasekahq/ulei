# ADR-0067: The crate `connector-mcp-example`, the MCP wrapper example of C11

Status: accepted
Date: 2026-11-23
Affects: crates/connector-mcp-example, dep:ulei-contracts, dep:serde_json, dep:listenfd

## Context

ADR-0056 section 7 decides the MCP wrapper example `ulei-connector-mcp-example` and its card 10
builds it as the crate `crates/connector-mcp-example`. The ADR gate asks for an ADR added with a
new crate and its dependencies. The example starts the wrapped server as a child, and a child
inherits every fd without close-on-exec, the run's broker socket included (ADR-0056 section 5.3).
The workspace denies `unsafe`, and neither std nor `serde_json` owns an inherited fd by its
number, as ADR-0065 found for the test connector.

## Decision

- The workspace gains `crates/connector-mcp-example`, package `ulei-connector-mcp-example`, class
  2 with role "source connector" and contracts `["C11"]`. Its binaries are the example and
  `ulei-connector-mcp-synthetic`, the synthetic stdio MCP server its tests and CI wrap.
- It depends on `ulei-contracts` (the C11 types), `serde_json` (JSON-RPC and C11 JSON) and
  `listenfd` (to take the broker fd and close it before the child starts), all workspace
  declarations already in `Cargo.lock`.
- It shares no JSON-RPC types with the runner: the runner's MCP client stays in `ulei-core`, and
  `ulei-contracts` holds no logic.
- It is not released (ADR-0056 section 6).

## Consequences

- `Cargo.lock` gains only the new package.
- The crate holds no `unsafe`; the variable `LISTEN_FDNAMES` stays in the example's own
  environment, which no child sees, since the child's environment is cleared.
