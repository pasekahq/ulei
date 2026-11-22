# AGENTS.md — `ulei-connector-test`

The stdio test connector of C11 (ADR-0056 section 7): the conformance subject of the runner's
tests.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Speak only C11: one `Request` line on stdin, `Line`s on stdout built from the generated
  `ulei_contracts::generated::c11` types, free text on stderr. The run's last line is `done` or
  `error`.
- Items are synthetic and deterministic: item `n` after cursor `n - 1`, a checkpoint after each.
  The runner's and the MCP example's tests compare against these bytes.
- A key's value never reaches stdout or stderr; only its file's SHA-256 does.
- Dependencies are the ones ADR-0056 names for this crate; a new one needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
