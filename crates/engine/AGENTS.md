# AGENTS.md: `ulei-engine`

The ecosystem's own synchronous agent loop: tool loop, tools, sessions, the `ulei-engine` binary.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Every model call goes to the qb gateway over its contract and nowhere else (ADR-0009): no
  vendor adapter, vendor host, vendor credential path or vendor SDK crate. The contract is C4G,
  qb's own file vendored at the qb pin of `ecosystem.lock` (ADR-0038).
- Tests use a scripted fake gateway and never reach a vendor. The fake is
  `tests/fake_gateway/`; a test crate includes it with `mod fake_gateway;`. It
  answers in the vendored C4G form only, built from the generated types; a test of a field the
  types do not know sends raw bytes (`Answer::Raw`, `FakeGateway::answer_once`).
- A test of the garden's C7 or the archive's C10 uses the same fake as its socket, answering
  `POST /v1/<method>` through `FakeGateway::respond`.
- The one test of a real gateway is `tests/qb_contract.rs`: qb built at the pin, on fake vendors,
  ignored in the ordinary run and run by the CI job `qb-contract` (ADR-0038 section 6).
- MCP servers are reached only on loopback over the `gateway.rs` reader (ADR-0015, ADR-0026);
  tests use the fake Streamable HTTP server `tests/fake_mcp/` with `mod fake_gateway; mod fake_mcp;`.
- Synchronous: no async runtime, no background threads or timers when idle.
- `ulei-engine` is held to the daemon budgets of ADR-0010; a new dependency needs its own ADR.
- Sort through `crate::sort`, not the slice sorts or a collected `BTreeSet` or JSON `Map`:
  each such call site links its own copy of the sort (ADR-0027).
- TLS lives only in the `ulei-web` helper (ADR-0015, ADR-0021): `ureq` is used only in
  `src/bin/ulei-web.rs`, and the helper's logic in `src/web*` compiles only with the `web`
  feature. `ulei-engine` never gets the feature. The helper's tests in `tests/web.rs` and the
  end-to-end run of `serve` in `tests/e2e.rs` need `--all-features` and use the fake Exa
  `tests/fake_exa/` on loopback, never the network.
- Tests that drive `ulei-engine serve` over its C6 socket start it and call it through
  `tests/c6_wire/` (`mod fake_gateway; mod c6_wire;`).
- Settings: `src/config.rs` reads `ulei.toml` only through `ulei_contracts::config` (ADR-0040):
  the file is required and checked whole against C9, and each `ULEI_*` setting variable wins
  over its key. A test, fake or CI driver that starts `ulei-engine` writes a valid C9 document or
  points at an empty one.
- Talk to other modules only through `ulei-contracts` and the wire.
- Check: `python3 -m ci.broad_check` from the repository root.
