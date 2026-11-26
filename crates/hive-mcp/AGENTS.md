# AGENTS.md — `ulei-hive-mcp`

The hive's typed tools for the engine (ADR-0071): `hive_create`, `hive_list`, `hive_show`,
`hive_steer` and `hive_cancel` of C2, and the read-only snapshots `hive_inspect`, `hive_health`
and `hive_file` (ADR-0086), served over MCP Streamable HTTP on a loopback address
(`[hive_mcp]` of C9), each call run as the hive's CLI. The engine reaches it as one
`[engine.mcp.<name>]` with the bearer header in its env-file and gets no path of the hive.

Read the root `AGENTS.md` first. Rules specific to this crate:

- The tools' shapes are C2: inputs decode into the generated `ulei_contracts::generated::c2`
  types, outputs are built from them. `tools::input_schemas` must equal the C2 definitions
  (`tools::tests::served_schemas_are_the_c2_definitions`); change C2 first, then the code.
- Never add a field that names a script, an executor, a model, a panel, a delivery, a tag or a
  path, and never pass `--created-by` or `--by` from an input: ADR-0071 section 7. The timing
  fields of ADR-0082 (`not_before`, `every`, `cron`, `tz`, `until`, `max_runs`, `recheck_*`) are
  the one exception: the hive schedules, ulei has no scheduler of its own (ADR-0016), and every
  result still goes to the owner inbox. A new hive flag needs the ADR amended and a test that an
  option-like value stays a value. `hive_file`'s `path` is the one path an input names (ADR-0086
  section 3): relative, no `..`, read-only, checked here and again by the CLI. The filter
  `created_by` of `hive_list` goes as `--creator`, never as `--created-by`.
- The CLI runs as an argv vector (values as `--flag=value`, positionals after `--`), never a
  shell, with the environment cleared to `hive::KEPT_ENV` and `HIVE_CONFIG`, one run at a time,
  within `hive_mcp.timeout` and the stdout and stderr caps.
- Nothing is logged but the tool, the card id, the outcome and the time; never the token or a
  field value.
- It does not import, link or ship anything of `hive-py/` (ADR-0068 section 1.3);
  `tests/real_hive.rs` runs the real CLI on a scratch store, `tests/engine_client.rs` the real
  `ulei-engine` client. `ulei-hive-mcp-fake-hive` is the tests' stand-in for the CLI; it is not
  released.
- Dependencies are the ones ADR-0071 names: `ulei-contracts` and `serde_json`. No HTTP or MCP
  crate; a new one needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
