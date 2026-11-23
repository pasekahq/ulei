# AGENTS.md — `ulei-core`

Owner inbox and events (C1), module registry, config, notify fallback, MCP facade (C2), the `ulei` binary
and the source connectors' runner `ulei-connectors` (C10 client, C11).

Read the root `AGENTS.md` first. Rules specific to this crate:

- S1 lands here: the owner inbox in Rust over the same `events.sqlite3` file and the same C1 packet (HMAC-SHA256, at-least-once, `id` idempotent), first in shadow mode next to the Python copy.
- The `ulei` binary is measured by `ci/budgets.sh`; keep it small and free of startup I/O.
- Everything that opens `events.sqlite3` lives in the `ulei-inbox` binary; `ulei` links no SQLite and
  forwards `ulei inbox ...` and `ulei status ...` to its sibling (ADR-0007, ADR-0040 section 6).
  `ulei-inbox status` opens no database.
- Config comes from a file; no personal ids or paths in code. `ulei-inbox serve` and `status` read
  `ulei.toml` only through `ulei_contracts::config`; this crate holds no TOML reader.
- `ulei-connectors` (module `connectors`, ADR-0056) reads `ulei.toml` only through
  `ulei_contracts::config`, runs only the names of `connectors.enabled` whose `connector.json`
  is a valid C11 manifest, never runs git, and is the only party that puts connector items into
  C10. It never reads a key file; a run record holds no item content and no key value.
- A connector runs only through `systemd-run` with the properties of ADR-0056 section 5.1 and the
  launcher `ulei-connectors exec`, which refuses before executing anything unless it finds its
  seccomp filter and its own run unit, then restricts itself with the Landlock rules of section
  5.2 (ABI 3 or later, hard requirement) unless the owner set `landlock = "off"`. The runner reads
  the unit's start and `Result=` from `systemd-run`'s stderr lines, never from elapsed time. The
  checks needing a user manager run in CI job `systemd-units` (`ci/connectors_run.py`); the
  crate's tests put a fake `systemd-run` on `PATH`, and a unit test restricts one thread with the
  host's Landlock.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
