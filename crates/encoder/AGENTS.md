# AGENTS.md — `ulei-encoder`

The encoder engine of the garden's embedders (ADR-0087): one parameterised BERT / XLM-RoBERTa
forward pass, pooling and L2 normalisation, and the weights it reads. Milestone E1: portable
fp32, weights on the heap from a `safetensors` checkpoint.

Read the root `AGENTS.md` first. Rules specific to this crate:

- It is a library crate (class `"lib"`, ADR-0072), not a module: it depends on no workspace
  crate, has no binary, reads no environment or `ulei.toml`, spawns nothing and opens no socket;
  it opens only the paths its caller hands it. Only the modules in `users` of its `Cargo.toml`
  (today `ulei-garden`) may depend on it; a new user needs an ADR. Budgets belong to the binaries
  that link it. `cargo xtask arch` enforces this.
- A model is configuration (`config::Config`, read from the checkpoint's `config.json`, plus the
  pooling) and a weight file. No code path names a model; per-model values (positions offset,
  LayerNorm eps, sizes, pooling) come from the config.
- `forward` reads weights only through slices borrowed from `weights::Checkpoint`, so a mapped or
  quantised backing replaces the owned `Vec<f32>` without touching the forward pass.
- Every numeric change is checked against the golden dumps of ADR-0087 E0: `tests/golden.rs`,
  run with `ULEI_ENCODER_GOLDEN_DIR` (the model's dump directory with `manifest.json`) and
  `ULEI_ENCODER_MODEL_DIR` (the checkpoint with `config.json` and `model.safetensors`); without
  them it skips. Acceptance: every `emb` / `layer_NN` max abs diff <= 1e-4, pooled cosine
  >= 0.99999.
- `unsafe` stays denied in E1. ADR-0087 section 5 lifts it for a `kernels` module only, from E5.
- The only dependency is `serde_json`; a new one needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
