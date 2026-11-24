# AGENTS.md — `ulei-infer`

The inference stack (ADR-0087): the lower layers written once per CPU architecture, the models
on top as feature-gated modules. Today it holds the encoder engine of the garden's embedders at
milestone E1: portable fp32, weights on the heap from a `safetensors` checkpoint.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Layers, bottom up:
  - `kernels`: the matrix kernels and their dispatch. Models call only its functions
    (`kernels::linear`, `kernels::dot`); today they delegate to `kernels::portable`, later to
    the int8 GEMM kernel per instruction set picked at start.
  - `weights`: the tensor container. Today `Checkpoint` reads `safetensors` into owned
    `Vec<f32>`; later the own int8 format opened by `mmap`.
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
- `bert` reads weights only through slices borrowed from `weights::Checkpoint`, so a mapped or
  quantised backing replaces the owned `Vec<f32>` without touching the model.
- Every numeric change is checked against the golden dumps of ADR-0087 E0: `tests/golden.rs`,
  run with `--features bert` (or `--all-features`), `ULEI_INFER_GOLDEN_DIR` (the model's dump
  directory with `manifest.json`) and `ULEI_INFER_MODEL_DIR` (the checkpoint with `config.json`
  and `model.safetensors`); without them it skips. Acceptance: every `emb` / `layer_NN` max abs
  diff <= 1e-4, pooled cosine >= 0.99999.
- `unsafe` stays denied in E1. ADR-0087 section 5 lifts it, from E4 / E5, for two modules only:
  `kernels` (intrinsics, under the fuzz rule) and the read-only `mmap` of the weight file in
  `weights`. Nowhere else.
- The only dependency is `serde_json`; a new one needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
