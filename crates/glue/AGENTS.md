# AGENTS.md — `ulei-glue`

Glue to class-1 products: qb (C4) and the A2A gate (C5).

Read the root `AGENTS.md` first. Rules specific to this crate:

- Never edit qb or the gate here. Pin bumps go through `ecosystem.lock` plus the vendored contract.
- Map product events onto ecosystem contracts (qb `on_dead` -> C1 `quota_dead`; gate preview/released -> C1).
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
