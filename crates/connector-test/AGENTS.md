# AGENTS.md — `ulei-connector-test`

The stdio test connector of C11 (ADR-0056 section 7): the conformance subject of the runner's
tests.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Speak only C11: one `Request` line on stdin, `Line`s on stdout built from the generated
  `ulei_contracts::generated::c11` types, free text on stderr. The run's last line is `done` or
  `error`. A `Request` missing a field or with `limit` 0 is `bad_request`.
- Items are synthetic and deterministic: item `n` after cursor `n - 1`, a checkpoint after each.
  The runner's and the MCP example's tests compare against these bytes.
- A key's value never reaches stdout or stderr; only its file's SHA-256 does.
- The probes run only when argv names them (`--probe-inet`, `--probe-unix PATH`, `--probe-read`,
  `--probe-write`, `--probe-create`, `--probe-truncate` with a `PATH`, `--connect HOST:PORT`,
  `--probe-limit HOST:PORT`, `--probe-direct IP:PORT`): one item each, before the numbered items,
  whose text is the call's answer (`EAFNOSUPPORT errno 97`, `connected`, `EACCES errno 13`,
  `created`, the broker's reply line with the first line read from the source, ...).
  `ci/connectors_run.py` reads them from its fake C10.
- The broker is the fd named `broker` of `LISTEN_FDNAMES`, taken once when a probe needs it,
  after `LISTEN_PID` names this process, through `listenfd` (ADR-0065), which checks it is a
  Unix stream socket and removes `LISTEN_FDS` and `LISTEN_PID`. `LISTEN_FDNAMES` stays: removing a
  variable is `unsafe` in Rust 2024, the crate holds no `unsafe`, and the connector starts no
  child. A connector that starts one clears its environment, as the MCP example does.
- Dependencies are the ones ADR-0056 names for this crate; a new one needs an ADR.
- Released as `connectors/test/` with the manifest `deploy/connectors/test/connector.json`
  (`ci/release.sh`, ADR-0056 section 6); the released `ulei-connectors list` must accept it.
- Check: `python3 -m ci.broad_check` from the repository root.
