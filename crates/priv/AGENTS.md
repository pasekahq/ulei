# AGENTS.md — `ulei-priv`

The privileged helper (ADR-0073): `ulei-priv serve` runs as root from the system unit
`deploy/systemd/system/ulei-priv.service` and executes only the commands of the policy
`/etc/ulei/priv.toml` for the uids it lists; `ulei-priv list` and `ulei-priv run <id> [<arg>]` are
its client, which the engine's `terminal` tool runs in place of `sudo`.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Nothing from the client reaches the child but one `{arg}` value equal to an entry of the
  command's `arg` list in the policy. No shell, no glob, no client environment: the child's
  environment is exactly `child::ENV`, cwd `/`, stdin `/dev/null`, its own process group.
- The policy file and its directory must be owned by the daemon's own euid (root in the unit) and
  not writable by group or others; `policy::check_owner` takes the owner as an argument so the
  tests run as a normal user. Every `argv[0]` is absolute; a `<PLACEHOLDER>` left is refused.
- The wire is module-internal (ADR-0073 section 7), documented in `src/wire.rs`: one JSON request
  line of at most 4 KiB per connection, a JSON header line back, then for `run` the raw stdout
  and stderr bytes. No other module may speak it; if one must, it becomes a contract first.
- Every call, allowed or refused, writes one audit line (`src/audit.rs`) to the audit file and to
  stderr before the reply. No audit file, no command: `audit_failed`.
- One command at a time (`busy`), at most `serve::MAX_CONNECTIONS` connections handled at once.
  The main thread blocks in `accept`: 0 wake-ups idle, checked by `ci/budgets.sh`.
- Client exit codes: the child's (128 + signal when killed), 77 refused, 69 daemon unreachable,
  2 usage. The help text is what the model sees: keep it short.
- Dependencies are `serde_json` and `rustix` (ADR-0073); a new one needs an ADR.
- Released static in `ci/release.sh`; `deploy/install.sh` checks its sum but never installs it.
  The owner installs it as root to `/usr/local/lib/ulei/ulei-priv` (runbook
  `docs/runbooks/mvp-pilot-cutover.md` part 5.2), never into the ulei account's home.
- Check: `python3 -m ci.broad_check` from the repository root.
