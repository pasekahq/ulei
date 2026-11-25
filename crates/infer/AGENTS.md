# AGENTS.md — `ulei-infer`

The inference stack (ADR-0087): the lower layers written once per CPU architecture, the models
on top as feature-gated modules. Today it holds the encoder engine of the garden's embedders at
milestone E5: the portable fp32 forward pass on a `safetensors` checkpoint (E1, on the heap) and
the int8 forward pass on the engine's own weight file (E3), mapped read-only (E4), with the
int8 kernels per instruction set (E5), and text in through the tokenizer section of that file
(E2).

Read the root `AGENTS.md` first. Rules specific to this crate:

- Layers, bottom up:
  - `kernels`: the matrix kernels and their dispatch. Models call only its functions
    (`kernels::linear`, `kernels::dot`, `kernels::quantize`, `kernels::quantize_rows`,
    `kernels::linear_i8`, `kernels::attention`, `kernels::gelu`, and the `_into` forms that
    write into the caller's buffers with the kernels' working buffers in a `kernels::Scratch`
    kept by the caller: no kernel allocates once its scratch has grown). `portable` is the
    reference; `portable::linear_i8`: int8 x int8 summed in `i32` per block of B input
    channels, each block sum times its activation scale summed in `f32`, times the output
    channel's weight scale, plus the bias. `kernels::Kernel::best()` is picked at first use by
    run-time feature detection (`kernels::select` overrides it, for measurements and tests):
    - `avx2` (`x86_64`): the integer block sums of `linear_i8`, eight output channels per
      register, with `VPMADDUBSW` (activations must be in `[-127, 127]`, as `quantize` makes
      them; it checks per call and leaves anything else to `portable`); `attention` with the
      keys transposed per head (eight keys per register, each score in `portable::dot`'s
      order); the same portable source of `quantize_rows` and `gelu` compiled with AVX2;
    - `neon` (`aarch64`, `SDOT` with `dotprod`, widening multiplies otherwise): compiled and
      fuzz-tested, **not picked by `best()`** until its tests have run on an `aarch64` host
      (ADR-0087 section 5).
    A kernel computes only integer block sums; the fp32 combination is `kernels::combine`,
    shared. **Every kernel's output is bit-identical to `portable`**: `kernels::fuzz` checks
    the sums and the `f32` outputs on random shapes and extreme values (`fuzz_long`, ignored,
    runs about 1.5 million cases at opt-level 3), and `tests/golden_int8.rs` checks the pooled
    vectors of every kernel bit for bit. A change that breaks bit-equality is a bug, not a
    tolerance question.
  - `weights`: the tensor containers. `Checkpoint` reads `safetensors` into owned `Vec<f32>`;
    `file::WeightFile::open` maps the engine's own weight file (magic `ULEIINFW`, version 3:
    the file's length and a content digest in the 64-byte preamble, JSON header, 64-byte
    aligned blobs; format in the module docs) private and read-only and hands out slices into
    the mapping, checked once at open for range and alignment and the file's length against
    the preamble's (a malformed, truncated or longer file is an `Error::Corrupt`, never a
    panic; an older version an `Error::Weights` with "convert the model again"); the content
    digest is the SHA-256 of the whole file with the digest's own bytes read as zeros
    (`file::canonical`), which the caller computes (this crate has no SHA-256) and puts in with
    `file::seal`; `WeightFile::digest` hands it out unchecked, and nothing at open reads the
    blobs to check it (ADR-0087 E6c); `WeightFile::read` copies a file that is not
    on disk into an anonymous read-only mapping (tests); `file::write` writes it. The mapping
    and the in-place view live in the private module `weights::map`, the crate's only
    `unsafe` until E5.
  - `ops`: model-agnostic fp32 operations (`erf` / GELU, `LayerNorm`, softmax, L2 normalise).
    Two GELUs: `ops::gelu` (exact, `f64` `erf`) for the fp32 reference path, `ops::gelu_f32`
    (one fixed `f32` formula, within 6.1e-7 of the exact one over every `f32`) for the int8
    path. Two softmaxes likewise: `ops::softmax` (the library's `expf`) for the fp32 reference
    path, `ops::softmax_f32` with `ops::exp_f32` (one fixed `f32` formula, relative error
    8.1e-8 over every `f32` in `[-87.33, 0]`) for the int8 path; `kernels::attention` takes
    an `ops::Softmax` saying which. `gelu_f32`, `exp_f32` and `softmax_f32` must stay plain
    `f32` operations without library calls, in a fixed order, so every build (and every C
    library) gives the same bits.
  - `text` (feature `text`; `bert` enables it): text to token ids, model-independent. The
    `SentencePiece` Unigram tokenizer of the XLM-RoBERTa family, reproducing the `tokenizers`
    library as the checkpoint's `transformers` class runs it (`Pipeline::WhitespaceSplit`, the
    default) or as the checkpoint's `tokenizer.json` describes it (`Pipeline::CollapseSpaces`),
    truncation included. Its data is a section of the weight file (`tokenizer.*` tensors,
    `meta.tokenizer`) that `text::convert` builds from `tokenizer.json` and
    `text::Tokenizer::from_file` reads in place: nothing is built at load. The grapheme
    table `text/grapheme_table.rs` is generated (see its header); do not edit it by hand.
    **The ids must equal the reference's exactly**: a change to `text` is checked with
    `tests/tokenizer_parity.rs` on the reference's ids (below), not by eye.
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
  checkpoint (pure computation; the CLI is `ulei-garden-embed convert`, ADR-0087 section 2): the
  int8 rules of ADR-0087 sections 2 and 3 (symmetric, `max |x| / 127`, round half away from
  zero, clamp to +-127), the token table in the caller's row order with its id -> row map, the
  activation block size B in the header (`act_block`, default 32) with the config, pooling, the
  query and passage prefixes (`prefixes`, ADR-0087 E6) and the SHA-256 of the checkpoint and of
  the row order. `LayerNorm`, softmax, GELU and the
  attention products stay fp32.
- The forward pass allocates its activations in a `bert::Workspace` the caller keeps across
  inputs (the buffer pool of ADR-0087 section 3): `Encoder::embed_with` / `embed_text_with`.
  Its buffers grow to the longest input and are never shrunk (`Workspace::release` frees them),
  every value is written before it is read, so a reused workspace gives the bits of a new one
  and no layer allocates at steady state. `embed` / `embed_text` / `forward` take a new one.
- `bert::Encoder::from_file` binds the tokenizer and the prefixes with the weights;
  `Encoder::embed_text` takes the text as the model gets it, prefix included: the caller puts
  `Encoder::prefixes` of its input's kind before it (e5's `query: ` / `passage: `). A file of an
  older format (`ulei-infer/bert-int8/1` without the tokenizer section, `/2` without the
  prefixes, `/3` without the content digest) is refused with "convert the model again"; a weight file of another model family
  with its own reason.
- Every numeric change is checked against the golden dumps of ADR-0087 E0: `tests/golden.rs`,
  run with `--features bert` (or `--all-features`), `ULEI_INFER_GOLDEN_DIR` (the model's dump
  directory with `manifest.json`) and `ULEI_INFER_MODEL_DIR` (the checkpoint with `config.json`
  and `model.safetensors`); without them it skips. Acceptance: every `emb` / `layer_NN` max abs
  diff <= 1e-4, pooled cosine >= 0.99999. The int8 path: `tests/golden_int8.rs` with
  `ULEI_INFER_INT8_FILE` (the converted file) and `ULEI_INFER_GOLDEN_DIR`; acceptance pooled
  cosine >= 0.999 against PyTorch fp32 on every case, and the tokenizer's ids from each case's
  `prefixed_text` equal to its `input_ids` with `embed_text` bit-identical to the ids path.
  Optional: `ULEI_INFER_SIM_DIR` and `ULEI_INFER_SIM_LINEAR_DIR` (dumps of the Python
  simulation of the scheme: end to end, per layer on the simulation's input, per linear layer
  on its input, the last within 1e-5 of `1 + |y|`), `ULEI_INFER_MODEL_DIR` (fp32 timing). Run them at opt-level 3; the debug
  profile is 30 to 60 times slower.
- `unsafe` is denied (workspace lint `unsafe_code = "deny"`). ADR-0087 section 5 lifts it for
  these modules only, each by one `#[allow(unsafe_code)]` on its `mod` line: `weights::map` (the
  read-only `mmap` of the weight file and the in-place view of its bytes, since E4) and
  the instruction-set modules `kernels::avx2` and `kernels::neon` (intrinsics and `SDOT` as
  inline assembly, under the fuzz rule, since E5). Nowhere else. A `#[target_feature]`
  function is called only through a token type that exists once its feature is detected.
  Every `unsafe` block or impl carries a `// SAFETY:` comment, and each module exposes only
  safe functions. The one unchecked precondition (of `weights::map`): a mapped file is not
  written or truncated while mapped (the converter renames a new file into place).
- The dependencies are `serde_json` and `rustix` (feature `mm`, for `mmap`; raw syscalls, no
  libc, ADR-0087 `Affects: dep:rustix`); a new one needs an ADR.
- `tests/tokenizer_parity.rs` (ignored): `ULEI_INFER_TOKENIZER_FILE` (a weight file) and
  `ULEI_INFER_TOKENIZER_CASES` (JSON lines `{"text", "ids"}` of the reference); its second test
  runs random text through the real tokenizer (`ULEI_INFER_FUZZ_ROUNDS`).
- `examples/tokenize_probe.rs` (feature `bert`) is the E2 probe: ids and encode time per text
  line, from a weight file or a `tokenizer.json` alone, and the text-in vectors.
- `examples/mem_probe.rs` (feature `bert`) is the E4 memory and start-up probe: it reads
  `/proc/self/*`, which is fine in an example and banned in `src/`. `examples/speed_probe.rs`
  is the E5 speed probe: per-input latency per kernel. Speed figures name their build
  (profile, opt-level, target, `-C` flags): the size-first release profile swings the
  portable code's speed with code layout (ADR-0087 E4).
- Check: `python3 -m ci.broad_check` from the repository root.
