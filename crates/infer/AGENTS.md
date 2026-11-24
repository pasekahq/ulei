# AGENTS.md — `ulei-infer`

The inference stack (ADR-0086): the lower layers written once per CPU architecture, the models
on top as feature-gated modules. Today it holds the encoder engine of the garden's embedders at
milestone E3: the portable fp32 forward pass on a `safetensors` checkpoint (E1) and the portable
int8 forward pass on the engine's own weight file (E3), both on the heap.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Layers, bottom up:
  - `kernels`: the matrix kernels and their dispatch. Models call only its functions
    (`kernels::linear`, `kernels::dot`, `kernels::quantize`, `kernels::quantize_rows`,
    `kernels::linear_i8`); today they delegate to `kernels::portable`, later to the int8 GEMM
    kernel per instruction set picked at start. `portable::linear_i8` is the reference: int8 x
    int8 summed in `i32` per block of B input channels, each block sum times its activation
    scale summed in `f32`, times the output channel's weight scale, plus the bias.
  - `weights`: the tensor containers. `Checkpoint` reads `safetensors` into owned `Vec<f32>`;
    `file::WeightFile` reads the engine's own weight file (magic `ULEIINFW`, JSON header,
    64-byte aligned blobs; format in the module docs) into owned buffers, `file::write`
    writes it. E4 replaces the owned buffers by a read-only `mmap` behind the same accessors.
  - `ops`: model-agnostic fp32 operations (`erf` / GELU, `LayerNorm`, softmax, L2 normalise).
  - Models, each behind its own cargo feature and in its own module: `bert` (feature `bert`,
    the BERT / XLM-RoBERTa encoder family). A speech model (`conformer`) is a later module of
    the same kind, by its own ADR.
- **A new model is a feature-gated module; a new kernel goes into `kernels` only.** A model
  never calls a kernel implementation directly and never touches the backing of `weights`, so
  a kernel or a weight format is swapped without touching a model.
- No default features: a user enables the models it runs. CI builds, lints and tests with
  `--all-features`; the golden test needs `bert` (`required-features`).
- It is a library crate (class `"lib"`, ADR-0072), not a module: it depends on no workspace
  crate, has no binary, reads no environment or `ulei.toml`, spawns nothing and opens no socket;
  it opens only the paths its caller hands it. Only the modules in `users` of its `Cargo.toml`
  (today `ulei-garden`) may depend on it; a new user needs an ADR. Budgets belong to the binaries
  that link it. `cargo xtask arch` enforces this. Everything of the inference stack lives in
  this one crate: library to library dependencies stay forbidden.
- A model is configuration and a weight file. In `bert`, no code path names a model; per-model
  values (positions offset, LayerNorm eps, sizes, pooling) come from `bert::Config`, read from
  the checkpoint's `config.json` plus the pooling.
- `bert` reads weights only through slices borrowed from `weights::Checkpoint` or
  `weights::file::WeightFile` (`bert::params::Tensors`), so a mapped backing replaces the owned
  buffers without touching the model. `bert::convert` writes the int8 weight file from a
  checkpoint (pure computation; the CLI is `ulei-garden-embed convert`, ADR-0086 section 2): the
  int8 rules of ADR-0086 sections 2 and 3 (symmetric, `max |x| / 127`, round half away from
  zero, clamp to +-127), the token table in the caller's row order with its id -> row map, the
  activation block size B in the header (`act_block`, default 64) with the config, pooling and
  the SHA-256 of the checkpoint and of the row order. `LayerNorm`, softmax, GELU and the
  attention products stay fp32.
- Every numeric change is checked against the golden dumps of ADR-0086 E0: `tests/golden.rs`,
  run with `--features bert` (or `--all-features`), `ULEI_INFER_GOLDEN_DIR` (the model's dump
  directory with `manifest.json`) and `ULEI_INFER_MODEL_DIR` (the checkpoint with `config.json`
  and `model.safetensors`); without them it skips. Acceptance: every `emb` / `layer_NN` max abs
  diff <= 1e-4, pooled cosine >= 0.99999. The int8 path: `tests/golden_int8.rs` with
  `ULEI_INFER_INT8_FILE` (the converted file) and `ULEI_INFER_GOLDEN_DIR`; acceptance pooled
  cosine >= 0.999 against PyTorch fp32 on every case. Optional: `ULEI_INFER_SIM_DIR` and
  `ULEI_INFER_SIM_LINEAR_DIR` (dumps of the Python simulation of the scheme: end to end, per
  layer on the simulation's input, per linear layer on its input, the last within 1e-5 of
  `1 + |y|`), `ULEI_INFER_MODEL_DIR` (fp32 timing). Run them at opt-level 3; the debug
  profile is 30 to 60 times slower.
- `unsafe` stays denied through E3. ADR-0086 section 5 lifts it, from E4 / E5, for two modules only:
  `kernels` (intrinsics, under the fuzz rule) and the read-only `mmap` of the weight file in
  `weights`. Nowhere else.
- The only dependency is `serde_json`; a new one needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
