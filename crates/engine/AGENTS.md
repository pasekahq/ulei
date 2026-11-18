# AGENTS.md: `ulei-engine`

The ecosystem's own synchronous agent loop: tool loop, tools, sessions, the `ulei-engine` binary.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Every model call goes to the qb gateway over its contract and nowhere else (ADR-0009): no
  vendor adapter, vendor host, vendor credential path or vendor SDK crate.
- Tests and CI use only a scripted fake gateway; they never reach a real gateway or a vendor.
  The fake is `tests/fake_gateway/`; a test crate includes it with `mod fake_gateway;`.
- MCP servers are reached only on loopback over the `gateway.rs` reader (ADR-0015, ADR-0026);
  tests use the fake Streamable HTTP server `tests/fake_mcp/` with `mod fake_gateway; mod fake_mcp;`.
- Synchronous: no async runtime, no background threads or timers when idle.
- `ulei-engine` is held to the daemon budgets of ADR-0010; a new dependency needs its own ADR.
- TLS lives only in the `ulei-web` helper (ADR-0015, ADR-0021): `ureq` is used only in
  `src/bin/ulei-web.rs`, and the helper's logic in `src/web*` compiles only with the `web`
  feature. `ulei-engine` never gets the feature. The helper's tests in `tests/web.rs` need
  `--all-features` and use a local fake Exa on loopback, never the network.
- Talk to other modules only through `ulei-contracts` and the wire.
- Check: `python3 -m ci.broad_check` from the repository root.
