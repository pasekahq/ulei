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
- The probes run only when argv names them (`--probe-inet`, `--probe-unix PATH`, `--probe-read`,
  `--probe-write`, `--probe-create`, `--probe-truncate` with a `PATH`, `--connect HOST:PORT`,
  `--probe-limit HOST:PORT`, `--probe-direct IP:PORT`): one item each, before the numbered items,
  whose text is the call's answer (`EAFNOSUPPORT errno 97`, `connected`, `EACCES errno 13`,
  `created`, the broker's reply line with the first line read from the source, ...).
  `ci/connectors_run.py` reads them from its fake C10.
- The broker is the fd named `broker` of `LISTEN_FDNAMES`, taken once when a probe needs it. Its
  one `unsafe` block owns that inherited fd: std and rustix have no safe way to, and
  `pidfd_getfd` is outside the unit's `@system-service` filter.
- Dependencies are the ones ADR-0056 names for this crate; a new one needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
