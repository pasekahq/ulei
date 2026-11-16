# AGENTS.md — `ulei-hive`

Cards, heads, acceptance and delivery of results (C3).

Read the root `AGENTS.md` first. Rules specific to this crate:

- A head is a vendor CLI run in a workspace; the contract is C3 (card -> head -> REPORT.md -> acceptance).
- Results reach the owner as C1 events, never by calling core code.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
