# AGENTS.md — `ulei-memory`

Memory below the garden: the archive of raw items (immutable, compressed, FTS5), served as C10
by `ulei-memory serve`, and later the forest made from it (ADR-0051 section 1). The garden and
its seedbed live in `crates/garden` (ADR-0043, ADR-0055). The crate holds the archive store,
`src/archive.rs`, the daemon's transport, `src/serve.rs`, and the binary `ulei-memory` with
`serve` and `delete`, `src/main.rs`; [`README.md`](README.md) describes the file, the operations,
the daemon and the command.

Read the root `AGENTS.md` first. Rules specific to this crate:

- The archive is raw material as it came: a stored item's bytes never change, and only C10
  `delete`, the owner's command, removes an item, writing an audit row that is never removed.
  The schema's triggers refuse every other update or delete; keep them.
- `src/archive/query.rs` ports the reference's FTS5 query sanitizer
  (`THIRD_PARTY_NOTICES.md#ref-agent`), the same port as the engine's history search.
- The archive reads the engine's dialogue only through C6 `raw_sessions`, never the journal's
  database (ADR-0051 section 3).
- The archive parses no source format: the putter gives the text it indexes.
- The socket never serves `delete`; only the owner's command `ulei-memory delete` removes an item.
- `src/serve.rs` follows the garden's daemon (`crates/garden/src/serve.rs`, ADR-0047 section 2)
  in its bind, peer check, deadlines and HTTP framing; a fix in one is checked against the other.
- The daemon is a static musl binary, built by `cargo build --release --locked -p ulei-memory
  --bin ulei-memory --target x86_64-unknown-linux-musl` and held to the `ulei-memory` section of
  `ci/budgets.sh` (ADR-0051 section 7), which `ci/memory_run.py` drives.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
