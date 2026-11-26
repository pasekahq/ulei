# ADR-0072: Library crates: a third crate class for pure computation that named modules may link

Status: proposed (draft, not for merge)
Date: 2026-11-24
Affects: xtask, AGENTS.md, crates/infer, crates/garden, dep:ulei-infer, dep:serde_json, dep:rustix

Amends ADR-0001 section 3 (and the class list of ADR-0001 section 2 and ADR-0002).

## Context

ADR-0001 made the crate graph the module boundary, the Rust form of the old `sys.path` ban: a
module crate depends on exactly one workspace crate, `ulei-contracts`, and reaches every other
module only over the wire. That keeps three guarantees: modules are coupled only by contracts,
budgets are per binary, and a fault in one module's process does not take down another.

The rule knew two kinds of crate: the contracts crate and modules. It did not foresee a stateless
computation library that more than one module links. The first one is `ulei-infer` (ADR-0087):
the BERT / XLM-R forward pass, to be linked by the garden's helper `ulei-garden-embed` and later by
a speech-to-text port in another module. Under ADR-0001 neither may depend on it.

## Options

| Option | What | Verdict |
|---|---|---|
| A | Keep the encoder inside `crates/garden` | The speech port would copy the kernels: the three-copies problem the monorepo was made to end. Rejected. |
| B | Put it into `ulei-contracts` | Breaks the contracts crate's rule: generated types plus at most 1,500 hand-written lines of wire helpers and the C9 decoders. Rejected. |
| C | Own repository, pinned in `ecosystem.lock` as a class-1 product | Fails the four-condition test of ADR-0002: it is a library import, not a wire interface (condition 2). Rejected. |
| D | A new crate class `"lib"` for pure computation libraries | Chosen. |

## Decision

1. **Class.** A library crate declares `[package.metadata.ulei] class = "lib"` (a string, like
   `"contracts"`; class 2 stays the integer of the modules), a `role`, `contracts = []` and
   `users = [...]`, the modules that may depend on it. It lives in `crates/<dir>` as
   `ulei-<dir>` like every crate.
2. **No workspace dependency.** A library depends on no workspace crate: not `ulei-contracts`, not
   a module, not another library. Its external dependencies follow the usual `dep:` ADR rule.
   Library to library edges need a later ADR.
3. **Who may link it.** A module may depend on `ulei-contracts` and on every library whose `users`
   names it, and on no other workspace crate. A new user is an edit of `users` and needs an ADR
   (`Affects:` both crates). The allow-list is chosen over "any module may link any library"
   because it costs a few lines in `xtask arch`, makes every edge a reviewed decision, and stops a
   library from becoming a shared state or helper dump used by every module.
4. **Pure computation.** A library holds no process, socket, wire protocol, config or environment
   reading (`ulei.toml`, C9); it knows no module names, paths or the reference agent; it opens
   only paths its caller hands it. It has no binary target and no build script; tests, examples
   and benches are fine, and its integration tests may read the environment.
5. **Budgets stay per binary.** A library has no budget row; the binaries that link it carry its
   size and RSS in theirs.
6. **Everything else of ADR-0001 section 3 holds**: no module to module dependency, `xtask` is
   never a dependency, path dependencies stay inside the workspace, and the source bans apply to
   libraries too.

`cargo xtask arch` enforces:

- a library has no workspace dependency, `contracts = []`, a `users` list naming existing module
  crates, and only `lib`, `test`, `example` and `bench` targets;
- a module's dependency on a library is red unless the library's `users` names the module;
- every file of a library's `src/` is free of `std::env`, `env::var`, `std::process`,
  `process::Command`, `std::net`, `os::unix::net`, Unix and TCP/UDP socket types and `ulei.toml`,
  on top of the bans every crate keeps (`#[path]`, `include!`, Hermes, personal paths, vendor
  hosts).

`cargo xtask module-map` shows the class as `lib (users: ...)`.

## Consequences

- `ulei-infer` becomes the first library, `users = ["ulei-garden"]`. The speech-to-text port
  adds its module to `users` in its own ADR.
- A fault in a library runs in the process of the module that links it; isolation between
  modules is unchanged because no library is shared state, only shared code.
- The source bans are lexical: they keep honest code honest and are no sandbox. Review still
  reads a library's diff for hidden I/O.
