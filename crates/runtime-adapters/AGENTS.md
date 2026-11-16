# AGENTS.md — `ulei-runtime-adapters`

Adapters between the core and foreign agent runtimes (C6).

Read the root `AGENTS.md` first. Rules specific to this crate:

- The only crate allowed to know about Hermes (or any other runtime).
- Runtime-side plugins may be Python; they talk to this crate over a unix socket using C6.
- Everything Hermes patches do today must become a C6 implementation here or an upstream PR.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
