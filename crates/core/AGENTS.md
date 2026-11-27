# AGENTS.md — `ulei-core`

Owner inbox and events (C1), module registry, config, notify fallback, MCP facade (C2), the `ulei` binary
and the source connectors' runner `ulei-connectors` (C10 client, C11).

Read the root `AGENTS.md` first. Rules specific to this crate:

- S1 lands here: the owner inbox in Rust over the same `events.sqlite3` file and the same C1 packet (HMAC-SHA256, at-least-once, `id` idempotent), first in shadow mode next to the Python copy.
- The `ulei` binary is measured by `ci/budgets.sh`; keep it small and free of startup I/O.
- Everything that opens `events.sqlite3` lives in the `ulei-inbox` binary; `ulei` links no SQLite and
  forwards `ulei inbox ...` and `ulei status ...` to its sibling (ADR-0007, ADR-0040 section 6).
  `ulei-inbox status` and `ulei-inbox reconcile` open no database: they are C1 clients of
  `ulei-inbox serve`, the one writer of the store.
- `uncertain` is final for delivery; only the owner's C1 `reconcile` (ADR-0099) leaves it, and
  `Inbox::reconcile` changes the row and stores its audit line in table `reconciled` in one
  transaction. The caller is the socket peer's credentials, never an argument.
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
  checks needing a user manager run in CI job `systemd-units` (`ci/connectors_run.py`,
  `ci/connectors_e2e.py`, `ci/units_run.py`); the crate's tests put a fake `systemd-run` on
  `PATH`, and a unit test restricts one thread with the host's Landlock.
- A run's network is its broker (ADR-0056 section 5.3, `connectors/broker.rs`): a socket in
  `connectors.run_dir` that lives only as long as the run, passed as fd 3 through `OpenFile=`,
  connecting only the manifest's `source.addresses` and the owner's `allow` and never reading a
  stream. The owner's `network = "direct"` is the one way a connector gets sockets of its own:
  `AF_INET` and `AF_INET6`, with connect ports limited by Landlock from ABI 4.
- An `mcp` connector runs through the same unit and driver as a stdio one; `connectors/mcp.rs` is
  its stdio JSON-RPC client (ADR-0056 section 2). It parses a result with a `serde_json`
  `DeserializeSeed` over a reader that cuts long strings and fails a number over 1,024 bytes,
  handing each item to the driver as it is parsed, so neither it nor the stdio path ever holds a
  page or a whole `put` body (`put` streams the body to C10). `mcp::linger` takes the wait as an
  argument, so its test waits milliseconds; CI proves the real 3 s.
- `ulei-connectors serve` (`connectors/daemon.rs`) keeps its schedule in `connectors/schedule.rs`,
  which reads no clock: the daemon passes the time in, so the schedule's tests need no waiting.
  The daemon sleeps until the nearest due time or a run's end, one run per connector across
  processes through the `flock` of `<connectors.state>/<name>/lock`, at most four at once.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
