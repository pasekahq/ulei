# AGENTS.md — `ulei-memory`

The forest: the cold archive of raw sessions. The garden and its retrieval live in
`crates/garden` (ADR-0043).

Read the root `AGENTS.md` first. Rules specific to this crate:

- The forest reads raw sessions through C6 `raw_sessions`, never the runtime's database directly.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
