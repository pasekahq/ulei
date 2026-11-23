# AGENTS.md — `ulei-connector-mcp-example`

The MCP wrapper example of C11 (ADR-0056 section 7, ADR-0067): an MCP server over stdio serving
`ulei_pull` that wraps a ready MCP server's listing tool. It is the pattern a connector writer
copies to wrap a server, so it stays small and readable.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Speak only C11 to the runner: `ulei_pull` results whose `structuredContent` is a `Page` built
  from the generated `ulei_contracts::generated::c11` types, or an `Error` with `isError`. Its
  argv, cursor and error codes are in the docstring of `src/main.rs` and ADR-0056 section 7.
- A page holds at most the request's `limit` items; the runner ends a run with more `protocol`.
- The wrapped server is a child with a cleared environment (`PATH`, `HOME`, `TMPDIR`, `LANG`
  only), and the broker fd is closed before it starts.
- `ulei-connector-mcp-synthetic` is the synthetic ready server of the tests and of
  `ci/connectors_run.py`; its `--count N` records are the test connector's items, so the runner's
  tests compare the two connectors' `put` bodies byte for byte.
- Dependencies are the ones ADR-0067 names; no MCP SDK, and a new one needs an ADR. Not released.
- Check: `python3 -m ci.broad_check` from the repository root.
