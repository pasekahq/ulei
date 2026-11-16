# AGENTS.md — `ulei-memory`

Garden (facts with validity), retrieval and the forest (cold session archive) as one module (C7).

Read the root `AGENTS.md` first. Rules specific to this crate:

- The forest reads raw sessions through C6 `raw_sessions`, never the runtime's database directly.
- A garden miss is a C1 `garden_miss` event.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
