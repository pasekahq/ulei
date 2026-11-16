# AGENTS.md — `ulei-core`

Owner inbox and events (C1), module registry, config, notify fallback, MCP facade (C2), the `ulei` binary.

Read the root `AGENTS.md` first. Rules specific to this crate:

- S1 lands here: the owner inbox in Rust over the same `events.sqlite3` file and the same C1 packet (HMAC-SHA256, at-least-once, `id` idempotent), first in shadow mode next to the Python copy.
- The `ulei` binary is measured by `ci/budgets.sh`; keep it small and free of startup I/O.
- Everything that opens `events.sqlite3` lives in the `ulei-inbox` binary; `ulei` links no SQLite and
  forwards `ulei inbox ...` to its sibling (ADR-0007).
- Config comes from a file; no personal ids or paths in code.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
