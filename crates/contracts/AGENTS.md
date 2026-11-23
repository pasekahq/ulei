# AGENTS.md — `ulei-contracts`

Generated wire types for C1 to C8, C10 and C11, the `ulei.toml` key catalogue `c9::KEYS` (C9
generates no types and has no round trip) and the `CONTRACTS` table.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Everything in `src/generated/` and `tests/fixtures.rs` is generated: run `cargo xtask gen-contracts`.
- Hand-written code here stays under 1,500 lines and is thin wire helpers plus the two decoders of
  C9, `config` (finds `ulei.toml` from the flag, `ULEI_CONFIG`, `XDG_CONFIG_HOME` and `HOME`, reads
  it, checks it against `c9::KEYS`) and `env_file` (reads an env-file the config names). These two
  are the only code in the crate that reads a file or the environment; nothing else in it holds
  logic.
- This crate depends on no workspace crate.
- Change a contract in `contracts/` (version, CHANGELOG, fixtures), then regenerate.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
