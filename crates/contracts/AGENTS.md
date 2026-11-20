# AGENTS.md — `ulei-contracts`

Generated wire types for C1–C8, the `ulei.toml` key catalogue `c9::KEYS` (C9 generates no
types and has no round trip) and the `CONTRACTS` table.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Everything in `src/generated/` and `tests/fixtures.rs` is generated: run `cargo xtask gen-contracts`.
- Hand-written code here stays under 1 500 lines and contains no business logic (thin wire helpers only).
- This crate depends on no workspace crate.
- Change a contract in `contracts/` (version, CHANGELOG, fixtures), then regenerate.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
