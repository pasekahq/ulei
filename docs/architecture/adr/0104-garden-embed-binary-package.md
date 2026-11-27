# ADR-0104: `ulei-garden-embed` in a binary package of its own, released as the static musl `native` helper

Status: accepted
Date: 2026-11-27
Affects: crates/garden-embed, crates/garden, crates/infer, xtask, dep:ulei-garden, dep:ulei-infer, dep:serde_json, dep:sha2

Amends ADR-0001 (section 3: a fourth crate class `"bin"`; section 8: the release profile; the
budget table: a row for the helper), ADR-0047 sections 1 and 7 (where the helper is built and
which build is released), ADR-0075 section 1 (the released helper) and ADR-0087 (section 6.1 and
open question 19: the daemon's 3 MiB exception is withdrawn).

## Context

- The garden's embedder helper `ulei-garden-embed` was a binary of the package `ulei-garden`.
  The release shipped its glibc build with `onnx`, which needs ONNX Runtime on the host
  (ADR-0075 section 1). Its static musl build with the encoder engine (`native`, ADR-0087 E6)
  was built and budgeted in CI but not released.
- Cargo sets the opt-level per package, and under fat LTO the engine's loops are vectorised only
  at the opt-level of the package that links the binary (ADR-0087 section 5.2). The package
  `ulei-garden` was built at opt-level 3 for the helper's sake, so its daemon grew past
  ADR-0001's 2.5 MiB row and was held to 3 MiB until the helper had a package of its own
  (ADR-0087 section 6.1, open question 19).
- ADR-0001 lets a module crate depend on no other module, and ADR-0072 gives library crates no
  binary. The helper is neither: it is the garden's own process, speaking the garden's internal
  pipe `embed::pipe` (ADR-0047 section 2), which has no contract.

## Decision

### 1. A crate class for a module's binaries

A binary package declares `[package.metadata.ulei] class = "bin"`, `module = "<crate>"` naming
one module crate (class 2), a `role` and `contracts = []`. It lives in `crates/<dir>` as
`ulei-<dir>`. It has binary and test targets only: no library and no build script, so no crate
depends on it. It may depend on its module, on `ulei-contracts` and on the library crates whose
`users` name it, and on no other workspace crate. Its binaries are the module's: they talk to
other modules only as the module does. `cargo xtask arch` enforces these rules and
`cargo xtask module-map` lists the class as `bin (module: ...)`.

Rejected: the helper as a class-2 module of its own, which would need its own copy of the
garden's pipe and backends or a contract for an internal pipe; the helper's code in a library
crate, which ADR-0072 keeps free of processes and files.

### 2. The package `ulei-garden-embed`

`crates/garden-embed` holds the helper's `main.rs` as it was, with the features `native` and
`onnx` forwarding to the garden's. Its dependencies are `ulei-garden`, `ulei-infer` (feature
`bert`; its `users` name the package), `serde_json` and `sha2`, which the binary already used.
The release profile builds the package at opt-level 3
(`[profile.release.package.ulei-garden-embed]`); the package `ulei-garden`, its daemon and
`garden-eval` build at the profile's `"s"`, and `ulei-infer` stays at 3 (ADR-0087 section 5.2).

The garden's integration tests that run the helper find it beside `ulei-garden` in the target
directory. A test run of the workspace builds it there, as the package has an integration test
of its own; a run of the garden's tests alone builds it first with `cargo build -p
ulei-garden-embed` and the same profile, target and features, and the mutation runs test both
packages.

### 3. Release and install

- `ci/release.sh` builds `-p ulei-garden-embed --features native` for
  `x86_64-unknown-linux-musl` among the static binaries: static-pie, listed in `SHA256SUMS`,
  checked static and compared by the rebuild of ADR-0042 section 10.
- The glibc build with `onnx` leaves the release. It stays a build of the package for the
  `garden-onnx` job, the budgets job and `docs/runbooks/garden-gold.md`. Shipping both would put
  two helpers behind the one `garden.embedder.helper` key and a dynamic binary back into a
  release of static ones (ADR-0001 section 8).
- `deploy/install.sh` installs it as before, to `$HOME/.local/bin/ulei-garden-embed`, the path
  `garden.embedder.helper` names, through the same check and rename as every binary.
  `deploy/install.sh --verify RELEASE_DIR` runs only that check and writes nothing;
  `ci/release.sh` runs it on its output, so a release that lacks a binary the installer expects,
  or does not list it in `SHA256SUMS`, fails. `ci/install_run.py` shows the check refusing a
  release with any one of those binaries removed.
- On a deployment the native helper needs the engine's weight file `model.ulei-infer`, which
  `ulei-garden-embed convert` writes from an fp32 checkpoint and `verify` checks, in the
  directory `garden.embedder.model_dir` names, alone (a directory with an ONNX model too is
  refused). `garden.embedder.ort_library` stays required by C9 and is ignored by `native`, as is
  `threads` (ADR-0087 open question 13). The model identity changes, so the worker embeds every
  fact again. `docs/runbooks/garden-service.md` step 5 is the host procedure.

### 4. Budgets

| Binary | Static size | Other rows |
|---|---|---|
| `ulei-garden` | ≤ 2,621,440 B, ADR-0001's daemon row (the 3 MiB exception of ADR-0087 section 6.1 is withdrawn) | ADR-0047 section 7, unchanged |
| `ulei-garden-embed`, static musl with `native` (released) | ≤ 2,097,152 B (`ULEI_GARDEN_EMBED_MAX_BINARY_BYTES`), static | memory per model: ADR-0087 section 6 |
| `ulei-garden-embed`, glibc with `onnx` (not released) | ≤ 2,097,152 B, `ldd` lists only glibc's own libraries | ADR-0047 section 7 |

## Consequences

- The release has eleven binaries, all static; none needs ONNX Runtime on the host.
- A deployment that ran the ONNX helper converts a weight file and points `model_dir` at it
  before it installs this release with the embedder enabled.
- A later binary that must build at another opt-level than its module's daemon gets a binary
  package of its module the same way.
