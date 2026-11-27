# AGENTS.md — `ulei-garden-embed`

The garden's embedder helper `ulei-garden-embed`: the process `ulei-garden serve` starts as its
child and speaks `ulei_garden::embed::pipe` to, and the one-shot file commands `convert` and
`verify` of the encoder engine's weight file. `src/main.rs` is its command line; what it does is
the garden's (`crates/garden/AGENTS.md`, ADR-0047 sections 1 and 2, ADR-0087).

Read the root `AGENTS.md` and `crates/garden/AGENTS.md` first. Rules specific to this package:

- It is a binary package (class `"bin"`, ADR-0104) of the module `ulei-garden`: no library, no
  build script, no contract of its own. It depends on `ulei-garden`, on `ulei-infer` (feature
  `bert`), whose `users` name it, and on `serde_json` and `sha2`; a new dependency needs an ADR.
- Its reason to exist is the release profile: it builds at opt-level 3
  (`[profile.release.package.ulei-garden-embed]` in the workspace `Cargo.toml`), so that under
  fat LTO the engine's loops are vectorised in the helper, while `ulei-garden` builds at `"s"`
  (ADR-0087 section 5.2). Code of the helper's backends and pipe stays in `ulei-garden`'s
  `embed` modules; only the command line lives here.
- The features `native` and `onnx` forward to the garden's. The release builds `native` alone,
  `cargo build --release --locked -p ulei-garden-embed --features native --target
  x86_64-unknown-linux-musl`, a static-pie binary held to the `ulei-garden-embed` sections of
  `ci/budgets.sh` and shipped by `ci/release.sh`; the glibc build with `onnx` is built only in CI
  and by hand, never released.
- The garden's integration tests run this binary from beside `ulei-garden` in the target
  directory; this package's own test `tests/embed_backends.rs` is what makes a workspace test run
  build it there.
- Check: `python3 -m ci.broad_check` from the repository root.
