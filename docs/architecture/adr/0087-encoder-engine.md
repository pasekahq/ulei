# ADR-0087: Encoder engine for embedders: one parameterised BERT / XLM-R encoder in Rust, int8 weights by mmap, SIMD kernels per instruction set, in place of the ONNX Runtime helper

Status: proposed (draft, not for merge)
Date: 2026-11-24
Affects: crates/garden, crates/infer, dep:serde_json, dep:ulei-infer, dep:rustix, contract:C7, contract:C9
Amends: ADR-0047 section 2 (section 8 below: a corrupted weight file starts the daemon lexical-only; C9 0.11.0. Section 9: the helper's query and indexing lanes on its pipe).

## Context

- **The helper today.** The garden's dense side runs in the helper `ulei-garden-embed` (ADR-0047
  section 1, A3). It is a dynamic glibc binary that `dlopen`s ONNX Runtime, the one exception to
  the static musl rule of ADR-0001 section 8. Cost:
  - 25.3 s for a fresh process to load and warm bge-m3;
  - on the pilot host's class (ADR-0086 section 9.2), bge-m3 int8 peaks at 891 MB RSS and
    e5-small int8 at 295 MB. Both are anonymous memory, held while the process lives.
- **What the measurements decided.** ADR-0086 section 9.2 chose the models: e5-small for
  `standard` by default, bge-m3 for `max`, `mac` and `standard` on request. Section 9.4 states
  what a dedicated engine buys:
  - RAM: clean, evictable, shared weight pages; the 250k-row token table mostly never paged in.
    That holds only for template text. Measured 2026-11-24 on the pilot host: 15.9 MB of ru+en
    Wikipedia text (MIRACL) touches 41 549 ids and, in checkpoint row order, pages in 69.9 of
    91.6 MiB of the e5-small table (76 %) and 115 of 244 MiB at bge-m3's row size. Section 2
    reorders the rows; ADR-0086 section 9.4 carries the same claim and is corrected there by a
    follow-up;
  - start-up: a mapping instead of a load;
  - deployment: a static binary.
  It does not buy indexing speed: ONNX Runtime's kernels are at least as fast as a hand-written
  engine.
- **The reference method.** A pure-Go GigaAM CTC engine (gitlab.com/asvedr/earworm, ROADMAP M0
  to M8) went naive → verified → fast:
  - golden tests against the PyTorch and ONNX reference, layer by layer;
  - int8 per-channel weights in an mmap'd own format;
  - one int8 GEMM kernel per instruction set (NEON SDOT, AVX2), dispatched at run time;
  - a fuzz test proving each kernel bit-equal to the portable one.
  Its result: a 3.4 MB binary, 256 MB peak RSS of which 22 MB dirty, WER equal to the
  original. On the pilot host (Skylake, AVX2 path) it runs at RTF 0.86 on one
  thread with the same WER (7.86 %), against 0.34 on Apple M3 Pro (SDOT path).
- **One architecture family.** The three measured embedders are one family: BERT / XLM-RoBERTa
  encoders with one 250 002-token SentencePiece Unigram vocabulary. The garden already
  tokenises it in Rust (`embed/tokenizer.rs`, golden-tested): the pipeline the checkpoint's
  `tokenizer.json` describes (precompiled character map through `spm_precompiled`, runs of
  spaces collapsed, Metaspace, own Viterbi), its input cut to a byte prefix and long words to
  1024-byte windows, literal special tokens not split out; the ONNX helper feeds it raw text, no
  prefixes (bge-m3). It is not the pipeline the golden dumps used (E2 result, section 4).

| Model | Layers × width, heads | FFN | LayerNorm eps | Positions | Pooling | Input prefixes | int8 ONNX file |
|---|---|---|---|---|---|---|---|
| multilingual-e5-small | 12 × 384, 12 | 1536 | 1e-12 | from 0, 512 (BERT) | mean over mask | `query: ` / `passage: ` | 118 MB |
| multilingual-e5-base | 12 × 768, 12 | 3072 | 1e-5 | from 2, 514 | mean over mask | as e5-small | 279 MB |
| bge-m3 | 24 × 1024, 16 | 4096 | 1e-5 | from 2, 8194 (used to 512) | CLS | none | 570 MB |

The per-model values are read from each checkpoint's `config.json` at conversion and verified
against the golden dumps. The table is what the conversion is expected to find.

## Decision

### 1. One engine, models as configuration

- A new library crate `crates/infer` (`ulei-infer`) is the one library of the inference stack:
  the lower layers written once per CPU architecture, the models on top as feature-gated
  modules. Library to library dependencies stay forbidden (ADR-0072), so all of it is this crate:
  - `kernels`: the matrix kernels and their run-time dispatch (section 5), the only kernel code;
  - `weights`: the tensor container and the weight format (section 2);
  - `ops`: model-agnostic fp32 operations (LayerNorm, softmax, GELU, L2 normalisation);
  - `text` (cargo feature `text`, enabled by `bert`; from E2): text to token ids, model
    independent: the SentencePiece Unigram tokenizer of the XLM-RoBERTa family, an own
    implementation that reproduces the reference's ids (E2 result, section 4), its data read in
    place from the model's weight file (section 2). It replaces
    `crates/garden/src/embed/tokenizer.rs` at E6; that file and `spm_precompiled` stay for the
    ONNX helper until E7;
  - `bert` (cargo feature `bert`): this decision's encoder: embeddings, self-attention, FFN with
    GELU, post-LayerNorm, pooling (CLS or masked mean), and text in through `text`.
- A new model is a new feature-gated module on the same layers; a new kernel goes into
  `kernels` only. The speech port (ADR-0086 open question 3) becomes a second model module
  (`conformer`) and adds its module crate to `users` by its own ADR.
- Its crate class is the library class of ADR-0072: no workspace dependency, linked only by the
  modules in its `users` (today `ulei-garden`).
- A model is a config (the columns above) plus a weight file. No model-specific code paths
  beyond the config.
- Inference only, at most 512 tokens per input as today (ADR-0047).
- **Target platforms (owner's statement, 2026-11-26).** Linux on x86_64 is the platform of the
  measurements and the release. macOS on Apple Silicon is a target platform as well, as a small
  always-on server box (Mac mini or Studio class), not a laptop, but not first priority: the
  work for it is **deferred until after the release** (section 5.4). Linux on `aarch64` is not
  verified. A CPU without AVX2 (x86_64) or without the `aarch64` kernel runs the portable
  kernel, 8 to 12× slower (E10r, section 4): a limitation, not a supported way to index.
- Cross-encoder rerankers of the same family (XLM-R) are a later use of the same engine, not
  part of this decision.

### 2. Weight format and conversion

- One file per model: a header (config, tensor table, SHA-256 of the source checkpoint) and
  64-byte aligned blobs, opened by `mmap` read-only.
- Linear weights (Q, K, V, O, FFN in and out) are int8 with a per-output-channel fp32 scale.
- The token embedding table is int8 with a per-row scale. It is read row by row, so only the
  pages holding rows of seen tokens are faulted in by the reads themselves. The process holds
  more: on a read fault of a file mapping Linux also maps the already cached pages of the
  surrounding 64 KiB (`fault_around_bytes`), and readahead has cached them. Measured at E4
  (section 4): after 116 inputs the rows read need 6.6 MiB of pages, 21.7 MiB of the table
  are resident. `MADV_RANDOM` does not change it (kernel 6.8), and the knob is host-wide and
  root-only. These pages are clean and reclaimable. The table below counts 4 KiB pages.
- The table's rows are stored in descending token frequency, with an id → row map in the file
  (250 037 × 4 bytes, about 1 MB). The per-row scale sits with its row, or in an array in the
  same order, so the scales are not a second scattered read. The frequency list comes from a
  fixed public ru+en text sample recorded with the converter, so the output stays
  deterministic. Measured 2026-11-24 on the pilot host, on the held-out half of the Wikipedia
  sample above with the order taken from a separate MIRACL sample:

| Row size | Checkpoint order | Frequency order | Floor (seen rows packed) |
|---|---|---|---|
| 384 bytes (e5-small) | 66.6 MiB | 32.7 MiB | 13.5 MiB |
| 1024 bytes (bge-m3) | 105.5 MiB | 56.4 MiB | 35.9 MiB |

  The pages that serve 99 % of token occurrences shrink from 46.2 to 13.0 MiB (384 bytes) and
  from 74.1 to 32.1 MiB (1024 bytes). e5-small and bge-m3 have the same vocabulary (all 250 002
  pieces map to the same ids), so one frequency list serves both.
- LayerNorm parameters, biases and position embeddings are fp32.
- **The tokenizer section (from E2).** The converter also reads the checkpoint's
  `tokenizer.json` and writes, so that one file and one SHA-256 identify model and tokenizer:
  - `tokenizer.charsmap.trie` (`u32`, the precompiled character map's darts-clone double array
    as the model ships it) and `tokenizer.charsmap.strings` (`u8`, its NUL-ended strings);
  - `tokenizer.pieces.bytes` (`u8`), `.offsets` (`u32`, n + 1), `.ids` (`u32`) and `.scores`
    (`f64`; 65 856 of the scores are not `f32` values): the 250 002 pieces sorted by their bytes,
    searched in place by narrowing a range byte by byte;
  - `meta.tokenizer`: format, pipeline (section 4, E2), vocabulary size, `unk` / `<s>` /
    `</s>` ids, the lowest score's bits, the truncation length (512) and the added tokens.
  Nothing is built at load: binding reads the header and checks the arrays' lengths and end
  offsets; every index read later is checked, so a corrupt section is an error, never a panic.
  The SHA-256 of `tokenizer.json` goes into `meta.source`. The section adds 6 820 176 bytes
  (e5-small file: 127 294 672 bytes). It needs two new element types, `u8` and `f64`: the file
  format is version 2 and the BERT format `ulei-infer/bert-int8/2`; a file of version 1 is
  refused with "convert the model again".
- **The input prefixes (from E6, open question 11).** `meta.prefixes` holds the model's
  `{"query": ..., "passage": ...}`, written by the converter from `--query-prefix` and
  `--passage-prefix` (both required, empty for a model without prefixes; e5: `query: ` /
  `passage: `), so the helper names no model and the file's SHA-256 covers them. The BERT format
  is `ulei-infer/bert-int8/3`; `/1` and `/2` files are refused with "convert the model again",
  a file of another model family with its own reason. The e5-small file is 127 294 736 bytes.
- **The content digest (from E6c, open question 12).** The preamble grows from 24 to 64 bytes
  (weight file version 3, BERT format `ulei-infer/bert-int8/4`): after the JSON header's length
  come the file's total length (`u64`) and a 32-byte content digest. The digest is the SHA-256
  of the whole file, every byte from 0 to its length, with the digest's own 32 bytes read as
  zeros. It therefore covers every byte that changes the output: the preamble (magic, version,
  both lengths), the header (`meta` with config, pooling, prefixes, block size and the
  tokenizer's object; the tensor table), the padding and every blob, the tokenizer section
  included. The converter writes the file with the digest's bytes zero, hashes the bytes as it
  writes them, puts the digest in and renames the file into place; `ulei-infer` has no SHA-256
  of its own (it offers `file::canonical`, the bytes to hash, and `file::seal`), the hash is the
  garden's existing `sha2`. Versions 1 and 2 (BERT formats `/1` to `/3`) are refused with
  "convert the model again". The 64-byte alignment of the header's end and of every blob is
  unchanged. e5-small: 127 294 800 bytes; bge-m3: 602 605 584 bytes (the longer preamble fell
  into the header's padding).
- Every int8 value the converter writes (linear weights, token table) is in [−127, 127]: the
  symmetric quantisation clamps there (`kernels::quantize`; tested on the converter's output).
  No kernel relies on it for the weights: the AVX2 kernel of section 5 is exact for any weight
  byte, −128 included, and the token table is not a matrix operand. A weight file from another
  writer that holds −128 is therefore memory-safe and gives the same numbers; nothing scans
  the mapped file at load (that would page in all of it).
- The converter is a subcommand of the helper (`ulei-garden-embed convert`). It reads the
  model's original `safetensors` checkpoint and quantises in Rust; no Python enters the
  repository's runtime or build. The output is deterministic, so its content digest is the model
  identity of ADR-0047 section 2 (from E6c; before, the SHA-256 of the whole file, hashed at
  every start). `ulei-garden-embed verify FILE` hashes the file again against the digest.

### 3. Activations

- Activations are quantised to int8 for each Linear layer with one scale per token row and per
  block of B input channels. The GEMM is integer inside a block (int32 accumulation); the
  block sums are scaled by their block's activation scale and combined in fp32, then scaled by
  the weight's output-channel scale. B = 32 bytes is one AVX2 register or two NEON registers
  (half an AVX-512 register; ggml's Q8_0 uses the same block), so the kernel loop of section 5
  works block by block. The cost is one fp32 multiply-add per row, block and output: K / 32 =
  12 per output (48 for FFN out) on e5-small, 32 (128) on bge-m3, instead of 1.
- **B = 32 for both models**, one kernel. B is a field of the weight file's header, written by
  the converter (default 32), not a compile-time constant. Simulated 2026-11-24 on the pilot
  host (addendum to the E3 simulation report), min pooled cosine against PyTorch fp32:

| Model, texts | B = 64 | B = 32 | B = 16 |
|---|---|---|---|
| e5-small, 664 texts up to 203 tokens | 0.99950 | 0.99960 | not run |
| e5-small, 144 texts of exactly 256 / 384 / 512 tokens | 0.99968 | 0.99973 | not run |
| bge-m3, 72 texts (60 up to 203 tokens, 12 of 512) | 0.99707 | 0.99799 | 0.99838 |

  B = 64 would do for e5-small; on bge-m3 B = 32 gains 0.0009 at the worst text, and B = 16
  only 0.0004 more. Long inputs are not the hard case for e5-small (mean pooling averages the
  error over the tokens); its worst texts are short queries.
- Why blocks. One scale per row, as first written here, does not reach the 0.999 bound of
  section 4. Simulated 2026-11-24 on the pilot host with exact integer arithmetic, e5-small, 664
  ru/en texts, pooled cosine against PyTorch fp32:

| Scheme | Min | Median | Texts below 0.999 |
|---|---|---|---|
| int8 weights only (per output channel) | 0.99978 | 0.99987 | 0 |
| int8 token table only (per row) | 0.99999 | 1.00000 | 0 |
| weights + table + activations, one scale per row | 0.99830 | 0.99904 | 301 of 664 |
| as above, scale per row and per 64-channel block | 0.99950 | 0.99970 | 0 |
| as above, scale per row and per 32-channel block | 0.99960 | 0.99978 | 0 |
| published ONNX int8 export on ONNX Runtime, for comparison | 0.99128 | 0.99600 | 664 of 664 |

  The error comes from activation quantisation. The Linear inputs have fixed outlier channels
  (the LayerNorm outputs: channel 146 in layers 1 to 4, channel 15 in layers 5 to 10, the row
  maximum in 20 to 89 % of rows; the GELU outputs feeding FFN out reach max/rms 11 to 17). A
  per-row scale spends its range on them; a block isolates them.
- Measured and rejected (same set, min pooled cosine): clipping activations at the 99.9th
  percentile 0.932 (the clipped channels carry the signal; never clip); one scale per weight
  matrix 0.979; one activation scale per sequence 0.972. No single layer needs fp32: keeping
  layer 0 or layer 11 in fp32 moves the min by at most 0.0003; the error accumulates over all
  layers.
- **bge-m3 (same addendum, 72 texts, CLS pooling).** The worst texts are the 512-token
  inputs: CLS pooling reads one token and does not average the error away.
  - The per-row activation scheme is unusable on bge-m3: min 0.976, no better than the
    published ONNX int8 export on ONNX Runtime (0.975). Block scales are mandatory.
  - With B = 32: min 0.99799, median 0.99872.
  - No tested int8 scheme reaches 0.999 on every text. On bge-m3, int8 weights alone (per
    output channel) already reach only min 0.99865.
  - The best tested scheme adds weight scales per output channel and per block of 32 input
    channels: min 0.99880, median 0.99933, with 11 of the 72 texts (9 of the 12 long ones)
    still below 0.999. It costs about 19 MB of scales as fp16 (38 MB as fp32).
  - The simulation's per-text values carry about ±3·10⁻⁴ of jitter from accumulation order
    alone, so a Rust kernel lands within that band, not on the simulated values.
  - The bge-m3 checkpoint is published as `pytorch_model.bin` only (no `model.safetensors`).
  - The acceptance bound for bge-m3 is open question 7 (resolved: min ≥ 0.997, median ≥ 0.998).
- Attention scores, softmax, LayerNorm and GELU run in fp32. They get SIMD only where the
  profile shows them, as in the reference (its profile: GEMM 87 %).
- **GELU of the int8 path** (from E5) is one fixed `f32` formula, `ops::gelu_f32`, in place of
  the exact `erf` in `f64` (41 % of the int8 forward pass at E5). With `z = x · (1/√2)` (the
  `f32` constant), `a = |z|`, every operation an `f32` multiply, add, divide or compare,
  rounded in the order written, no fused multiply-add and no library call:
  - `a < 2`: `(0.5 x) · (1 + z P₁(z²) / Q₁(z²))`, a rational approximation of `erf z`
    (P₁, Q₁ of degree 3 in `z²`);
  - `2 ≤ a < 4`: `c = P₂(a − 2) / Q₂(a − 2)` approximates `erfc a` (degree 4 each), `h =
    (0.5 x) · c`; the result is `x − h` for `x > 0`, `h` otherwise;
  - `a ≥ 4`: `x` for `x > 0`, `0 · x` otherwise.
  The coefficients are the `f32` constants in `ops` (fitted 2026-11-25 by a weighted Lawson
  iteration on the absolute GELU error). Largest absolute difference from the exact GELU over
  every finite `f32` input: 6.1·10⁻⁷ (at x = 2.77; an ignored test sweeps all of them).
  Because it is plain `f32` arithmetic, the portable and AVX2 builds give the same bits (fuzz
  test over every `f32` bit pattern); NEON compiles the same source. The fp32 reference path
  (E1) keeps the exact GELU: it is the checking tool. The Python simulation of section 3 still
  uses the exact GELU; the int8 engine now differs from it by this formula too.
- **Softmax of the int8 path** (from E5c) uses its own `exp`, `ops::exp_f32`, in place of the C
  library's `expf`, under the same rules as the GELU: `f32` multiply, add, compare and bit moves in
  the order written, no fused multiply-add, no library call. With `c = x` clamped to
  `[−87.5, 88.375]`:
  - `k` is the nearest integer to `c · log₂e` (adding and subtracting `1.5 · 2²³`);
  - `r = (c − k · ln2_hi) − k · ln2_lo`, `ln2_hi` with 15 significant bits;
  - `p = (r² · P(r) + r) + 1`, `P` of degree 5 (the Cephes `expf` coefficients rounded to `f32`);
  - the result is `p · 2^k`, `2^k` built from its exponent bits.
  Below −87.5 it gives 0, above 88.375 `+∞`, `NaN` stays `NaN`. Largest relative error over every
  `f32` in `[−87.33, 0]` (all softmax sees): 8.1·10⁻⁸ (an ignored test sweeps them). The softmax
  around it (`ops::softmax_f32`) takes the row maximum over eight running maxima and sums in the
  order of the dot product (eight running sums), so the AVX2 build of the same source gives the
  same bits (fuzz test over every `f32` bit pattern and random rows). The fp32 reference path keeps
  `expf`. Measured in section 5.3.
- Activation buffers come from a per-process pool reused across layers and batches, as the
  reference Demucs port does (gitlab.com/asvedr/gemucs, `internal/kernels/pool.go`). **Done at
  E6b** (section 4): `bert::Workspace` with the kernels' `kernels::Scratch`, kept by the caller;
  each buffer grows to the longest input and is never shrunk (`Workspace::release` frees it), so no
  layer allocates at steady state. The helper keeps one for its life and releases it after a call
  that left it above 16 MiB (the idle row of section 6). Since section 9 it keeps two, one per lane:
  the passages' up to 16 MiB, the queries' up to 2 MiB.

### 4. Milestones and acceptance, in order; none starts before the previous one is green

| Step | Content | Accepted when |
|---|---|---|
| E0 Reference | Golden dumps from the reference (PyTorch fp32 and ONNX Runtime int8) on a fixed ru/en text set: token ids, embedding output, every layer's output, pooled vector. Generated outside the repository (pasekahq/paseka-evals); the repository holds their SHA-256 and a skip when absent. PyTorch fp32 is the reference of every accuracy check; the ONNX Runtime run is only the baseline the engine must not be worse than | the dump script is reproducible; the SHA-256 of the dumps and of the checkpoint are recorded in the repository (still to do) |
| E1 Naive fp32 | Portable Rust forward pass, fp32 weights | every layer max abs diff ≤ 1e-4 against PyTorch fp32; pooled cosine ≥ 0.99999. Done for e5-small (below) |
| E2 Configs | e5 mean pooling and prefixes, bge CLS; tokenizer in the crate (module `text`, data in the weight file) | e5-small and bge-m3 both pass E1; token ids equal to the reference's on every text checked |
| E3 int8 | Format of section 2, activation quantisation of section 3 | e5-small: pooled cosine against PyTorch fp32 ≥ 0.999 on every text of the fixed set, including 512-token inputs (bge-m3 and other 24-layer models: min ≥ 0.997 and median ≥ 0.998, open question 7); on the ADR-0086 section 9.2 sets, the paired-bootstrap 95 % interval of the recall@3 difference against fp32 lies within ±2 points, and the engine is not below the ONNX Runtime int8 run by more than that interval |
| E4 Memory and start | mmap, buffer pool | dirty RSS and clean RSS measured per model (targets in section 6); ready within 100 ms of start with a warm page cache. "Ready" means the model is bound and able to take input; the time to the first vector is recorded beside it, set by the kernels (E5) and the input length |
| E5 Kernels | Section 5 | each kernel passes the bit-equality fuzz test on real hardware with its flag; speed recorded per host class |
| E6 Helper swap | `ulei-garden-embed` built on the encoder as a static musl binary; same pipe protocol and model identity | the helper's budget row (section 6) holds in CI; `garden-onnx` replaced by an encoder job |
| E7 Removal | `ort` and `spm_precompiled` dropped | its own ADR, after E6 has run on the reference hosts of ADR-0086 section 5 |

e5-small goes through E0 to E6 first, being the smallest and quickest to verify. bge-m3 then
repeats E0 to E4 on the same code.

- **E1 result (e5-small, 2026-11-24, pilot host).** All 16 golden cases pass. Worst layer max
  abs diff 8.5e-6 (512-token case, layer 9) against the 1e-4 bound; worst pooled cosine
  1 − 1.8e-13. Commit 7bf099b; crate renamed to `ulei-infer` in 9bf9385 with identical diffs.
- **E2 result, tokenizer (e5-small; bge-m3's tokenizer alone; 2026-11-25, pilot host).**
  Commit 74cffff.
  - Decisions (the architect's, followed): own implementation, no new dependency; the
    reference is the `tokenizers` library as `transformers` drives it for the checkpoint
    (`AutoTokenizer`, truncation 512), not the SentencePiece C++ library; the data lives in the
    weight file (section 2); a model-independent module `text` behind a feature, used by `bert`;
    truncation as the reference (first 510 pieces between `<s>` and `</s>`); the engine takes
    text already prefixed (`Encoder::embed_text`), the caller adds `query: ` / `passage: `.
  - Two reference pipelines exist for these checkpoints. `transformers` 5 builds its own for
    XLM-RoBERTa: the character map, then split at every whitespace character, `▁` before each
    word. `tokenizer.json` describes another, which the `tokenizers` library alone and the ONNX
    helper's tokenizer run: runs of spaces collapsed to one, `▁` before the text, so trailing or
    repeated whitespace becomes a `▁` piece. They differ on 1 of 16 golden texts, 288 of the
    664 synthetic texts (every passage ending in a newline), none of the 283 844 Wikipedia
    texts. The golden dumps, the simulation and the E3 baselines used the first; it also equals
    the SentencePiece library on the 664 synthetic texts. `text` implements both
    (`Pipeline::WhitespaceSplit`, the converter's default, and `Pipeline::CollapseSpaces`);
    the E6 helper's choice was open question 10 (resolved).
  - Ids against the reference, 0 mismatching texts in every set: 16 golden cases (1 772 ids), 664
    synthetic texts (45 113), 283 844 ru+en Wikipedia texts (the MIRACL paragraphs of the paging
    study with `passage: `, and their sentences with `query: `; 14 045 569 ids), 16 177
    adversarial texts (control and zero-width characters, bidi marks, every whitespace
    character, combining sequences, 3 000 non-NFKC characters, emoji sequences, scripts, U+FFFD,
    literal special tokens, words of up to 100 000 bytes, the empty string), 178 texts of 495 to
    525 ids around the truncation edge, 20 000 random strings. The same holds for
    `CollapseSpaces` against `tokenizer.json`, and for bge-m3's tokenizer (`<mask>` with
    `lstrip`) against its `AutoTokenizer`.
  - The ONNX helper's tokenizer equals `tokenizer.json`'s pipeline on the golden, synthetic,
    Wikipedia and edge sets; it differs on 42 adversarial texts (36 with literal special tokens,
    6 words over 1 024 bytes) and on the random strings with special tokens.
  - Text in, end to end: the tokenizer gives the dumped `input_ids` of all 16 golden cases, and
    `embed_text` is bit-identical to the forward pass on those ids (golden output unchanged, min
    pooled cosine 0.999562); the 116 probe vectors are byte-identical text in and ids in.
  - Cost (release build, one thread; other load on the host about 0.3 vCPU, steal under 1 %
    of one): median 114 µs per text and 4.4 µs per id on the 116 probe inputs, 39 ms for all
    of them against 11.7 s to embed them (0.34 %). Bound in 2.2 to 3.1 ms, first vector of an 8-token query 19 to 24 ms
    from spawn, as with ids in. After the 116 inputs: anonymous 3.4 MiB, clean pages of the file
    50.6 MiB (45.3 MiB ids in): the range search touches 5.5 of the section's 6.5 MiB.
  - `ulei-garden-embed` (release gnu, `onnx`) grows 35 912 B (658 400 → 694 312 B), the
    converter's part; the encode path adds 27 104 B to a forward-pass binary, the expected
    growth of the E6 helper.
  - The grapheme cluster rules (Unicode 17.0.0) are generated from `unicode-segmentation`'s
    tables; notices for it and for the ported `tokenizers` behaviour are in
    `THIRD_PARTY_NOTICES.md`.
  - Not done in E2: bge-m3 through E1 (no `safetensors` checkpoint, section 3); the prefix's
    place for E6 (open question 11).
- **E3 result (e5-small, 2026-11-24, pilot host), fidelity half.**
  - All 16 golden cases are ≥ 0.999. With B = 32: min 0.99958, the 512-token cases 0.99977 and
    0.99978. With B = 64: min 0.99939. The minimum is on the 4-token empty input both times.
  - The recall half of the E3 criterion: run at E2 on the Rust engine, text in (below).
  - The converter's int8 weights, token table and scales are bit-identical to the simulation's.
  - Rust and the simulation cannot be compared bit for bit end to end. A value within a few ulp
    of an int8 rounding boundary rounds the other way under any change of float order
    (LayerNorm, softmax, GELU), and the one-step change grows through the layers. The
    simulation against itself, with only its LayerNorm in f64, diverges as far: tensor max abs
    diff 0.12, pooled 1 − cos up to 1.6·10⁻⁴.
  - So the check is twofold. Each Linear is run on the simulation's own input: max abs diff
    ≤ 1e-5 · (1 + |y|), measured 4.8·10⁻⁷ at B = 64 and 7.1·10⁻⁷ at B = 32. Then the fp32
    bound applies end to end.
  - The e5-small weight file is 120 474 496 bytes (114.9 MiB) against "≤ 120 MB" in section 6
    (open question 9).
  - Commits 33066d9 (format, converter, int8 path) and 7bb1de3 (B = 32 default).
- **E3 result, recall half (e5-small, 2026-11-25, pilot host).** The 144 synthetic documents
  and 340 queries embedded by the Rust engine from text (`passage: ` / `query: `, the file's
  tokenizer, the current int8 path) against PyTorch fp32, the protocol of the E3 simulation
  (pool of 144 documents, 2 000 paired bootstrap resamples by question):

| | R@1 | R@3 | R@10 | MRR |
|---|---|---|---|---|
| PyTorch fp32 | 59.71 | 76.18 | 89.71 | 70.06 |
| Rust int8, text in | 59.12 | 76.18 | 90.00 | 69.69 |

  - ΔR@3 +0.00, 95 % interval [+0.00, +0.00]: no query's hit at 3 changed. ΔMRR −0.38
    [−0.88, 0.00]. 32 of 340 top-3 lists changed. The interval lies within ±2 points: the
    recall half holds. ONNX Runtime int8 on the same protocol: ΔR@3 −0.29 [−3.24, +2.65], so the
    engine is not below it by more than that interval.
  - Pooled cosine against fp32 on the 664 texts: min 0.999600, median 0.999779.
  - Not run: the pool with 10 000 filler records.
- **E4 result (e5-small, 2026-11-24, pilot host).** The weight file is mapped private and
  read-only; the tensors are slices into the mapping, checked once at open for range and
  alignment.
  - Output bit-identical to E3: the 116 pooled vectors (16 golden cases, 100 ru/en texts of 12
    to 189 tokens) are byte-identical, and the golden test output is unchanged (min pooled
    cosine 0.999576).
  - Memory after the 116 inputs (8 615 tokens), release build, glibc, one thread:

| | RSS | Anonymous | Clean, weight file | Peak anonymous while embedding |
|---|---|---|---|---|
| E4, mmap | 51.2 MiB | 3.5 MiB | 45.0 MiB | 7.8 MiB (512-token inputs) |
| E3, owned buffers | 121.9 MiB | 119.2 MiB | 0 | 124.0 MiB |
| ONNX Runtime helper (ADR-0086 section 9.2) | 295 MB | all | | |

  - Of the 45.0 MiB clean: all linear weights (20.25 MiB, every input needs them), the id →
    row map (1 MiB, touched once at bind by its range check) and 21.7 MiB of the token table,
    of which the rows read need 6.6 MiB (section 2, fault-around). The clean worst case on a
    large mixed corpus is the whole file, about 115 MiB [inferred, not measured]; it is
    reclaimable, not anonymous.
  - Start-up, warm page cache: the model is bound and able to take input after 2.3 to 3.9 ms
    (7.5 to 8.5 ms with the weight file evicted). The first vector of an 8-token query is out
    after 248 to 266 ms warm: the portable int8 forward pass at the size-first release profile.
    That number is re-measured after E5. The ready target of E4 is met.
  - The buffer pool of section 3 is not done at E4. Activations are allocated per operation; the
    anonymous peak above is their high-water mark plus glibc retention. (Done at E6b.)
  - Not measured at E4: the idle RSS after 10 minutes (section 6). It moves to E6, where idle is
    a real state of the helper process.
  - The release profile (`opt-level = "s"`) does not align loops. On the pilot host (Skylake,
    JCC erratum microcode) the portable forward pass ran 1.9× slower after an unrelated code
    change and returned to E3's speed at opt-level 3 or with LLVM's
    `-x86-branches-within-32B-boundaries`. Every speed record from E5 on names its build flags.
  - The `unsafe` of the mapping is the private module `weights::map` (section 5).
  - Commits 53e4f43 (mmap) and d9a023b (memory and start-up probe, an example of the crate).
- **E5 result (e5-small, 2026-11-25, pilot host, kernel 6.8.0-149), AVX2 half.**
  - Kernels: AVX2 (enabled, picked at run time) and NEON (compiled only). A kernel computes
    only the integer block sums; the fp32 combination is shared code, so every kernel's
    output is bit-identical to the portable one. AVX2 uses `VPMOVSXBW` + `VPMADDWD` on pairs of
    channels, one output channel per lane, no `VPMADDUBSW`.
  - Bit-equality: 1.33 million random block-sum cases and 200 000 random linear layers
    (widths and blocks not multiples of the vector, −128 and 127) equal portable bit for bit;
    the 116 pooled vectors of E4 are byte-identical under every kernel and both build
    profiles; the golden output is unchanged (min pooled cosine 0.999576).
  - Speed, one thread, 116 inputs (8 615 tokens), median of 5 rounds after a warm-up round,
    release gnu build with `ulei-infer` at opt-level 3 (section 5.2):

| Tokens (inputs) | portable | AVX2 | ONNX Runtime 1.28.0 int8 | AVX2 / ONNX Runtime |
|---|---|---|---|---|
| 4–16 (32) | 132.0 ms | 45.8 ms | 11.5 ms | 3.97 |
| 17–64 (33) | 175.7 ms | 56.0 ms | 13.4 ms | 4.18 |
| 65–192 (49) | 1 196 ms | 380 ms | 71.5 ms | 5.31 |
| 512 (2) | 5 449 ms | 2 083 ms | 413 ms | 5.04 |
| all 116, per round | 81.8 s | 27.0 s | 5.31 s | 5.09 |

  - At the size-first profile (`opt-level = "s"` everywhere) the AVX2 total is 59.7 s and
    portable 142.4 s.
  - **The provisional "≤ 1.5× ONNX Runtime" of section 6 is not met: 5.0 to 5.3× on
    passages.** Where the time goes (AVX2, all inputs): GELU 41 % (the exact `erf` in
    `f64`), int8 linear 37 %, attention 15 % (35 % at 512 tokens), quantisation 4 %.
    A cheaper GELU changes outputs and needs the golden bound re-run; it is proposed, not
    done. So are `VPMADDUBSW` under the [−127, 127] clamp the format guarantees (pair sums
    at most 32 258, no saturation), an AVX-512BW port, and the fused attention of 5.1.
  - Start-up, warm page cache: bound in 2.2 to 2.4 ms; the first vector of an 8-token query
    in 34 to 35 ms from spawn (E4: 248 to 266 ms); 149 to 170 ms with the weight file evicted.
  - Bit-identical changes beside the kernel: the shared Q/K/V input is quantised once, and
    activation quantisation and the attention products are the portable source compiled a
    second time with AVX2.
  - Not done: NEON on hardware (no aarch64 host; it stays disabled), AVX-512BW, VNNI.
  - The E5 acceptance holds for AVX2 on the pilot host's class; for NEON it waits for an
    aarch64 run. Commit add0af9.
- **E5 follow-up (e5-small, 2026-11-25, pilot host, kernel 6.8.0-149): three speed steps.**
  - `f32` GELU (section 3), commit 20e5f6c. It changes outputs. Golden cases against PyTorch
    fp32: min pooled cosine 0.999562 (was 0.999576, the 4-token empty input both times), all
    16 ≥ 0.999. On the 116 probe inputs: against the previous int8 vectors min 0.999929, median
    0.999968; against PyTorch fp32 min 0.999562, median 0.999781 (previous int8: 0.999576,
    0.999780).
  - `VPMADDUBSW` in the AVX2 kernel (section 5), commit fc34a28. Bit-identical.
  - Attention with the keys transposed per head (section 5.1), commit f53cf1f. Bit-identical.
  - Bit-equality after all three: 1.48 million random block-sum cases of the `VPMADDUBSW`
    kernel, 200 000 random linear layers, 20 000 random attention shapes and every `f32` bit
    pattern through the GELU equal portable; the 116 probe vectors of the last two steps are
    byte-identical to the first's, and the golden output is unchanged by them.
  - Speed, measured as for E5 (one thread, 116 inputs, median of 5 rounds after a warm-up
    round, release gnu build, `ulei-infer` at opt-level 3, no `-C` flags), all builds and ONNX
    Runtime 1.28.0 in one session, other load on the host about 0.1 vCPU:

| Tokens (inputs) | E5 | + `f32` GELU | + `VPMADDUBSW` | + attention | ONNX Runtime 1.28.0 | last / ONNX Runtime |
|---|---|---|---|---|---|---|
| 4–16 (32) | 43.9 ms | 22.1 ms | 21.5 ms | 21.0 ms | 9.8 ms | 2.14 |
| 17–64 (33) | 52.9 ms | 28.2 ms | 26.0 ms | 25.1 ms | 11.7 ms | 2.14 |
| 65–192 (49) | 361 ms | 199 ms | 188 ms | 178 ms | 63.6 ms | 2.80 |
| 512 (2) | 1 955 ms | 1 358 ms | 1 334 ms | 1 145 ms | 341 ms | 3.36 |
| all 116, per round | 25.6 s | 14.4 s | 14.1 s | 13.2 s | 4.64 s | 2.84 |

  - Where the time goes now (all inputs; timer build): int8 linear 61 %, attention 25 %
    (scores 11 %, softmax 9 %, weighted sum 5 %), activation quantisation 7 %, LayerNorm 2 %,
    GELU 2 %. At 512 tokens attention is half: scores 23 %, softmax 17 % (a `libm` `expf` per
    score), weighted sum 10 %.
  - **The provisional "≤ 1.5× ONNX Runtime" of section 6 is still not met: 2.8× in total,
    2.8 to 3.4× on passages** (was 5.1×). The column stays as it is; it is the owner's
    decision (section 6).
- **E5c (e5-small, 2026-11-25, pilot host): profile and own `exp`.** An instruction-level profile
  of the linear kernel, a bound on its per-block `f32` work, and an own `exp` in the int8 path's
  softmax (section 3), commit 983eeba. Golden min pooled cosine unchanged (0.999562). Results
  in section 5.3, beside the estimates they test.
- **E6 result (e5-small, 2026-11-25, pilot host, kernel 6.8.0-149).** Commit 417fe8c. Report
  `reports/e6-helper.md` in the experiment directory. **Every timing below was taken with
  another workload on the host (load average 1.9 to 2.6 on 3 vCPUs, steal under 0.3 %) and is
  to be re-measured.**
  - The helper `ulei-garden-embed` gains the backend `native` (cargo feature `native`): the
    int8 engine on the weight file `model.ulei-infer` in the model directory, one text at a time
    on the main thread, the pooling, tokenizer and prefixes from its header. The default
    `--embedder auto` picks the backend by the model file in the directory (both files:
    refused), so the daemon's command line, the pipe protocol and an ONNX deployment are
    unchanged; ADR-0047 section 1's `--embedder onnx|fake` (default `onnx`) is amended so. The
    ONNX backend stays buildable and selectable until E7 (open question 3).
  - Identity: both digests of the ready line are the weight file's SHA-256 (it holds the
    tokenizer), read from the descriptor the engine maps, so a file renamed into place takes
    effect at the next start; the new id makes the worker embed every fact again (ADR-0047
    section 2), verified by a test that moves a store from an ONNX-shaped helper to the engine.
  - Built with `native` alone the helper is a static musl binary (742 136 B; the ONNX gnu build
    704 024 B; budget 2 MiB). Nothing in it needs a C library or `dlopen`; the crate's bundled
    SQLite is compiled for the target (so the build needs `musl-tools`, as the daemon's does) but
    not linked into the helper. `ci/budgets.sh` checks its size and that it is static; the CI job
    `garden-encoder` converts a tiny checkpoint at run time and runs the musl daemon with it,
    beside `garden-onnx` while ONNX stays selectable.
  - Vectors: the helper's are the engine's text-in vectors bit for bit on the 664 synthetic
    texts (gnu and musl). Against the ONNX helper on e5-small (ONNX Runtime 1.28.0): cosine
    min 0.98968, median 0.99536, different quantisations and tokenizer pipelines (question 10).
    garden's own path (`garden-eval --embedder helper`, 144 documents, 340 queries, hybrid):
    recall@3 0.532 against 0.525, MRR 0.597 against 0.600.
  - Section 6 on the helper (116 E4 inputs, release, one thread; ONNX Runtime `--threads 1`):

| | native gnu | native musl | ONNX Runtime helper |
|---|---|---|---|
| anonymous (dirty) after the inputs | 3.3 MiB | 0.3 MiB | 225.3 MiB |
| clean pages of the weight file | 53.0 MiB | 51.3 MiB | 17.9 MiB |
| after 600 s idle | unchanged, 0 wake-ups | unchanged, 0 wake-ups | unchanged, 0 wake-ups |
| ready line after spawn | 791 ms | 779 ms | 1 513 ms |
| all 116 inputs, median of 5 rounds | 18.7 s (3.52×) | 23.6 s (4.44×) | 5.31 s |

  - Ready is red: the model is bound about 10 ms after spawn, then the SHA-256 of the 127 MB
    file takes about 0.75 s. Speed is red: the helper's code is a `ulei-garden` binary at
    `"s"` (section 5.2), and the musl build is 26 % slower than gnu on the same code
    (allocator, not profiled). A 512-token passage takes about 1.7 s, the most a query waits
    behind the worker (ADR-0047 section 2; its 5 s call deadline holds).
  - Not changed: C9 (`garden.embedder.ort_library` is still required and ignored by `native`;
    `model_dir`'s description names only the ONNX files).
- **E6b result (2026-11-25, pilot host, kernel 6.8.0-149): buffer pool, the musl gap, the helper's
  codegen.** Commits b029533 (pool), 4b198df (the helper keeps it), 0c8c4ad (quads). Report
  `reports/e6b-pool.md` and the re-measure before it, `reports/e6-remeasure.md`, in the experiment
  directory.
  - **Pool** (section 3): `Encoder::embed_with` / `embed_text_with` take a `bert::Workspace`;
    `embed`, `embed_text` and `forward` take a new one per call. The kernels gained `_into` forms
    that write into the caller's buffers, with their own buffers in a `kernels::Scratch`; their
    arithmetic is unchanged. Every value is written before it is read, so a reused workspace gives
    the bits of a new one (a unit test, and the fuzz tests now share one stale scratch across all
    cases and kernels). No `unsafe`; a buffer is zeroed only where it grows.
  - **Why it mattered: static musl.** The re-measure found each forward pass making about 5 800
    allocations, 49 of them of 128 KiB or more (1.7 GB per round of the 116 inputs). glibc reuses
    such blocks; musl maps and unmaps them every time (2.35 M minor faults against 57 k) and its
    `memcpy` / `memset` are `rep movsq` / `rep stosq`, slow to start. Together 94 % of the musl
    build's 33 % gap.
  - **What closed it.** The pool: allocator calls per forward pass 5 831 → 9 (none of the 9 in a
    layer, none of 128 KiB or more at steady state); minor faults per round 12 411 → 1 (glibc),
    586 436 → 2 (musl). Then counting `memcpy` calls showed 89.3 million per round, 4 bytes each:
    the AVX2 kernel packing each token row into quads, where the byte loop of a quad compiled to a
    `memcpy` call. A full quad is now one 4-byte load (commit 0c8c4ad): 4 176 calls per round. musl
    against glibc, bare engine: 1.33× → 1.05×; helper 1.30× → 0.99×.
  - Speed, e5-small, 116 inputs, one thread, median of 5 rounds after a warm-up, release profile
    as committed (`"s"`, `ulei-infer` at 3, fat LTO, one codegen unit, no `-C` flags; musl with the
    host `gcc` for C), quiet host (other load under 0.3 vCPU before every run, steal under 0.15 %),
    ONNX Runtime 1.28.0 in the same session:

| Build | E6 code (same day, quiet host) | E6b | E6b / ONNX Runtime |
|---|---|---|---|
| engine alone (`speed_probe`), glibc | 12.07 s | 11.89 s | 2.31 |
| engine alone, static musl | 16.06 s | 12.49 s | 2.43 |
| helper `ulei-garden-embed`, glibc | 17.76 s | 18.79 s | 3.65 |
| helper, static musl (the shipped build) | 23.10 s | 18.52 s | 3.60 |
| ONNX Runtime 1.28.0 (Python, same ids) | 5.09 s | 5.15 s | 1 |

  - The helper on glibc is 5 to 6 % slower than before (same session, rounds do not overlap); not
    isolated. The engine alone and the helper at opt-level 3 (below) do not show it.
  - **Helper against the engine alone: 1.6×, not closed by E6b.** A non-inlined entry point did not
    help (the helper calls only non-generic engine functions; `#[inline(never)]` on them changed
    nothing measurable); the same helper without fat LTO runs as fast as the probe. Cause [inferred]: under fat LTO, loop and SLP vectorisation run only
    in the link-time pipeline, at the opt-level of the binary's package, and rustc runs neither at
    `"s"`; `ulei-infer`'s opt-level 3 (section 5.2) keeps its scalar optimisation, but its
    auto-vectorised loops (activation quantisation, GELU, softmax, the score dot products, the `f32`
    combination of the block sums) are vectorised only in a binary of a package at 2 or 3. The
    hand-written AVX2 parts are not affected. Nothing in `ulei-infer`'s source changes that short of
    explicit SIMD in those loops. The option is open question 14 (resolved: `ulei-garden` at 3).
  - bge-m3, 76 inputs, engine alone, 3 rounds after a warm-up: glibc 91.7 s, static musl 84.7 s,
    ONNX Runtime 33.6 s in one session (2.73× / 2.52×). bge-m3 runs drift by ±5 % between runs here;
    interleaved with the E6 code, glibc is unchanged (E6 code 90.4 to 91.4 s, E6b 87.4 to 92.2 s) and
    static musl is now about 5 % faster than glibc (83.9 to 87.4 s; not explained). The 76 vectors
    are byte-identical to E4's.
  - **Memory against section 6** (`mem_probe`, anonymous = dirty): e5-small holds 9.2 MiB of
    buffers after its 512-token inputs: 10.4 MiB dirty while embedding (E6 code: peak 8.7 MiB),
    9.8 MiB in the idle helper after the 116 inputs (E6: 3.3 MiB) — under 60 and 20 MB. bge-m3 holds
    24.4 MiB after a 512-token input: 26.6 MiB while embedding (under 150 MB); the helper releases
    them after such a call (`embed::native::KEEP_BYTES`, 16 MiB), which leaves 1.0 (musl) to 4.6
    (glibc) MiB in the probe; the static musl helper on bge-m3 holds 9.3 MiB after the 116 inputs
    (the buffers of its shorter inputs). No timer.
  - Outputs: golden output unchanged (e5-small, bge-m3); the 116 probe vectors, the 664 helper
    vectors and the 76 bge-m3 vectors byte-identical; `fuzz_long` green.
  - Start-up unchanged (ready 0.70 to 0.77 s: the identity hash, open question 12, not touched).
  - bge-m3's weight file reconverted to format 3 (empty prefixes, CLS, B = 32, `whitespace-split`):
    602 605 584 bytes, SHA-256 `44bd8f56…6e5d26`; the 16 golden cosines are those of section 4.1
    (min 0.99848).
- **E6c result (2026-11-25, pilot host, kernel 6.8.0-149): the identity from the file's content
  digest (open question 12).** Commit 3f3df29. Report `reports/e6c-hash.md` in the experiment
  directory.
  - Format: section 2, "The content digest". The helper's start checks only what reads no blob:
    the preamble (magic, version, reserved field), the file's length against the recorded one,
    the header's JSON and the tensor table (offsets, alignment, bounds, nothing after the last
    tensor), and that a digest was sealed. A file that fails is refused as "model file
    corrupted: <path>: <reason>" (truncated, too long, malformed, never sealed), distinct from
    "format version N, expected 3: ...; convert the model again" for an older file. A changed
    byte inside a blob, or a changed `meta` string that still parses, is not seen at start.
  - Identity on the ready line: `model_sha256` and `tokenizer_sha256` are both the sealed content
    digest (the file holds the tokenizer), as both were the file's SHA-256 before, so the garden's
    identity logic and the pipe are unchanged. **The value changes once**: it is not the old file
    hash (other preamble, and the digest is over the canonical bytes), so the first start on a
    format-4 file gives a new model id and the worker embeds every fact again (ADR-0047 section 2),
    as at E6. Identities: e5-small `a06d503f…25d923`, bge-m3 `60691050…349270`.
  - `ulei-garden-embed verify FILE` runs the start's checks, hashes the whole file and compares:
    `OK FILE: content sha256 <hex>, <bytes> bytes, <format>` and exit 0 on a match; otherwise one
    stderr line and exit 1 ("model file corrupted: content SHA-256 is X, the preamble says Y", or
    both lengths for a truncated or longer file, or the format refusal), exit 2 on bad usage. The
    CI job `garden-encoder` runs it after `convert`. (Since section 8: exit 3 for a corrupted file,
    exit 1 for the format refusal and a file that cannot be read.)
  - Both real files reconverted to format 4 with the E6b commands (e5-small: mean, `query: ` /
    `passage: `, B = 32, `whitespace-split`; bge-m3: CLS, no prefixes, B = 32, `whitespace-split`).
    The data sections are byte-identical to the format-3 files' and the headers equal but for
    `meta.format`; the golden output on both is line for line that of E6b (e5-small worst 0.999562,
    bge-m3 worst 0.998485); the helper's 664 synthetic vectors are byte-identical to E6b's, gnu
    and musl. The format-3 files were deleted.
  - Start-up, release as committed, 9 starts (e5-small) and 5 (bge-m3) after one warm-up, median;
    quiet host (other load under 0.2 vCPU before every series, steal under 0.3 %). "Cold": the
    file's pages dropped from the guest's page cache with `posix_fadvise(DONTNEED)` before each
    start (no root needed; the hypervisor's own cache is out of reach, so cold here may be warmer
    than a real cold boot):

| Helper | e5-small before | e5-small after | bge-m3 before | bge-m3 after |
|---|---|---|---|---|
| gnu, warm: ready line | 837 ms | 3.3 ms | 3 500 ms | 3.5 ms |
| gnu, cold: ready line | 887 ms | 12.2 ms | 4 642 ms | 10.5 ms |
| static musl, warm | 694 ms | 3.3 ms | 3 674 ms | 4.4 ms |
| static musl, cold | 847 ms | 11.4 ms | 4 060 ms | 12.4 ms |
| first vector of an 8-token query, cold, gnu | 928 ms | 207 ms | 4 930 ms | 1 332 ms |
| weight file in the page cache after a cold start | all of it (121.4 / 574.7 MiB) | 1.6 MiB | all of it | 1.6 MiB |

  - "Ready after start ≤ 100 ms" (section 6) is met for both models, warm and cold. What is left
    of the 3 to 12 ms [inferred from the E4 probe's breakdown, not profiled here]: process start
    and `exec`, `mmap`, reading and parsing the JSON header (29 KiB for e5-small, 57 KiB for
    bge-m3) and binding the tensors and the tokenizer section's arrays; cold, the header's and
    the tokenizer arrays' first pages come from disk. The first vector then faults in the pages
    it touches.
  - `verify`, whole file, 3 runs: e5-small 0.70 to 0.87 s, bge-m3 3.3 to 4.6 s (warm and cold,
    gnu and musl; about 170 MB/s of SHA-256 on one vCPU, `sha2` without SHA extensions).
  - Helper binary: gnu 656 392 B (+6 408), static musl 754 424 B (+4 096), ONNX gnu 722 880 B;
    budget 2 MiB.
  - Garden side not changed here; what the change leaves open there is open question 15.
- **E7k result (2026-11-25, pilot host, kernel 6.8.0-149): several token rows per pass over
  the weights, and what the measurement found instead.** Commits 3faec51 (`op_probe`), b7b35aa
  (linear layer), 47557f6 (quantisation), 20ac72a (attention). Report `reports/e7k-multirow.md` in
  the experiment directory.
  - **The premise did not hold.** The AVX2 kernel already shared each load of a packed weight
    quad among four token rows and packed every tile once per call; the weights were never
    streamed once per row. The flat cost per token is the integer core running near its port
    limit. Measured with one process timing many plans in turn, under the host's quiet lock:
    2 to 8 rows per tile, 1 to 3 tiles of eight outputs per pass, chunks of 4 to 16 KiB of packed
    weights (cache blocking over the input) and a `VPMADDWD` variant on sign-extended `i16`
    pairs. Four or six rows with one tile were best for every shape and row count; chunking,
    wider tiles and the `i16` variant did not pay. The kernel keeps four rows per tile for every
    shape (`kernels::ROWS`).
  - **What changed.**
    - Linear layer: the block sums are no longer stored. After each block of 32 inputs the eight
      lanes of each row are converted, times the row's block scale, added to an `f32` register,
      and at the end times the weight scale plus the bias go straight into the output. This is
      `kernels::combine`'s order, one output per lane, no FMA.
    - Activation quantisation with AVX2 intrinsics: the same maximum, scale, IEEE division,
      round half away from zero, clamp and `NaN` handling, eight values at a time.
    - Attention: each head's keys in contiguous panels of eight keys (before: a 2 KiB stride
      at 512 tokens and a bounds check per vector load in the score loop), each panel serving
      four queries; each head's values copied contiguous, two queries per pass for both head
      widths.
    - Every result is bit-identical (below). NEON keeps `combine` and its per-row loop; it
      compiles for `aarch64` and has still never run on hardware.
  - Speed, e5-small, 116 inputs, one thread, median of 5 rounds after a warm-up (min to max in
    brackets), release as committed (`ulei-infer` and `ulei-garden` at 3, fat LTO, one codegen
    unit, no `-C` flags), one session with ONNX Runtime 1.28.0 at its end. Quiet host: other
    load at most 0.13 vCPU and steal at most 0.2 % during every run:

| Build | HEAD c57435b | E7k | HEAD / ORT | E7k / ORT |
|---|---|---|---|---|
| engine alone (`speed_probe`), glibc | 12.47 s (12.38–13.12) | 9.50 s (9.37–10.03) | 2.61 | 1.99 |
| engine alone, static musl | 11.06 s (10.94–11.18) | 8.54 s (8.39–9.66) | 2.32 | 1.79 |
| helper `ulei-garden-embed`, glibc | 11.90 s (11.60–12.48) | 9.71 s (9.31–10.93) | 2.49 | 2.04 |
| helper, static musl (the shipped build) | 12.01 s (11.74–12.49) | 9.25 s (8.79–9.59) | 2.52 | 1.94 |
| ONNX Runtime 1.28.0 (Python, same ids) | 4.77 s (4.63–4.90) | | 1 | |

  - By length, engine alone on glibc, sum over the bucket's inputs: 4–16 tokens (32 inputs)
    665 → 482 ms (ORT 306; 2.17 → 1.58×), 17–64 (33) 948 → 702 ms (419; 2.26 → 1.68×), 65–192 (49)
    8.72 → 6.85 s (3.33; 2.62 → 2.06×), 512 (2) 2.11 → 1.46 s (0.71; 2.97 → 2.06×).
  - Helper latency per request (median): a query of up to 16 tokens 19.5 → 16.1 ms (static musl;
    glibc 19.5 → 18.1 ms; ORT 9.9 ms), within the 150 ms query target before and after; a
    512-token input 1 040 → 730 ms (static musl; glibc 967 → 739 ms; ORT 354 ms).
  - bge-m3, 76 inputs, engine alone, HEAD and E7k interleaved in 5 passes, one round per process;
    ONNX Runtime 5 rounds after a warm-up in the same session: glibc 92.2 s (90.7–93.9) → 74.1 s
    (71.9–74.8), 2.87 → 2.31×; static musl 84.8 s (84.0–92.8) → 70.2 s (68.8–71.0), 2.64 →
    2.19×; ORT 32.1 s (31.2–34.4). By length (glibc): 4–16 tokens 2.42 → 2.15×, 17–64 2.59 →
    2.12×, 65–511 2.85 → 2.34×, 512 3.42 → 2.46×.
  - Where the time goes after the change (`op_probe`: one int8 layer of each model's shape on
    synthetic data, instrumented timers, median of 7 (e5-small) or 5 (bge-m3) runs, share of the
    layer):

| Shape, tokens | layer time, HEAD → E7k | linear | attention | quantisation | GELU | residual + LayerNorm |
|---|---|---|---|---|---|---|
| e5-small, 16 | 2.16 → 1.35 ms | 86 % | 2 % | 6 % | 3 % | 3 % |
| e5-small, 128 | 14.2 → 10.5 ms | 72 % | 15 % | 6 % | 3 % | 4 % |
| e5-small, 512 | 81.0 → 59.6 ms | 50 % | 40 % | 4 % | 2 % | 3 % |
| bge-m3, 16 | 9.6 → 9.3 ms | 93 % | 2 % | 2 % | 1 % | 2 % |
| bge-m3, 128 | 71.2 → 62.3 ms | 86 % | 8 % | 3 % | 2 % | 2 % |
| bge-m3, 512 | 386 → 290 ms | 72 % | 22 % | 2 % | 1 % | 2 % |

  - The linear layers run at 23 to 31·10⁹ multiply-adds per second (fewest at 16 tokens). By
    instruction count, the integer core (`VPSIGNB`, `VPMADDUBSW`, `VPMADDWD`, `VPADDD` per row,
    quad and eight outputs, three of them on the two vector multiply ports) allows about 20
    multiply-adds per cycle; the clock under this load is not known in the guest, so the kernel's
    efficiency is 60 to 80 % at 2.0 to 2.6 GHz [inferred].
  - Outputs: the 116 probe vectors (AVX2 glibc and static musl, portable glibc), the 664 helper
    vectors (glibc, static musl) and the 76 bge-m3 vectors (glibc, static musl) are
    byte-identical to a build of c57435b; the committed tree builds byte-identical binaries to
    the measured ones. `golden_int8` (e5-small worst 0.999562, bge-m3 worst 0.998485) and
    `golden` (e5-small fp32) give E6b's lines. `fuzz_long` green (1 256 s).
  - Memory (`mem_probe`, anonymous = dirty): the kernel's buffers grow by one head's values
    (64 KiB e5-small, 128 KiB bge-m3 at 512 tokens) and lose the four rows' block sums: e5-small
    10.4 → 10.5 MiB after the 116 inputs (glibc), bge-m3 26.6 → 26.7 MiB after its 76; section 6
    holds.
  - Against the ≤ 1.5× line: not reached. e5-small is at 1.8 to 2.0× (engine) and 1.9 to 2.0×
    (helper), bge-m3 at 2.2 to 2.3×. What the rest is made of: section 5.3, "E7k".
- **E10r result (2026-11-26, four machines rented by the hour, the pilot host for reference):
  the engine on other CPUs.** Binaries of commit 4286ee0 (E7k final, static musl), format-4 weight
  files, one thread, ONNX Runtime 1.28.0 and numpy 2.5.5 from pip (the pilot host's versions; the
  ONNX files downloaded on each machine and equal by SHA-256 to the pilot host's), Ubuntu 24.04,
  one run of 21 to 27 minutes per machine, every machine destroyed afterwards. Report
  `reports/e10r-bitlaunch.md` and raw summary `e10r/results/summary.txt` in the experiment
  directory. Nothing committed to the repository. Every number is **measured** unless marked
  [inferred]. Ratios compare within one machine and session only.
  - Machines (`lscpu`, `free`):

| | nibble-1024 | DigitalOcean c-2 | Vultr HF 1 vCPU | Linode g6-dedicated-2 | pilot host (E7k) |
|---|---|---|---|---|---|
| CPU | Intel Xeon Gold 6150 @ 2.70 GHz (Skylake-SP) | Intel Xeon Platinum 8168 @ 2.70 GHz (Skylake-SP) | "Intel Core Processor (Skylake, IBRS, no TSX)", model masked | AMD EPYC 7713 (Zen 3, Milan) | Intel Xeon (Skylake, IBRS) |
| vCPU | 1 | 2 (dedicated) | 1 | 2 (no SMT siblings) | 3 |
| avx2 / fma | yes / yes | yes / yes | yes / yes | yes / yes | yes / yes |
| avx512f / bw / vl | yes / yes / yes | yes / yes / yes | no / no / no | no | yes |
| avx512_vnni / avx_vnni | no / no | no / no | no / no | no / no | no |
| `sha_ni` | no | no | no | yes (also `vaes`) | no |
| RAM | 961 MiB, no swap | 3 915 MiB, no swap | 955 MiB, 3 GiB swap file (image default) | 3 915 MiB, 496 MiB swap partition | 7.8 GiB |
| virtualisation, kernel | KVM, 6.8.0-117 | KVM, 6.8.0-149 | KVM with Hyper-V enlightenments, 6.8.0-149 | KVM, 6.8.0-149 | KVM, 6.8.0-149 |
| steal during the series | ≤ 0.03 % | ≤ 0.07 % | ≤ 0.16 % | 0.00 % | ≤ 0.2 % |

  The EPYC's guest view: L1d 128 KiB, L2 1 MiB, L3 32 MiB per instance, 2.0 GHz nominal in
  `cpuinfo`. `sha_ni` is from the report's flag list; the summary's flag line lists only the
  flags the driver filters for. **No machine had VNNI, so no VNNI kernel could be tried**, and
  there is no AVX-512 kernel to measure on the two Skylake-SP parts that expose it (flags
  recorded only). The Vultr and Linode machines have AVX2 and no AVX-512.
  - **Bit identity across CPUs (7 of 7 comparisons per machine, on all four rented CPUs, against
    reference vectors made on the pilot host).** Every comparison is byte-identical (`cmp`):
    `speed_probe` AVX2 and portable on e5-small (116 inputs) against the reference vectors; the
    helper on the same 116 texts; the helper on the 664 synthetic texts; `speed_probe` AVX2 and
    portable on bge-m3 (76 inputs) and `mem_probe` on bge-m3 against the bge-m3 reference. That
    includes the CPU without AVX-512 and an AMD part. `ulei-garden-embed verify` gives the same
    content digests everywhere (e5-small `a06d5035…`, bge-m3 `60691050…`). The golden cosines
    follow from byte identity and were not re-run there (no cargo on the machines).
  - Speed, one thread, seconds per round (median). e5-small, 116 inputs: engine and helper one
    warm-up and three timed rounds, ONNX Runtime one warm-up pass and three rounds. bge-m3, 76
    inputs: the engine two rounds in one process after `drop_caches` (round 1 cold, round 2
    warm), portable and ONNX Runtime one timed round each (ONNX Runtime after a warm-up pass):

| | nibble-1024 | c-2 | Vultr HF | g6-dedicated-2 | pilot host, E7k session |
|---|---|---|---|---|---|
| e5 engine, AVX2 | 9.42 | 7.68 | 7.18 (rounds 6.19, 6.27, 7.37, 7.18) | 5.83 | 8.54 |
| e5 engine, portable | 75.66 | 66.15 | 60.02 | 59.86 | not timed |
| e5 helper (static musl) | 9.56 | 7.98 | 6.31 | 5.85 | 9.25 |
| e5 ONNX Runtime | 4.88 | 3.58 | 4.51 | 3.66 | 4.77 |
| e5 engine / ONNX Runtime | 1.93 | 2.15 | 1.59 (1.39 on its best rounds) | 1.59 | 1.79 |
| e5 helper / ONNX Runtime | 1.96 | 2.23 | 1.40 | 1.60 | 1.94 |
| bge-m3 engine, AVX2, round 1 (cold) / round 2 | 71.09 / 69.39 | 61.76 / 60.52 | 57.41 / 48.32 | 47.99 / 47.13 | 70.19 |
| bge-m3 engine, portable (1 round) | 701.3 | 609.8 | 570.3 | 547.0 | not timed |
| bge-m3 ONNX Runtime | OOM-killed | 24.86 | 30.66 (swapping) | 25.80 | 32.12 |
| bge-m3 engine (round 2) / ONNX Runtime | n/a | 2.43 | 1.58 | 1.83 | 2.19 |

  The pilot host's column is the E7k session of 2026-11-25, not re-run (the host was shared). By
  length, e5-small engine / ONNX Runtime (4–16, 17–64, 65–192, 512 tokens): nibble-1024 1.61 /
  1.68 / 1.97 / 1.98; c-2 1.86 / 1.93 / 2.16 / 2.33; Vultr HF 1.39 / 1.41 / 1.66 / 1.38;
  g6-dedicated-2 1.60 / 1.56 / 1.58 / 1.63; pilot host 1.58 / 1.68 / 2.06 / 2.06.
  - **The engine / ONNX Runtime ratio depends on the hardware**: 1.4 to 2.4× over these machines
    (e5-small engine 1.59 to 2.15, helper 1.40 to 2.23, bge-m3 1.58 to 2.43). On the EPYC it is
    flat over the input length (1.56 to 1.63); on the two Skylake-SP Xeons and the pilot host it
    grows with length (c-2 1.86 → 2.33). ONNX Runtime gains more than the engine from the
    dedicated Platinum 8168 (the ratio is highest there); the cause is not measured. The
    absolute engine time also varies 1.6× over the five CPUs (e5-small 5.83 to 9.42 s) for the
    same inputs: a ratio to ONNX Runtime moves with the hardware (section 5.3).
  - Vultr HF's engine rounds drift (6.2 → 7.4 s with 0 % steal; the helper's rounds, run after
    them, are steady at 6.3 s); the cause is not measured. The cold first bge-m3 round costs
    +1.7 s (nibble-1024), +1.2 s (c-2), +9.1 s (Vultr HF) and +0.9 s (EPYC) against the warm one
    (567 major faults on each: the about 340 MiB of the file that the 76 inputs touch).
  - **The portable kernel is 8 to 12× slower than AVX2** on one thread (e5-small 8.0 to 10.3×,
    bge-m3 10.0 to 11.8×, from the medians above; the report's note says 8 to 10× for e5-small
    and 9.9 to 12.6× for bge-m3, and the 12.6 does not come out of the table's numbers).
    **Limitation: a CPU without AVX2 runs the portable kernel and is impractical for indexing
    today** (the 116 e5-small inputs take 60 to 76 s here against 6 to 9 s; one bge-m3 round of
    76 inputs 9 to 12 minutes against about a minute). It is the bit-exact reference, not a
    fallback to ship for x86.
  - **bge-m3 on a 1 GB machine: the product claim.**

| | nibble-1024 (961 MiB, no swap) | Vultr HF (955 MiB, 3 GiB swap) | c-2 (3.9 GiB) | g6-dedicated-2 (3.9 GiB) |
|---|---|---|---|---|
| engine `speed_probe`, AVX2 | runs, both rounds, vectors identical | runs, identical | runs, identical | runs, identical |
| engine peak RSS / of it anonymous (dirty) | 354 MiB / 25.5 MiB | 354 / 25.5 MiB | 354 / 25.5 MiB | 354 / 25.5 MiB |
| `MemAvailable` minimum during the engine run | 665 MiB | 638 MiB | 3 534 MiB | not recorded in the report |
| `mem_probe`: dirty after 76 inputs / after release | 26 168 / 976 KiB (mapping 336 MiB clean) | 26 172 / 980 KiB | 26 172 / 980 KiB | 26 172 / 980 KiB |
| ONNX Runtime bge-m3 | **OOM-killed after 3 s**, in session creation (`anon-rss` 776 560 kB); global OOM, nothing else killed, the machine stayed usable | ran: peak anonymous 709 MiB and 23 MiB in swap, `MemAvailable` down to 25 MiB, 38 278 pages swapped out (about 150 MiB of other processes), 6 392 major faults, 30.66 s | ran: peak anonymous 729 MiB, maxrss 846 MiB | ran: peak anonymous 730 MiB, maxrss 847 MiB, no swap used |

  ONNX Runtime needs about 730 to 780 MiB of anonymous memory for bge-m3 (e5-small: maxrss 256 to
  257 MiB, anonymous 213 to 216 MiB). On the 1 GB machine without swap the engine runs bge-m3
  with 26 MiB of dirty memory and 665 MiB still available, and ONNX Runtime is killed before its
  first inference; with swap (the other 1 GB machine's image ships a swap file) ONNX Runtime
  completes by pushing about 150 MiB of everything else out and leaving 25 MiB available.
  **This is the measurement behind the claim that the engine fits where ONNX Runtime does not.**
  The e5-small helper holds 9.7 MiB dirty after the 116 texts, RSS about 56 MiB, on every
  machine. On the 1 GB machine without swap the page cache could not keep whole weight files
  beside the machine's other use: `verify` of the bge-m3 file re-read 268 MiB from disk on every
  warm run; inference itself needs only the touched pages (about 340 MiB for bge-m3), which
  stayed resident.
  - Start-up and `verify`, medians (e5-small 5 runs, bge-m3 3 runs; cold = `posix_fadvise(DONTNEED)`
    before each run, as at E6c; `verify` as CPU seconds, user + sys, because the sampler's 0.5 s
    poll quantised the wall time):

| | nibble-1024 | c-2 | Vultr HF | g6-dedicated-2 | pilot host (E6c) |
|---|---|---|---|---|---|
| e5 ready, warm / cold | 2.9 / 7.4 ms | 2.5 / 8.7 ms | 2.0 / 5.2 ms | 2.4 / 7.6 ms | 3.3 / 11–12 ms |
| e5 first vector (8 tokens), warm / cold | 28 / 115 ms | 25 / 161 ms | 20 / 83 ms | 25 / 136 ms | |
| bge-m3 ready, warm / cold | 4.5 / 7.8 ms | 3.6 / 10.7 ms | 2.6 / 6.7 ms | 3.7 / 8.3 ms | 3.5–4.4 / 10–12 ms |
| bge-m3 first vector, warm / cold | 230 / 710 ms | 178 / 1 044 ms | 140 / 651 ms | 175 / 816 ms | |
| `verify` e5-small, CPU s, warm / cold | 0.73 / 0.79 | 0.64 / 0.69 | 0.64 / 0.67 | 0.11 / 0.17 | |
| `verify` bge-m3, CPU s, warm / cold | 3.64 / 3.88 | 3.03 / 3.27 | 2.75 / 2.88 | 0.51 / 0.78 | |

  "Ready after start ≤ 100 ms" (section 6) holds on all four machines, warm and cold. Cold
  `verify` read the whole file on every machine. `verify` is about 6 times faster on the EPYC
  (0.11 s against 0.64 to 0.79 s for e5-small, 0.51 s against 2.75 to 3.88 s for bge-m3).
  [Inferred: the digest is SHA-256 and only the EPYC has the `sha_ni` flag; the three Intel
  machines have not. Not tested by a build without the instructions.]
  - Idle: the helper after the 664 synthetic texts (one round 47.7 s on nibble-1024, 39.2 s c-2,
    32.5 s Vultr HF, 29.4 s g6-dedicated-2), then 120 s: 0 voluntary and 0 involuntary context
    switches on every machine; dirty (`Private_Dirty`) 4.0 MiB before and after, RSS 66 to 69
    MiB; the query after the idle took 16 / 12 / 15 / 12 ms.
  - Not done: the helper's speed on bge-m3 and ONNX Runtime's idle behaviour; glibc builds
    (static musl only); `golden_int8` (the cosines follow from the byte identity); bge-m3
    portable and ONNX Runtime have one timed round each.
- **E11m result (2026-11-26, Apple M3 Pro, commit 4286ee0): the first NEON run on hardware and a
  check on real texts.** The owner asked that all of it be fixed here. Report
  `mac-2026-11-26/results.md` in the experiment directory; the local patch beside it,
  `neon-default-local.patch`, makes `Kernel::best()` return `Neon` on `aarch64` and was reverted
  after the run (it is not in the repository). A clean checkout, no patch needed to build or run
  `ulei-infer`; the kit's `SHA256SUMS` 651 of 651 OK. Every number is **measured** unless marked
  [inferred].
  - Hardware and conditions: Apple M3 Pro, 6 performance and 6 efficiency cores, 16 MiB L2 per
    performance cluster, 18 GiB; `FEAT_DotProd` 1, `FEAT_I8MM` 1, `FEAT_BF16` 1; macOS 27.2;
    rustc 1.100.0 (LLVM 23.1.5), host `aarch64-apple-darwin`; release profile (`ulei-infer` at
    opt-level 3, fat LTO, one codegen unit, native target, no `-C` flags; the long fuzz in the
    documented `CARGO_PROFILE_TEST_OPT_LEVEL=3` test profile); Python 3.12, torch 2.15.0,
    transformers 5.24.1 (the golden manifests' versions), numpy 2.5.5; on mains, Low Power Mode
    off. **The machine was not idle**: a work laptop with an endpoint-security process using 11
    to 92 % of one core during the runs, a browser and the window server; load average 2.8 to 4.9
    on 12 cores. During the first timing pass another job (a test suite) ran too; every timing was
    taken twice, with and without it, the passes agree within 1 to 4 %, the tables show the
    second.
  - **NEON is bit-identical on real hardware.**
    - Probe vectors, `neon` and `portable` against the Linux AVX2 reference (`cmp`): e5-small, 116
      inputs, 5 rounds, byte-identical in both and against each other; bge-m3, 76 inputs, 2
      rounds, the same; repeated in three (e5-small) and two (bge-m3) separate runs.
    - Text in (`Encoder::embed_text_with`, NEON) over the 664 synthetic texts equals the Linux
      static-musl helper's vectors byte for byte: tokenizer, prefixes and pooling agree across
      architectures.
    - Golden tests (`--release`, one test thread), both kernels bit-identical: e5-small worst
      pooled cosine 0.999562, median 0.999772; bge-m3 worst 0.998485, median 0.998836 (the
      numbers of sections 4 and 4.1 to the last digit; golden test time 15.0 s portable / 10.2 s
      NEON for e5-small, 172.5 s / 104.7 s for bge-m3).
    - Fuzz, NEON against portable: the default run (3 000 block-sum shapes over both NEON paths
      = 6 000 kernel cases, 1 000 linear layers, quantisation 2 000, attention 300, softmax 3 000,
      GELU and exp strided) equal; `fuzz_long` (406.5 s): **4 000 000 block-sum cases** (2 000 000
      shapes over both paths, SDOT and the widening path forced on a `dotprod` CPU) and
      **200 000 linear-layer cases** (full `f32` output, SDOT), no mismatch.
    - `cargo test -p ulei-infer --release`: 39 passed, 3 ignored. With the local patch and
      `--all-features`: 68 + 1 + 5 + 2 passed, 0 failed, five ignored (`fuzz_long`, run
      separately; the two long `ops` sweeps, which involve no kernel; the two tokenizer parity
      tests, whose reference cases are not in the kit).
  - **What NEON covers.** On `aarch64` NEON implements only the integer block sums of
    `linear_i8`. `attention`, `gelu`, `quantize_rows` and softmax have no NEON variant and run the
    portable code; their long fuzz cases (attention 20 000, quantisation 200 000, softmax 200 000,
    GELU and `exp` over every `f32`) passed but check the portable code against its reference
    loop only and say nothing about NEON. Attention is 46 % of a NEON layer at 512 tokens (17 %
    at 128). **`Kernel::available()` is true for `Neon` on every `aarch64`, but `Kernel::best()`
    returns AVX2 or portable and never selects NEON**: without the patch every test that names no
    kernel (encoder unit tests, the fp32 golden comparison) runs `portable` on the Mac.
  - Speed, one thread, seconds per round (sum of per-input milliseconds as `speed_probe` prints
    them), pass 2; PyTorch fp32 is `AutoModel`, eager, `torch.set_num_threads(1)`,
    `inference_mode`, batch 1, fed the probe's token ids, pooling as in the weight file,
    L2-normalised (process user time not above wall time, so one thread):

| | e5-small, 116 inputs | bge-m3, 76 inputs |
|---|---|---|
| engine NEON | 7.25 (5 rounds, 7.13–7.42) | 56.56 (2 rounds, 55.71–57.42) |
| engine portable | 19.55 (5 rounds, 19.29–20.02) | 168.09 (2 rounds, 168.06–168.11) |
| PyTorch fp32, 1 thread | 1.19 (5 rounds, 1.09–1.53) | 8.74 (2 rounds, 7.64–9.84) |
| pass 1 (with the other job): NEON / portable / PyTorch | 7.12 / 19.61 / 1.22 | 57.03 / 172.03 / 8.16 |

  The PyTorch vectors against the reference: e5-small min cosine 0.999563, bge-m3 0.998112 (the
  int8 against fp32 gap). Milliseconds per input by length (median over the inputs of each
  input's median over rounds; the probe has no inputs of 193 to 511 tokens; first input of round
  0, cold: e5-small 34 ms, bge-m3 404 ms):

| e5-small, tokens (inputs) | NEON | portable | PyTorch fp32 | portable / NEON | PyTorch / NEON |
|---|---|---|---|---|---|
| ≤ 16 (32) | 9.9 | 31.2 | 4.2 | 3.14 | 0.43 |
| 17–64 (33) | 12.7 | 40.3 | 4.4 | 3.17 | 0.35 |
| 65–192 (49) | 103.4 | 286.6 | 14.7 | 2.77 | 0.14 |
| 512 (2) | 616.9 | 1 353.4 | 61.6 | 2.19 | 0.10 |

| bge-m3, tokens (inputs) | NEON | portable | PyTorch fp32 | portable / NEON | PyTorch / NEON |
|---|---|---|---|---|---|
| ≤ 16 (27) | 102.0 | 303.6 | 53.1 | 2.98 | 0.52 |
| 17–64 (12) | 155.9 | 490.6 | 55.2 | 3.15 | 0.35 |
| 65–192 (35) | 1 173.4 | 3 581.0 | 137.6 | 3.05 | 0.12 |
| 512 (2) | 5 615.9 | 15 330.1 | 475.3 | 2.73 | 0.08 |

  - NEON is 2.2 to 3.2× faster than `portable` on this Mac; the gain shrinks with length because
    attention stays portable.
  - **Single-thread PyTorch fp32 is 2 to 12× faster than the int8 NEON engine on this Mac**
    (0.43 down to 0.08 of the engine's time per input). The cause is [inferred]: macOS PyTorch
    runs fp32 matrix products through Accelerate, which uses the AMX matrix unit; not verified.
    **ONNX Runtime was not timed on the Mac**, so no ratio to it exists for Apple Silicon.
  - Where the time goes (`op_probe 384 1536 12 16,64,128,512 15 KERNEL`, one e5-small-shaped
    layer, median of 15, milliseconds per layer; the last column is the total times 12 layers):

| tokens | kernel | 4 linear | attention | GELU | quantise ×4 | LN + residual | total | × 12 |
|---|---|---|---|---|---|---|---|---|
| 16 | NEON | 0.82 | 0.03 | 0.03 | 0.02 | 0.02 | 0.92 | 11 |
| 16 | portable | 2.77 | 0.03 | 0.03 | 0.02 | 0.02 | 2.87 | 34 |
| 64 | NEON | 3.34 | 0.38 | 0.12 | 0.06 | 0.09 | 3.99 | 48 |
| 64 | portable | 11.07 | 0.40 | 0.12 | 0.06 | 0.09 | 11.74 | 141 |
| 128 | NEON | 6.70 | 1.45 | 0.24 | 0.12 | 0.18 | 8.69 | 104 |
| 128 | portable | 21.80 | 1.41 | 0.23 | 0.12 | 0.17 | 23.74 | 285 |
| 512 | NEON | 26.34 | 24.59 | 0.94 | 0.48 | 0.72 | 53.08 | 637 |
| 512 | portable | 88.04 | 24.38 | 0.93 | 0.48 | 0.71 | 114.54 | 1 375 |

  The × 12 column matches `speed_probe` (128 tokens 104 against 103 ms, 512 tokens 637 against
  617 ms). NEON speeds up the linear layers 3.3 to 3.4×, to about 34·10⁹ multiply-adds per
  second at every length against about 10·10⁹ portable [inferred from the table]; that is
  roughly 13 % of one P-core's `SDOT` peak if it issues four 128-bit `SDOT` per cycle [inferred;
  the issue rate is an assumption].
  - Memory, peak RSS (`/usr/bin/time -l`): e5-small 51 478 528 bytes (49 MiB) for both kernels;
    bge-m3 356 712 448 (340 MiB) NEON and 353 796 096 (337 MiB) portable; the PyTorch probe, its
    Python runtime included, 510 394 368 (487 MiB) and 1 652 310 016 (1.54 GiB). RSS counts the
    touched mapped pages; most of each file (127 MB and 603 MB) is the token table, of which a
    probe touches few rows [inferred].
  - **Workspace on macOS** (`cargo check --workspace --all-targets --exclude xtask
    --keep-going`; not investigated further): `ulei-infer` (all features: builds and runs
    unchanged, no warnings), `ulei-contracts`, `ulei-hive`, `ulei-glue` and
    `ulei-connector-mcp-example` build. `ulei-core` fails through the `landlock` crate
    (`libc::prctl`, `PR_SET_NO_NEW_PRIVS`, `O_PATH`, `SYS_landlock_*`). The other failures are
    `rustix` items macOS lacks: `sockopt::socket_peercred` (`SO_PEERCRED`) in the garden's,
    memory's and engine's `serve`, in `runtime-adapters` and in `channels`' C6 client, and the
    socket flags `SocketFlags::CLOEXEC` and `NONBLOCK`, `SendFlags::NOSIGNAL` and
    `RecvFlags::CMSG_CLOEXEC` in the engine's gateway, `channels`, `runtime-adapters` and
    `connector-test`. `ulei-garden --features native`, the one user of `ulei-infer`, fails only on
    `socket_peercred`.
  - **Real texts (local, aggregates only).** The corpus is the owner's local notes and own typed
    messages; nothing else about it is recorded here. Paragraphs capped at 400 tokens (e5
    tokenizer; longer ones split at sentence ends, a few single sentences above the cap kept whole
    and truncated at 512 by both pipelines), lines under 40 characters dropped, exact duplicates
    removed: **4 671 passages**. Language by Cyrillic share of letters (ru ≥ 0.8, en ≤ 0.2,
    otherwise mixed): ru 1 960, mixed 2 168, en 540, other 3. Length: ≤ 64 tokens 1 754, 65–192
    1 500, 193–400 1 281, > 400 136; mean 153 tokens. Two proxy query sets of 1 000, random
    samples: "first" (the first sentence of a passage of at least two sentences; ru 441, mixed 524,
    en 35) and "mid" (a random inner sentence of a passage of at least three sentences; ru 300,
    mixed 597, en 103); the relevant document is the source passage, and the query text lies
    inside it. fp32 is PyTorch `AutoModel` with the HF tokenizer, pooling and prefixes as in the
    weight file's header; int8 is `Encoder::embed_text_with`, NEON, through a throwaway example
    that is not committed. Retrieval: each engine's queries against its own passages, cosine over
    all 4 671 passages; the 95 % interval is a paired bootstrap over queries (2 000 resamples)
    of the mean int8 − fp32. Harness check on synthetic data first: the harness's fp32 against the
    kit's PyTorch dumps, cosine 1.000000 on all 16 golden cases (e5-small max abs diff 2.2e-7,
    bge-m3 3.6e-7); the engine's text-in vectors on the golden texts min 0.999563 / median
    0.999772 (e5-small) and 0.998485 / 0.998836 (bge-m3); the 664 synthetic vectors
    byte-identical to the Linux ones.
  - **e5-small on real texts.** Pooled cosine int8 against fp32, same text:

| set | n | min | p1 | median |
|---|---|---|---|---|
| passages, all | 4 671 | 0.999632 | 0.999731 | 0.999814 |
| queries "first" | 1 000 | 0.999448 | 0.999701 | 0.999811 |
| queries "mid" | 1 000 | 0.999666 | 0.999732 | 0.999804 |

  Passages by language and by length have min 0.999632 to 0.999745 and median 0.999788 to
  0.999817. **0 of 6 671 texts are below 0.999**, e5-small's golden per-case bound (lowest
  0.999448). Retrieval, overall rows (1 000 queries over 4 671 passages):

| model, queries | metric | fp32 | int8 | int8 − fp32 | 95 % interval |
|---|---|---|---|---|---|
| e5-small, first | R@1 | 0.8310 | 0.8360 | +0.0050 | [+0.0010, +0.0100] |
| e5-small, first | R@3 | 0.9010 | 0.9010 | +0.0000 | [−0.0050, +0.0060] |
| e5-small, first | R@10 | 0.9380 | 0.9370 | −0.0010 | [−0.0050, +0.0020] |
| e5-small, first | MRR | 0.8710 | 0.8742 | +0.0032 | [+0.0009, +0.0057] |
| e5-small, mid | R@1 | 0.5920 | 0.6000 | +0.0080 | [+0.0010, +0.0150] |
| e5-small, mid | R@3 | 0.7250 | 0.7280 | +0.0030 | [+0.0000, +0.0070] |
| e5-small, mid | R@10 | 0.8030 | 0.7980 | −0.0050 | [−0.0120, +0.0010] |
| e5-small, mid | MRR | 0.6713 | 0.6759 | +0.0046 | [+0.0011, +0.0084] |
| bge-m3, first | R@1 | 0.9070 | 0.9070 | +0.0000 | [−0.0050, +0.0050] |
| bge-m3, first | R@3 | 0.9540 | 0.9530 | −0.0010 | [−0.0040, +0.0020] |
| bge-m3, first | R@10 | 0.9740 | 0.9750 | +0.0010 | [−0.0020, +0.0040] |
| bge-m3, first | MRR | 0.9327 | 0.9331 | +0.0004 | [−0.0023, +0.0032] |
| bge-m3, mid | R@1 | 0.5550 | 0.5540 | −0.0010 | [−0.0100, +0.0080] |
| bge-m3, mid | R@3 | 0.7170 | 0.7170 | +0.0000 | [−0.0050, +0.0050] |
| bge-m3, mid | R@10 | 0.8170 | 0.8200 | +0.0030 | [−0.0030, +0.0090] |
| bge-m3, mid | MRR | 0.6514 | 0.6507 | −0.0007 | [−0.0055, +0.0039] |

  - e5-small: top-10 overlap (mean Jaccard) 0.9193 for "first" (min 0.54) and 0.9151 for "mid"
    (min 0.54); top-1 differs on 1.30 % (13 of 1 000) and 2.90 % (29) of queries. Counted by
    argmax, one exact score tie being counted differently, the flips are 12 ("first": 5 involve
    the relevant passage, all 5 with int8 right) and 29 ("mid": 12 involve it, 10 with int8 right
    and 2 with fp32 right); the rest are swaps between two wrong passages. No recall loss at 95 %
    confidence overall: int8 is slightly ahead on R@1 (+0.5 and +0.8 points, the intervals exclude
    0) and on MRR; R@10's intervals contain 0.
  - **bge-m3 on real texts.** Pooled cosine int8 against fp32:

| set | n | min | p1 | median |
|---|---|---|---|---|
| passages, all | 4 671 | 0.995676 | 0.997563 | 0.998556 |
| queries "first" | 1 000 | 0.996460 | 0.998023 | 0.998755 |
| queries "mid" | 1 000 | 0.996859 | 0.997729 | 0.998789 |

  By language the passages' minima are 0.995676 (mixed), 0.995861 (en) and 0.996042 (ru); by
  length the lowest minimum is 0.995676 (193–400 tokens). **14 of the 6 671 texts are below 0.997,
  the golden test's bound (11 of 4 671 passages: ru 5, mixed 5, en 1; 3 of 2 000 queries);
  the lowest is 0.995676.** 282 passages are below 0.998. The median over all 6 671 texts is
  0.998629, above the median bound of 0.998. So on real input 0.2 % of the texts fall below the
  minimum bound while the median holds and the overall retrieval is unchanged; see open
  question 7. The bit-identical result does not depend on this: the same 14 texts score the same
  on AVX2. Retrieval: top-10 overlap 0.9093 ("first", min 0.67) and 0.8971 ("mid", min 0.54);
  top-1 differs on 1.20 % (12 of 1 000) and 5.80 % (58). Of the 58 "mid" flips 21 involve the
  relevant passage (10 int8 right, 11 fp32 right), and 17 are swaps between two passages with
  cosine at least 0.98 to each other (near-duplicates). The 17.48 % top-1 change on the English
  "mid" queries is 18 queries, only 2 of which involve the relevant passage; the swapped pairs
  have a median cosine of 0.997 to each other (near-identical short messages). For "first": 12
  flips, 6 involving the relevant passage (3 each way).
  - **Subgroups, and why not to read much into them.** The report gives each model, query set,
    language group (3) and length group (4) separately: about 200 comparisons in all (2 models
    × 2 query sets × 7 groups × 4 metrics), of which about 10 would exclude 0 at 95 % by chance
    alone, and no correction for multiple comparisons was applied. The ones that exclude 0 point
    both ways: against int8, e5-small ru "mid" R@10 −1.7 points and bge-m3 ru "mid" R@1 −1.3
    points and MRR −0.9 points; for int8, e5-small R@1 in the 193–400-token group (+1.5 points
    "first", +1.2 "mid") and bge-m3 en "mid" MRR (+1.3 points). No consistent sign that int8
    ranks worse than fp32 on these texts. **A thin effect on Russian "mid" queries for bge-m3 is
    the one item worth re-checking on a larger query set** (300 queries in the group, its R@1
    interval about 2.3 points wide).
  - Throughput on this corpus, wall time including model load, informational: 8 single-thread
    engine processes against PyTorch with 8 threads at batch 32; e5-small engine 108 s and
    PyTorch 61 s for the 4 671 passages, bge-m3 1 233 s and 501 s. The 8 processes spread over
    the 6 + 6 cores, so it is not a per-core comparison.
  - What E11m settles and what it does not. The "no hardware, no kernel" rule of section 5 is
    met for the Apple M3 Pro (bit-equality, golden and probe vectors). Still untested: Linux
    `aarch64` (Graviton, Ampere), an `aarch64` CPU without `dotprod` running natively (only the
    forced widening path ran here), `i8mm`. The M3 Pro reports `FEAT_I8MM` 1 (open question 4).
    The owner's decision on enabling NEON is open question 17 (resolved); the rest of the Apple
    work is deferred (section 5.4).
- **Owner's decisions and open items after E10r and E11m (2026-11-26).** Recorded where they
  belong: the platform statement in section 1 ("Target platforms"); the backlog "Apple Silicon,
  after the release" in section 5.4; the observation that a ratio to ONNX Runtime depends on the
  hardware in section 5.3 and in the notes of section 6; open question 17 (NEON the default on
  `aarch64`, resolved) and the updated open questions 4 and 7 (the bge-m3 bound for real inputs
  is a proposal the owner has not decided).
- **Why PyTorch fp32, not ONNX Runtime, is the reference.** The published int8 export on ONNX
  Runtime 1.32.0 gives pooled cosine against fp32 of min 0.9913 / median 0.9960 on 664 texts,
  and the same range up to 512 tokens (min 0.9927). Two ONNX Runtime versions (1.28.0 and
  1.32.0, pip wheels) disagree with each other down to cosine 0.9967 on the same model and token
  ids. An ONNX Runtime vector is neither exact nor reproducible across versions, so no criterion
  is stated against it.
- **Why not ±0.5 points of recall.** On the 170-question synthetic set one independent
  question is 0.59 points, and the paired 95 % intervals are 1 to 6 points wide (1 to 3 for the
  simulated int8 schemes, about 6 for ONNX Runtime int8). A ±0.5 claim needs about 10× the
  questions (open question 6).

### 4.1 bge-m3 through E1 to E4 (2026-11-25, pilot host)

Measured on the pilot host (kernel 6.8.0-149) with another job on it (1 to 1.6 vCPU), on the code of the e5-small
results above. **No engine or converter change was needed**: `bert::Config` reads positions from 2
(`pad_token_id + 1`), eps 1e-5, 24 layers, 16 heads, FFN 4096 and one token type from `config.json`; the pooling
(CLS) comes from the dumps' manifest and the file header.

- **Checkpoint.** `pytorch_model.bin` (SHA-256 `b5e0ce34…6aad38`) was loaded with `weights_only=True` only and
  written as `model.safetensors` (SHA-256 `993b2248…e38e7e`); all 391 tensors are equal after the round trip.
- **E0.** Dumps of the 16 e5-small cases without prefixes, every layer of every case (178 MB).
- **E1.**
  - Pooled vectors: 1 − cos ≤ 1.6·10⁻¹² on all 16 cases.
  - Layers: 12 of 16 cases stay within 1e-4. The other 4 do not, from layer 11 on: worst 8.9·10⁻⁴ (512 tokens,
    layer 19).
  - The bound sits below PyTorch fp32's own error at this depth. On 6 cases against a float64 forward pass, PyTorch
    fp32 is off by up to 1.0·10⁻³ and the Rust path by up to 7.0·10⁻⁴, closer at every layer.
  - The error is flat at about 1e-5 through layer 9, then grows to 2·10⁻⁴ – 9·10⁻⁴; hidden states reach |x| = 30.
  - The bound for 24 layers (a float64 reference, or 1e-3) is the owner's call. The test is unchanged and fails on
    those 4 cases.
- **E3.**
  - Token order: e5-small and bge-m3 map all 250 002 pieces to the same ids. The order built the same way from
    bge-m3's tokenizer is the e5 order's first 250 002 entries (the e5 table has 35 more, unseen rows).
  - Weight file (B = 32): 602 605 520 bytes (574.7 MiB) against "≤ 580 MB", over in 10⁶ bytes and under in MiB
    (open question 9).
    - 32 MiB of it are the 8 194 fp32 position rows, of which 514 are reachable at 512 tokens. Writing only those
      would give 571.0·10⁶ bytes [inferred]; not done.
    - Conversion 25 s, peak RSS 2.7 GiB.
  - CLS cosine of the Rust int8 path (AVX2) against PyTorch fp32:

| Set | Min | Median | Below 0.999 | Below 0.998 |
|---|---|---|---|---|
| 16 golden cases | 0.99848 | 0.99884 | 10 | 0 |
| the simulation's 72 texts | 0.99783 | 0.99874 | 45 | 1 |
| same 72, simulation B = 32 | 0.99799 | 0.99872 | 47 | 2 |
| — of them 12 of 512 tokens (Rust) | 0.99783 | 0.99812 | 12 | 1 |
| 664 synthetic texts, no prefixes (Rust) | 0.99808 | 0.99894 | 361 | 0 |

  - Per text, Rust and the simulation's B = 32 differ by at most ±1.5·10⁻⁴: within the simulation's ±3·10⁻⁴
    jitter of section 3.
  - The proposal of open question 7 (min ≥ 0.997, median ≥ 0.998) holds on every set; 0.999 on every text does
    not. The owner took the proposal (open question 7, resolved).
  - Block weight scales would add about 0.0008 at the minimum, if the simulation carries over [inferred]. They are
    needed only for a bound above about 0.998; not built.
  - The AVX2 and portable kernels give bit-identical vectors.
  - The per-linear check against the simulation was not run (no bge-m3 dumps of it).
  - Retrieval (144 documents, 340 queries, the protocol of the e5-small recall half): fp32 R@3 92.06, MRR 85.64.
    Rust int8: ΔR@3 +0.29 [+0.00, +0.88], ΔMRR +0.12 [−0.40, +0.66].
- **E2 on the file.** The file's tokenizer gives bge-m3's `AutoTokenizer` ids on the 16 golden cases and the 664
  synthetic texts, with and without e5 prefixes (0 mismatching texts). Text in gives vectors byte-identical to
  ids in (76 inputs).
- **E4.** Release build, `ulei-infer` at opt-level 3, text in:

| After | RSS | Anonymous | Clean, weight file | Peak anonymous while embedding |
|---|---|---|---|---|
| 1 input | 298.8 MiB | 1.4 MiB | 294.8 MiB | 1.4 MiB |
| 16 golden cases | 330.3 MiB | 5.0 MiB | 322.7 MiB | 21.3 MiB (512-token inputs) |
| + 60 texts (5 914 tokens) | 345.7 MiB | 7.2 MiB | 335.9 MiB | 7.2 MiB |
| ONNX Runtime helper (ADR-0086 section 9.2) | 891 MB | all | | |

  - Against section 6: dirty memory meets "≤ 150 MB".
  - Clean memory, 335.9 MiB = 352.2·10⁶ bytes against "≤ 350 MB": under in MiB, over in 10⁶ bytes (open question
    9). The linear weights alone are 288 MiB of it, needed by every input. The whole file, 575 MiB, is the clean
    worst case [inferred].
  - Start-up with a warm page cache: bound in 4.7 to 8.0 ms, which meets "≤ 100 ms". The first vector of an
    8-token query is out 180 to 201 ms after spawn. With the file evicted: bound in 12 to 17 ms, first vector
    1.4 s. The ONNX Runtime helper takes 25.3 s to load.
- **Speed, under load, informational.** One thread, one pass over 16 golden cases and 60 texts:

| Tokens | Rust | ONNX Runtime 1.28.0 | Ratio |
|---|---|---|---|
| 4–16 | 191 ms | 94 ms | 2.0 |
| 512 | 9.4 s | 3.7 s | 2.6 |
| in total | 88.5 s | 36.4 s | 2.4 |

  - The two runs were not under the same load, so the ratios are rough.
  - Where the time goes: int8 linear 80 % (92 % up to 64 tokens), attention 13 % (31 % at 512 tokens),
    activation quantisation 4 %.
  - The time per token is 14 to 19 ms at every length. One layer's int8 weights (12 MiB) exceed a vCPU's 4 MiB of
    L2, so they are streamed once per token row [inferred].
  - That makes several rows per pass (section 5.3 (b)) the first kernel lever for bge-m3. Not done here.

### 5. Kernels per instruction set

The int8 GEMM (activations int8 × weights int8 → int32) is the hot spot. One portable
implementation and one kernel per instruction set. The best available kernel is picked once at
start by run-time feature detection. The binaries keep the baseline ISA (ADR-0086 section 2).

| Kernel | Instruction | CPUs | Status | Order |
|---|---|---|---|---|
| portable | plain Rust | all | E1 | reference for every fuzz test |
| AVX2 | `VPABSB` + `VPSIGNB` + `VPMADDUBSW`, then `VPMADDWD` by ones (from the E5 follow-up; `VPMOVSXBW` + `VPMADDWD` before) | x86_64 of the last decade, the x86 baseline | E5: enabled; fuzz-tested and measured on the pilot host (section 4) | 1: done |
| NEON SDOT | `SDOT` | Apple M1+, Neoverse N1 (Ampere), Graviton 2+ | E5: written (`SDOT`, and an exact widening path without `dotprod`), compiled for aarch64; E11m: run on the owner's Apple M3 Pro, bit-identical (section 4); **the default on `aarch64`** by the owner's decision of 2026-11-26 (open question 17), from the commit that follows this record | 1: done on the M3 Pro; Linux `aarch64` and a CPU without `dotprod` not run natively |
| AVX-512 VNNI / AVX-VNNI | `VPDPBUSD` (512 or 256 bit) | Cascade Lake, Ice Lake and later Xeons; AMD Zen 4+; Intel Alder Lake+ (AVX-VNNI) | new | 3: the largest gain over AVX2 on a large share of VPS hosts, built once a host we can rent by the hour shows the flag (card 7; E10r: none of the four rented machines nor the pilot host has it) |
| AVX-512BW without VNNI | `VPMADDWD` on 512-bit registers | Skylake-SP (the pilot host's class) | not built at E5: a 512-bit variant of a first AVX2 draft ran slower than that draft on the pilot host; the current design not yet ported | 2: testable on the pilot host; measure before enabling |
| arm64 i8mm | `SMMLA` | Graviton 3+, Neoverse V1/N2; Apple per generation, to verify | new | 4, or 2 if the owner's M3 Pro reports `hw.optional.arm.FEAT_I8MM = 1` (it does, E11m; work deferred, section 5.4) |

Rules:
- **Bit-equal.** Every kernel computes the same int32 sums as the portable one. A fuzz test over
  random shapes, including tails (k not a multiple of the vector width) and extreme values,
  proves it. The reference ran 7.4 million cases. The test runs in CI under emulation where
  possible, and on real hardware with the flag before the kernel is enabled.
- **`VPMADDUBSW`'s operand signedness (AVX2).** It multiplies unsigned by signed bytes and
  adds neighbouring pairs into `i16` with saturation. The weights' magnitudes (`VPABSB`, so
  −128 becomes an exact unsigned 128) are the unsigned operand; the activations, given the
  weights' signs (`VPSIGNB`), the signed one. That is exact when the activations are in
  [−127, 127] (their negation cannot wrap), which run-time quantisation guarantees; each
  product is then at most 128 · 127 = 16 256, a pair sum at most 32 512 < 32 767, so nothing
  saturates. Each `i16` result goes straight to `i32` (`VPMADDWD` by ones); no two are added
  in `i16`. The kernel checks per call that no activation is −128 (one pass over the
  activation bytes) and that the block size is a multiple of 4; anything else runs the
  portable kernel. The fuzz tests put every pair of extremes (±127, −128, 0) in every byte
  position of every lane.
- **VNNI's operand signedness.** `VPDPBUSD` multiplies unsigned by signed bytes; both operands
  here are signed. The kernel adds 128 to the activations (making them unsigned) and subtracts
  128 × (column sum of the weights), precomputed per output channel at load. The fuzz test
  covers the −128 edge.
- **Testable first.** Kernels are built in the order the hardware at hand can verify them: the
  pilot host (Intel Skylake: AVX2, AVX-512F/BW, no VNNI), the owner's Apple M3 Pro, then hosts
  rented by the hour (the BitLaunch account already used for the codegen stand: its hosts' CPU
  models are recorded first, card 7). A kernel whose flag no reachable host has waits.
- **No hardware, no kernel.** A kernel is enabled only after a run on a host with its flag:
  VNNI cannot be checked on the pilot host. Such runs are on rented hosts and the owner's (paid,
  ADR-0086 section 5).
- **Threads.** A query runs on one thread. Batch indexing splits GEMM rows across the threads
  the profile grants.
- **Shared with speech.** The kernels are not encoder-specific. A Rust port of the speech engine
  (ADR-0086 open question 3) reuses them: they live in `crates/infer`, module `kernels`, and the
  speech model is a second model module of the same crate (open question 1, resolved).

`unsafe` is denied workspace-wide (AGENTS.md). Kernels need intrinsics or inline assembly, and
the read-only `mmap` of the weight file (section 2) needs `unsafe`; without it the
weights would be heap copies and the memory win of section 6 is lost. So the crate lifts the
denial for two modules only: `kernels`, under the fuzz rule above, and the private module
`weights::map`. That module holds the `mmap` and `munmap` of the weight file, an anonymous
read-only mapping for files that are not on disk, the in-place view of the mapped bytes as
`f32`, `u32` and `i8`, and the `Send` / `Sync` of the mapping; it exposes only safe functions.
Each module lifts the denial by one `#[allow(unsafe_code)]` on its `mod` line, and every
`unsafe` block carries a `// SAFETY:` comment. Nothing else. This lifting is part of this
decision.

### 5.1 Fused attention, later

Attention scores (Q·Kᵀ, fp32) are the second hot spot once GEMM is fast; in the reference they
were 5 % of the profile. They get an int8 or fp32 SIMD kernel only if E5's profile shows them
above 15 %.

Done in the E5 follow-up, once the `f32` GELU had made attention 26 % of the time (52 % at
512 tokens): `kernels::attention` takes the whole multi-head attention of a sequence. The
portable version is the per-query loop (scores, softmax, weighted sum) and the reference. The
AVX2 version transposes each head's keys once, so one register holds one channel of eight
keys and one pass gives eight scores, each with the eight running sums and final sum of the
portable dot product in the same order; the weighted sums of two queries (head width 32) or
one (width 64) stay in registers over all values. Same operations in the same order, so
bit-identical (fuzz test, and the 116 probe vectors byte for byte). Measured: −6 % in total,
−14 % at 512 tokens; the weighted sum got 45 % faster, the scores 8 %, softmax not at all (a
`libm` `expf` per score). Faster scores or softmax need a different order of operations or an
own `exp`, so they change outputs. The own `exp` was done at E5c (section 3; measured in
section 5.3); the scores are not.

### 5.2 Build profile of `ulei-infer`: opt-level 3

The workspace's release profile is size-first (`opt-level = "s"`, ADR-0001 section 8). The
crate `ulei-infer` is built at opt-level 3 instead (`[profile.release.package.ulei-infer]` in
the workspace `Cargo.toml`, repeating `strip = true` as the `ulei-engine` override does). Only
this library's code is affected; the binaries that link it keep `"s"` for their own code.

Measured 2026-11-25 on the pilot host (kernel 6.8.0-149, rustc 1.100.0, target
`x86_64-unknown-linux-gnu`, fat LTO, one codegen unit, AVX2 kernel, one thread), the 16
golden inputs, median of 3 rounds after a warm-up round, total per round:

| Binary that runs the forward pass | `"s"` everywhere | `ulei-infer` at 3 | whole package at 3 |
|---|---|---|---|
| a `ulei-garden` example (the helper's situation under fat LTO) | 15.07 s | 9.92 s (1.52× faster) | 9.37 s (`ulei-garden` at 3) |
| a `ulei-infer` example (the probe) | 15.09 s | 6.27 s (2.41× faster) | same |

- Under fat LTO the linking package's opt-level still governs the final optimisation, so the
  gain in a garden binary (1.5×) is below the gain in the library's own example (2.4×).
  Setting all of `ulei-garden` to 3 gains only 6 % more and would also grow the daemon.
- Size, release gnu build with `--features onnx`: `ulei-garden-embed` 620 200 → 658 400 B
  (+38 200 B, budget 2 MiB); the daemon `ulei-garden` 2 023 096 B, unchanged. The helper
  holds only the converter of this crate today; the forward-pass binary above grows by
  37 440 B (417 928 → 455 368 B), the expected order of the E6 helper's growth.
- Outputs are bit-identical under both profiles (same 116 pooled vectors as E4).
- The rest of the speed is not the profile: E5 in section 4.
- **E6b: why the garden binary gains less [inferred, measured around it].** Under fat LTO the
  vectorisers run only at link time, at the opt-level of the binary's package (above); a binary of
  `ulei-garden` at `"s"` runs the engine's auto-vectorised loops as scalar code. The helper is 1.6×
  the probe on the same engine; without fat LTO, or with `ulei-garden` at 3, it equals the probe.
  The per-package exception for `ulei-garden` was open question 14: applied (owner's decision,
  2026-11-25), the helper now equals the probe.

### 5.3 Where the remaining speed is

**Estimates of the architect (2026-11-25), not measurements.** Starting point: the E5 follow-up
of section 4, e5-small at 2.84× ONNX Runtime on the pilot host, one thread; int8 linear 61 %,
attention 25 % (scores 11 %, softmax 9 %, weighted sum 5 %), activation quantisation 7 %.

- **Linear layers (61 %).**
  - (a) The cost of the activation block scales. B = 32 is exactly one AVX2 register, so every
    32 bytes the kernel converts the integer block sum to `f32`, multiplies it by the block's
    scale and adds it (section 3). This overhead may be comparable to the integer multiply
    itself. ONNX Runtime has one scale per tensor and does no such work. Ways out: B = 64 halves
    it (bge-m3 min pooled cosine 0.99799 → 0.99707, section 3); or reduce a row's blocks to a
    common scale with integer multipliers and convert to `f32` once per row (changes outputs and
    needs a new simulation).
  - (b) Several token rows per pass over the weights: moderate for e5-small (an FFN matrix,
    590 KB, fits L2), necessary for bge-m3 (4 MB does not).
  - (c) AVX-512BW: expected 1.3 to 1.5× on the linear layers on this Skylake.
  - (d) VNNI / SDOT: three instructions become one. A matter of hardware, not code.
- **Attention (25 %, half the time at 512 tokens).**
  - A vectorised `exp` in softmax: 9 % is spent in `libm` `expf`; it also removes the
    dependence of the vectors on the C library.
  - Scores Q·Kᵀ in `f32` (11 %): the rule "portable and AVX2 bit-identical" forbids FMA. A
    possible relaxation is bit-equality for the integer part only and a tolerance for the
    floats. That is a decision of method, to be taken only after the gain is measured.
- **Wall time, not the ratio to ONNX Runtime:** threads for batch indexing (section 5,
  "Threads"); grouping texts by length.
- **Order.** Profile first. If the `f32` combination of the blocks is a third or more of the
  linear time, the scale scheme is revisited first; otherwise AVX-512BW and several rows per
  pass.
- **Expectation:** about 1.8 to 2× ONNX Runtime on this host after the `exp`, AVX-512BW and the
  block work; 1.5× without VNNI is not expected.
- **Decision:** e5-small is not pushed further now beyond the profile and the `exp`. The kernel
  layout is revisited at bge-m3, whose matrices do not fit the cache.

**Measured afterwards (2026-11-25, pilot host, kernel 6.8.0-149), kept apart from the estimates
above.**

- **Profile.** `perf` sampling on `cpu-clock` at 4 kHz (the KVM guest has no hardware PMU, so no
  `cycles`), a build with line tables of the same code, samples attributed by source line. Share
  of all samples on the 116 probe inputs (two 512-token inputs in brackets):

| Part | Share |
|---|---|
| int8 linear | 58 % (35 %) |
| — integer multiply-accumulate (`VPABSB`, `VPSIGNB`, `VPMADDUBSW`, `VPMADDWD`, `VPADDD`, loads) | 32 % (21 %) |
| — per block: `i32` sums stored, converted to `f32`, scaled, added | 10 % (7 %) |
| — per block loop, weight tile packing, driver | 12 % (5 %) |
| — per row: weight scale, bias, call | 4 % (3 %) |
| attention: scores / softmax (`libm` `expf` 5.5 %, the rest 3.3 %) / weighted sum | 9 / 9 / 5 % (23 / 17 / 10 %) |
| activation quantisation | 7.5 % (4.6 %) |
| `libc` memset and memmove (zeroing new buffers) | 4 % (3 %) |
| LayerNorm, GELU | 2 %, 2 % |

- **Bound on the per-block `f32` work.** A build that keeps one integer sum per row and output and
  combines once per row (wrong outputs, timing only, not committed) runs the 116 inputs in
  11.12 s instead of 13.44 s: **at most 30 % of the linear time** (36 % at 512 tokens; the
  sampling alone gives 18 %). Below a third overall, so by the order rule above **AVX-512BW and
  several rows per pass come first**. At 512 tokens alone the bound is just over a third; left
  to the architect.
- **The `exp`: done** (section 3, "Softmax of the int8 path"). Softmax 8.7 % → 4.9 % of the time
  (16.9 % → 7.8 % at 512 tokens), no `libm` call left in the int8 path. Golden min pooled cosine
  0.999562, unchanged. Speed in one session, three alternating passes per build against ONNX
  Runtime 1.28.0, release build as committed:

| Tokens (inputs) | before | own `exp` | ONNX Runtime 1.28.0 | before / ORT | own `exp` / ORT |
|---|---|---|---|---|---|
| 4–16 (32) | 19.8 ms | 21.1 ms | 11.7 ms | 1.69 | 1.80 |
| 17–64 (33) | 24.0 ms | 25.6 ms | 13.6 ms | 1.77 | 1.89 |
| 65–192 (49) | 173 ms | 176 ms | 69.7 ms | 2.48 | 2.52 |
| 512 (2) | 1 114 ms | 1 061 ms | 408 ms | 2.73 | 2.60 |
| all 116, per round | 12.70 s | 12.75 s | 5.30 s | 2.40 | 2.41 |

  - In total the change is lost in the noise, −5 % at 512 tokens, +6 % on short inputs.
  - With LLVM's `-x86-branches-within-32B-boundaries` on both builds (the JCC-erratum diagnostic of
    section 4, not a proposal) the same change is −7 % in total, −15 % at 512 tokens and −2 to
    −4 % on short inputs. The short-input loss above is code placement [inferred].
  - This session's ORT ratio for the build before the change is 2.40, against 2.84 in the E5
    follow-up's session: ORT measured 5.30 s against 4.64 s then, the engine 12.7 s against 13.2 s.
    Ratios compare within one session only.
- **glibc and musl.** The probe built for `x86_64-unknown-linux-musl` writes the 116 vectors
  byte-identical to the glibc build, before and after the change. After it, the int8 path calls
  no `libm` function: `sqrt` in LayerNorm and L2 normalisation is the exact instruction. `exp` and
  `expf` are left only on the fp32 reference path.
- **musl speed (E6b, section 4).** The static musl build ran 33 % slower than glibc on the same code:
  large buffers mapped and unmapped per operation, and 89 million 4-byte `memcpy` calls per round
  from the kernel's quad packing, both slow in musl. The buffer pool and a fixed-size quad load
  bring it to 1.05× glibc (engine alone) and 0.99× (helper).
- Not measured: AVX-512BW, several rows per pass, the scale scheme, threads.

**E7k (2026-11-25): several rows per pass, measured** (section 4, E7k result).

- (b) is done and was not what bounded the speed: the kernel already read each packed weight
  tile once per four rows, and more rows, wider tiles or cache blocking over the input changed
  little (best 0.79 to 0.95 of the old kernel per layer shape). The gain came from combining the
  block sums in registers instead of storing them (the 10 % "per block" and 4 % "per row" of the
  profile above), from quantising with intrinsics and from the attention layout. In total
  e5-small 2.61 → 1.99× ONNX Runtime (engine, glibc; static musl helper 2.52 → 1.94×), bge-m3
  2.87 → 2.31×, outputs unchanged.
- Remaining, measured (`op_probe`, share of a layer after E7k): linear 86 % (16 tokens) to 50 %
  (512 tokens) for e5-small, 93 to 72 % for bge-m3; attention 2 % to 40 % (e5-small), 2 to 22 %
  (bge-m3); quantisation 2 to 6 %; GELU, residuals and LayerNorm 3 to 7 % together.
- What could still close the gap to 1.5× [inferred, not measured]:
  - (c) AVX-512BW: the same core at twice the width. ADR estimate 1.3 to 1.5× on the linear
    layers; an earlier 512-bit draft ran slower, and 512-bit code lowers this CPU's clock. With
    the linear share above, 1.5× on e5-small needs about 1.5× on the linear layers.
  - (d) VNNI or SDOT on hosts that have them: one instruction instead of four per quad.
  - Attention at 512 tokens: the scores and weighted sums run at 8 multiply-adds per cycle at
    most without fused multiply-add, which the rule "portable and AVX2 bit-identical" forbids; a
    relaxed rule would roughly double them. Softmax is a third (e5-small) to a sixth (bge-m3) of
    attention at 512 tokens (an unlocked, rough measurement); an explicit vector form of the same
    formula is open.
  - Short inputs: the linear layers run at 23·10⁹ multiply-adds per second at 16 tokens against
    31·10⁹ at 512; the packing of every weight tile per call and the weights' stream from memory
    are the likely rest. Packed weights in the file would remove the packing (a format change).
- What cannot be closed under the current rules on this host [inferred]: ONNX Runtime's AVX2
  int8 core multiplies unsigned activations by signed weights in three instructions and accepts
  `VPMADDUBSW`'s saturation; the exact signed path here needs the fourth (`VPSIGNB`), about a
  third more work per multiply-add, plus the per-block `f32` combination of the B = 32 scheme
  (at most 30 % of the linear time while the sums were stored, E5 follow-up; now in
  registers). On AVX2 without VNNI, 1.5× for e5-small needs every other lever above.

**Parked (owner's decision, 2026-11-26).** Work towards the "≤ 1.5× ONNX Runtime" line of
section 6 is deferred: the number stays in the table unchanged, and the line is revisited later,
possibly replaced by a constant. Until then the ideas below are a backlog, not a plan; none is
started without its own measurement first. Ordered by expected gain for the cost and by how few
rules each touches. "Measured" names the measurement a figure comes from; "inferred" is
arithmetic on measured shares, not observed.

| # | Idea | Expected gain | Cost | Rule it touches |
|---|---|---|---|---|
| 1 | Explicit vector form of the softmax in attention (the same `exp_f32` formula, the same order of the eight running sums) | Softmax is about a third of e5-small's attention at 512 tokens, about 12 % of a layer (measured, rough and unlocked, E7k); a vector form at 2 to 4× gives 6 to 9 % at 512 tokens, nothing below 64 tokens (inferred) | Small: one kernel and its fuzz test against `ops::softmax_f32` | None: the same operations in the same order, so bit-identical |
| 2 | AVX-512BW kernel: the AVX2 core at twice the width | 1.3 to 1.5× on the linear layers (the ADR's estimate, inferred), which are 50 to 93 % of a layer (measured, E7k); a 512-bit draft of an earlier AVX2 design ran slower than that design (measured, E5), and 512-bit code lowers this CPU's clock | Moderate: a kernel and its fuzz test; testable on the pilot host | None (a bit-equal kernel); "measure before enabling" (section 5) |
| 3 | Parallel indexing over several cores: several passages at once, each on its own thread with its own `bert::Workspace` over the one mapped `Encoder`; the forward pass stays single-threaded | Indexing wall time divided by up to the number of cores granted; a passage's own latency unchanged (inferred; a second thread over the same `Encoder` is the first step, open question 16) | Moderate: the helper's indexing side takes several passages at once, the worker sends several | Section 6 dirty memory: one workspace per thread (9.2 MiB e5-small, 24.4 MiB bge-m3 at 512 tokens, measured E6b); section 5 "Threads" (written as GEMM rows split across threads) and the thread grant of the profile |
| 4 | Weights stored pre-packed in the file (the kernel's tile layout), for short inputs | At 16 tokens the linear layers run at 23·10⁹ multiply-adds per second against 31·10⁹ at 512 (measured, E7k): at most about 25 % of the linear time, about 20 % of a 16-token query, if the packing is all of the difference (inferred; the packing and the weights' stream from memory were not separated) | Large: a new weight format version, the converter, a layout per instruction set (or one file per layout), every model converted again | Section 2 (the format); the content digest changes, so every stored vector is embedded again although the numbers stay the same |
| 5 | VNNI (`VPDPBUSD`) and SDOT hosts | One instruction instead of four per quad in the integer core (inferred: the core runs at 60 to 80 % of its port bound, E7k); the largest single gain where the flag exists | A kernel each (SDOT is written, never run) and a host with the flag (card 7, the owner's M3 Pro) | "No hardware, no kernel" (section 5) |
| 6 | Fused multiply-add in attention (scores and weighted sums) | About 2× on the scores and weighted sums (inferred: at most 8 multiply-adds per cycle without FMA), about two thirds of attention at 512 tokens: about 13 % of an e5-small layer at 512 tokens, nothing for short inputs (inferred) | Small in code | A decision of method: breaks "portable and AVX2 bit-identical" (the vectors would differ by host); needs a tolerance rule instead of byte equality |
| 7 | Ranges constrained so that `VPMADDUBSW` cannot saturate (for example weights in [−63, 63] times unsigned activations, with a column-sum correction as for VNNI), so the extra instruction for exactness (`VPSIGNB`) goes | Up to a quarter of the integer core's instructions, roughly 10 to 15 % of the linear time (inferred) | Moderate: converter, kernels, a new simulation and the golden runs | Section 2 (weights in [−127, 127]) and the AVX2 exactness rule of section 5; outputs change, so a new model id. **The accuracy cost is unknown**: no simulation of 7-bit weights has been run |
| 8 | Activation block B = 64 (or one integer common scale per row, section 5.3 (a)) | Halves the per-block `f32` combination, now about 3 instructions per 32 integer ones, in registers: a few per cent of the linear time (inferred) | Small: B is a header field, only a reconversion | Accuracy: bge-m3 min pooled cosine 0.99799 → 0.99707 in simulation (measured, section 3), just above its 0.997 bound; outputs and the model id change |

- **A ratio to ONNX Runtime is hardware-dependent (E10r, 2026-11-26).** On four rented CPUs
  and the pilot host the e5-small engine / ONNX Runtime ratio ran from 1.59 to 2.15 and the
  bge-m3 one from 1.58 to 2.43 (section 4, E10r); on the AMD EPYC it did not grow with the input
  length, on the Intel Xeons it did. The same engine meets or misses a "× ONNX Runtime" line
  depending on the machine, so when the parked line is revisited an absolute budget (seconds for
  a fixed input set on a named reference host) is the better-posed form. The number 1.5× in
  section 6 is not changed.
- Not on the list: grouping texts by length (the helper embeds one text per call, nothing is
  padded) and the activation quantisation (2 to 6 % of a layer after E7k, measured).

### 5.4 Apple Silicon, after the release

Owner's statement of 2026-11-26 (section 1, "Target platforms"): macOS on Apple Silicon is a
target as a small always-on server box, not first priority; the work below is **deferred until
after the release** and is a backlog, not a plan. Known for each item from E11m (section 4):

- **NEON as the default kernel is not in this backlog: it is decided and done before the
  release** (open question 17): `Kernel::best()` selects it on `aarch64` when available, from the
  commit that follows this record. What stays here is its untested hardware: Linux `aarch64`
  (Graviton, Ampere) and an `aarch64` CPU without `dotprod` running natively (only the forced
  widening path ran on the M3 Pro); both would be run with `kernels::fuzz`, `fuzz_long` and the
  golden test.
- **A NEON attention** under the "same operations in the same order" rule of section 5.1.
  Attention is portable on `aarch64` and is 46 % of a NEON layer at 512 tokens (17 % at 128), so
  it would help long inputs most; the NEON gain over portable falls from 3.2× to 2.2× with length.
- **Higher NEON linear throughput.** About 34·10⁹ multiply-adds per second, flat over length,
  roughly 13 % of a P-core's `SDOT` peak [inferred]. Ways: more token rows per weight load, as
  in E7k on AVX2; `i8mm` (`SMMLA`, the M3 Pro reports `FEAT_I8MM` 1). Bit-equality is safe
  because the block sums are exact integers.
- **Against Accelerate / AMX-backed fp32, and whether the engine should use Accelerate on
  macOS.** Single-thread PyTorch fp32 is 2 to 12× faster than the int8 NEON engine on the M3 Pro
  (cause [inferred]: Accelerate, AMX). The comparison that matters for a target is int8 NEON
  against a CPU runtime without AMX, or against the engine calling Accelerate; the engine's
  bit-equality rule would not hold across such a path.
- **ONNX Runtime on the Mac** was not timed; the ratio of section 6 has no Apple value.
- **Building the workspace on macOS.** Only `ulei-infer`, `ulei-contracts`, `ulei-hive`,
  `ulei-glue` and `ulei-connector-mcp-example` build. The rest fails on Linux-only APIs:
  `SO_PEERCRED` (`socket_peercred`), `landlock`, socket flags (section 4, E11m).
  `ulei-garden --features native` fails only on `socket_peercred`.
- **Idle and energy behaviour on macOS** is not measured (E11m ran on a busy work laptop).

### 6. Budgets (targets; replaced by E4 to E6 measurements before acceptance)

| Model | Weight file | Dirty RSS while embedding | Clean RSS while embedding | RSS idle after 10 min (no pressure) | Ready after start | Passage embed time vs ONNX Runtime on the same host |
|---|---|---|---|---|---|---|
| e5-small | ≤ 130 MB (owner's decision, open question 9; E6c: 127.29·10⁶ bytes with the tokenizer section) | ≤ 60 MB (E6: 3.3 MiB; E6b: 10.4 MiB, the buffers kept; E7k: 10.5 MiB; section 9, a 512-token passage and a 512-token query at once: 18.9 MiB) | ≤ 100 MB (E6: 53.0 MiB) | clean pages may stay; dirty ≤ 20 MB (E6: 3.3 MiB, 0 wake-ups in 600 s; E6b: 9.8 MiB; section 9, both lanes' buffers at their most kept: 11.6 MiB, 0 wake-ups in 600 s) | ≤ 100 ms (E6: bound in about 10 ms, ready line after 0.78 s, the identity hash; E6c: ready line 3.3 ms warm, 11 to 12 ms cold, section 4) | ≤ 1.5× (provisional, owner's decision; measured 2.84× after the E5 follow-up, section 4; 2.41× after E5c in a later session, section 5.3; 3.52× in the helper at E6, under load; E6b, quiet host: 2.31× the engine alone, 3.60× the static musl helper, section 4; `ulei-garden` at opt-level 3, open question 14: helper 2.22× glibc, 2.38× static musl; E7k, one session: engine 1.99× glibc, 1.79× static musl, helper 2.04× / 1.94×) |
| bge-m3 | ≤ 580 MB | ≤ 150 MB (E6b: 26.6 MiB; E7k: 26.7 MiB; section 9, two 512-token inputs at once: 49.6 MiB) | ≤ 350 MB | as above (E6b: the 24.4 MiB of buffers released after a long input; section 9, both lanes' buffers at their most kept: 18.3 MiB = 19.2 MB) | ≤ 100 ms (before E6c: 3.5 to 4.6 s, the identity hash; E6c: 3.5 to 4.4 ms warm, 10 to 12 ms cold) | ≤ 1.5× (E6b: 2.5 to 2.8×, engine alone; E7k: 2.31× glibc, 2.19× static musl) |

The speed row bounds the regression: the engine is not chosen for speed (ADR-0086 section 9.4),
but indexing time is a profile cost.

- "Ready after start" is the time from process start until the model is bound and able to take
  input, with a warm page cache. The time to the first vector is recorded beside it (E4: 2.3 to
  3.9 ms ready, about 250 ms to the first vector of an 8-token query before E5).
- The clean-RSS targets assume the frequency-ordered token table of section 2. Resident table
  pages are larger than the rows read by the kernel's 64 KiB fault-around (section 2); the
  clean figure is measured, not derived from the rows.
- "RSS idle after 10 min" is measured on the E6 helper, not at E4.
- Every speed figure names its build: profile, opt-level, target and any `-C` flags.
- The weight file column is in 10⁶ bytes. e5-small's row is ≤ 130 MB with the tokenizer
  section in the file (owner's decision, open question 9; the format-4 file is 127 294 800
  bytes = 127.3 MB). bge-m3's row is unchanged and its unit and size are still open (open
  question 9).
- The "≤ 1.5× ONNX Runtime" speed column is provisional: the reference Go engine ran 2.5×
  slower on this host's AVX2 path (RTF 0.86) than on the Apple SDOT path (RTF 0.34). E5
  measured e5-small at 5.1× ONNX Runtime on the pilot host, the E5 follow-up 2.84× (section
  4). Owner's decision (2026-11-25): the column stays "≤ 1.5×" for now and is revisited after
  the multi-row linear kernel (section 5.3 (b)) and a further speed iteration, when it may be
  replaced by a constant; the number is not changed.
  Owner's decision (2026-11-26): work towards the line is parked after E7k; the number stays
  unchanged and the line is revisited later, possibly replaced by a constant. The ideas for
  further speed are the backlog at the end of section 5.3. E10r adds that the ratio itself
  depends on the hardware (1.4 to 2.4× over five CPUs, end of section 5.3), an argument for an
  absolute budget when the line is revisited.

### 7. Not decided here

- Removing ONNX Runtime from the build (E7, its own ADR).
- The speech-to-text port (ADR-0086 open question 3).
- Rerankers on the same engine.
- Inputs longer than 512 tokens.

### 8. The garden and a corrupted weight file (open question 15; amends ADR-0047 section 2, C9 0.11.0)

Since E6c the helper's start reads only the preamble, the length and the header of the weight
file, so a changed blob byte is seen only by hashing the whole file (`ulei-garden-embed verify`).

- **Who hashes: the helper, as a child of the daemon.** With the `native` backend (the model
  directory holds `model.ulei-infer`) the daemon's worker runs `<garden.embedder.helper> verify
  <model_dir>/model.ulei-infer` (`embed::check`), with a deadline of 300 s after which the child
  is killed. The daemon links no weight-format code and adds no buffer of its own: its dirty
  memory and binary size do not depend on the file (measured below). The child reads with
  `pread` into a small buffer, so the file enters the page cache as clean, evictable pages and
  not the daemon's or the child's dirty memory. The ONNX backend has no check (its start hashes
  its files).
- **How "corrupted" is told apart: exit codes and a refusal class, never message text.** `verify`
  exits 0 with `OK FILE: content sha256 <hex>, ...`, 3 for a corrupted file (digest or length
  mismatch, malformed preamble, header or tensor table, never sealed), 1 for an older format
  ("convert the model again") or an unreadable file, 2 for bad usage. A helper that refuses its
  start on a corrupted file writes `{"error": <reason>, "class": "corrupted"}` and exits 3; the
  daemon reads it as `StartError::Corrupt`.
- **When.** At the worker's first tick after the start (after the ready line and the bind, so
  start-up is unchanged), then every `garden.schedule.verify_interval` seconds (C9 0.11.0,
  default 86 400), and at once when a restart of the helper is refused as corrupted. After a
  check that does not pass (corrupted, unusable, or no result) the next one comes 1 minute later,
  doubling up to 1 hour, never past the interval (review E13r, finding 3). The check is
  one more deadline of the worker's sleep (ADR-0047 section 6): no timer, no polling, no wake-up
  beyond its own run. It runs on the worker thread, so no passage is in flight while a verdict is
  applied; queries keep using the helper meanwhile.
- **Schema (ADR-0044, migration 16).** `fact_vectors.computed_at` (INTEGER NOT NULL DEFAULT 0;
  the vectors stored before the migration are 0, "before any check", trusted; a copy of a
  version keeps its vector's time) and the one-row table `model_checks` (start and digest of the
  last passed check; start and reason of a failed one since).
- **States** (in the supervisor, `embed::supervisor`; the last verdict persisted in
  `model_checks`):

| State | Entered by | Helper | `health` | `search`, `pointers` | Background embedding |
|---|---|---|---|---|---|
| ok | a passed check (the store records its start and digest) | ready, restarted after a fall with the backoff of ADR-0047 | `dense: ready` | hybrid | runs |
| corrupted | a corrupted verdict; a restart refused as corrupted (then checked at once); at start, a refusal as corrupted or a failed check on record | stopped, never restarted (`Due::Off`) | `dense: down`, `dense_error` "model file corrupted: FILE: ..." | lexical, `lexical_only: true` | paused |
| recovering | a passed check while corrupted | restarted at once, first pause again | `down` until the ready line, then `ready` | lexical, then hybrid | the facts whose vectors were dropped |

  On the corrupted verdict the store records the failure and deletes the vectors computed at or
  after the start of the last passed check (all models; every vector with a time when no check
  ever passed) in one unit of work; their facts are without a vector again. The rest of the index
  stays. Two stderr lines: `garden: embedder: down: <reason>` and the count dropped.
- **Decisions made from the dropped vectors (review E13r, finding 2).** The same unit of work
  undoes the reconciliation of new facts (`Edge::reconcile_similar`) those vectors fed, so the
  owner keeps no item the broken model made and a contradiction it missed is weighed again:
  - an open `contradiction` item that reconciliation wrote (it names the reconciled fact in
    `edge_proposal.fact_id`; a contradiction the edge finds when it decides a candidate does
    not) at or after the start of the last passed check, whose fact or rival loses its vector, is
    withdrawn: status `withdrawn`, out of the digest, and `resolve` answers it `not_open`. An item
    the owner already settled stays settled; one from before that start stays open;
  - the facts of the withdrawn items, and every current fact that loses its vector and that the
    edge queued for reconciliation when it accepted it (an `accepted` proposal row without a
    reason, no attribute, not secret), wait in `reconcile_queue` again. A withdrawn item's fact
    keeps the item's candidate; any other gets a candidate made from the fact (its text, entity
    id, source, times, lifetime type, confidence, domain, privacy), because the queue drops a
    candidate once its fact is reconciled. After recovery the worker re-embeds them and then
    reconciles them as new facts.
  - Not covered without a schema change: a fact reconciled in the window from its own, kept
    vector against a rival whose vector is dropped, when that weighing wrote no item (a missed
    contradiction) and the rival is not itself a queued fact. The store does not record when a
    fact was reconciled, so such a fact cannot be told from one reconciled before the window.
    Covering it needs the time of each reconciliation, for example a column
    `reconcile_queue.done_at` with the row kept instead of deleted (the queue then reads only
    rows without it), so that a failed check re-queues every fact with `done_at` at or after the
    start of the last passed check. Conversely, a fact reconciled under an earlier model whose
    vector of the current model was computed in the window is reconciled once more, which can
    repeat an item still open from that earlier reconciliation.
- **Recovery and the identity path.** A passed check while corrupted ends the hold. Same digest:
  the same model id, the kept vectors stay and only the dropped facts are embedded again. Another
  digest (the operator put another model there): the restarted helper names another model id
  and the existing path of ADR-0047 section 6 (`drop_other_vectors`) re-embeds every fact. A
  passed check whose digest differs from the ready helper's identity (a file renamed over the one
  the helper maps) restarts the helper on the new file, so the same path applies at once instead
  of at the next fall or start.
- **Start (amends ADR-0047 section 2 and the C9 description of `garden.embedder.enabled`).**
  With the `native` backend the daemon starts lexical-only, `dense: down` with the reason and one
  stderr line `garden: embedder: starting lexical-only, dense down until the model file verifies
  again: <reason>`, when the helper refuses with the class `corrupted`, or when `model_checks`
  records a failure and no passed check since (the helper that started is stopped before the
  bind). The worker's check at its first tick then looks for recovery. Every other refusal stops
  the start as before: a missing weight file, an older format, a bad config, any ONNX backend
  failure, and a `corrupted` refusal without the engine's file in the model directory (no check
  could end it).
- **Residual window: at most one period.** Facts embedded between a corruption on disk and the
  check that finds it get vectors from the broken model (up to `verify_interval`, 24 h by
  default); that check drops them and they are embedded again after recovery. Vectors stored
  before the migration count as checked, and a vector computed in the same second as a passed
  check's start is dropped with the later ones (conservative). A daemon restart runs the check
  again at once, so a restart does not extend the window. A check without a result is retried
  within minutes (finding 3), so it does not double the window either.
- **Measured** (pilot host, kernel 6.8.0-149, 3 vCPU Skylake, 7.9 GiB; release gnu builds at
  `opt-level` 3; another agent's embedding job used about one vCPU throughout, load average 2.0
  to 3.3, steal 0 to 1 %; the weight files' page-cache state was not controlled):

| | e5-small (127.3 MB) | bge-m3 (602.6 MB) |
|---|---|---|
| ready line after start | 19 ms | 19 ms |
| check wall time (child spawn to exit, sampled every 5 ms) | 0.90 s | 4.08 s (one run) |
| daemon dirty memory before → during the check | 828 → 912 KiB | 764 → 912 KiB |
| `verify` child: dirty peak / RSS | 1.5 MiB / 4.1 MiB | 1.8 MiB / 4.3 MiB |
| idle after the check, 300 s: context switches of the daemon / the helper | 0 / 0 | not run |

  Daemon binary (gnu, same target directory, before c57435b → after): 2 325 304 → 2 275 640 B
  (−49 664 B, text −49.7 KB; the shrink was not analysed, likely LTO inlining); helper 735 360 →
  735 728 B. The static musl daemon does not link on the pilot host; CI's budgets job measures it.

### 9. A query lane in the helper (open question 16; amends ADR-0047 section 2)

**Before (facts, garden at section 8).**

- The daemon serves C7 connections one at a time on its main thread (`serve::Server::run`); the
  worker is the one other thread (ADR-0047 section 6). Both use one `embed::pipe::Helper`
  through the supervisor, whose slot lock is held only to hand out the helper's `Arc`.
- `search` and `pointers` embed one query each (`embed_query`, one text). The worker sends one
  passage per tick, one text per call, and none within `worker::QUIET` (2 s) of the last `search`
  or `pointers`, so a burst of queries does not alternate with passages.
- `Helper` held one pipe: a `Mutex` around the channel pair to one thread that owned the
  helper's stdin and stdout. A call took that pair out of the mutex for its whole exchange (the
  lock itself is not held while the call waits); a query counted itself as waiting, and a
  passage did not take the pipe while any query waited (a condition variable). The helper read
  one line, embedded it on its one thread and answered: strict request and reply.
- So a query waited for at most one passage in flight (the remainder of it, up to the whole
  passage) and then for its own forward pass, never for a second passage; a second query could
  not be waiting at the same time, since the request path is one thread. The 5 s deadline
  (`pipe::CALL_DEADLINE`) counted from the send of each call, not from the wait for the pipe.
- Measured below: a 16-token e5-small query waited 409 ms (median) behind a stream of 512-token
  passages, against 16 to 18 ms alone; ADR-0047 section 3b gives `pointers` 150 ms.

**Design (approved by the owner; details here).** Two threads in the one helper process over
the one mapped `Encoder` (`Sync`; the engine's API is unchanged): a query lane and an indexing
lane, each with its own `bert::Workspace`. The engine stays single-threaded per forward pass.
The operating system does the preemption: the indexing thread runs at a lower priority, so on a
single core (or with the other cores busy) a query gets nearly the whole CPU and the passage
resumes afterwards.

- **Protocol: lanes on the existing pipe, chosen over a second channel.** A second pipe needs a
  third inherited descriptor, which `std::process::Command` cannot pass without `pre_exec`
  (`unsafe`) or clearing `FD_CLOEXEC` on a descriptor that every concurrently spawned child (the
  `verify` check of section 8) would then inherit too. On the one pipe:
  - The ready line gains `"lanes": 2` (`pipe::LANES`). Old daemons ignore the key.
  - With lanes, the daemon puts `"id": <u64>` into every request; `"kind": "query"` runs on the
    query lane (the helper's main thread, which also reads stdin), `"kind": "passage"` on the
    indexing lane (a thread of its own, `garden-embed-index`). Every answer header repeats its
    request's `id`; each lane answers its requests in order, the two lanes' answers interleave,
    each written whole under one lock.
  - A request without an `id` is answered in place as before, so a lanes helper still serves an
    old daemon (the pipe test of the native backend sends exactly that).
  - The daemon uses lanes only with a helper that announces them, and then sends at most one
    request per lane at a time; a writer thread owns the helper's stdin and a reader thread its
    stdout and hands each answer to the call whose `id` it carries. With any other helper (the
    ONNX backend, which keeps `pipe::serve` unchanged, or an older helper binary) the daemon keeps
    the single exchange above.
  - `native` and `fake` announce lanes; `onnx` does not (its embedder is not shared across
    threads and was not restructured).
- **Faults.** An answer for no call in flight, an answer of another shape than its call, a closed
  or failed pipe, or a missed deadline ends the pipe: the call that met it kills and reaps the
  helper, every call in flight on either lane fails (`EmbedError::Run`), and every later call
  fails as down; the supervisor's restart, backoff, `health` and the corrupted-file states of
  section 8 are unchanged (they act on the `Helper` as a whole). A refusal (`{"id", "error"}`)
  fails only its own call and the pipe stays in step.
- **Deadlines.** The query lane keeps `pipe::CALL_DEADLINE` (5 s). The indexing lane gets
  `pipe::PASSAGE_DEADLINE` (300 s, as the check of section 8): at its lower priority a passage
  may wait for the CPU while other work runs, and a hung helper is still found within 5 s by the
  next query. Without lanes every call keeps 5 s.
- **Priority: the indexing thread raises its own nice value by 10** (`pipe::INDEX_NICE`, at most
  to 19), with `rustix::process::getpriority_process` / `setpriority_process(None, ...)`: safe
  API of a dependency the crate already has (feature `process`), no privilege (raising one's own
  nice value is always allowed), no `unsafe`, the `linux_raw` backend in the static musl build
  too. On Linux the nice value of `PRIO_PROCESS` with pid 0 is the calling thread's own, so the
  query lane keeps the helper's (a test reads both threads' values from `/proc`). Elsewhere it
  would be the whole process's, so nothing is done there. `SCHED_IDLE` and `SCHED_BATCH` have no
  safe call in `rustix` 1.1 (no `sched_setscheduler`); they were measured by setting them from
  outside with `chrt` on the thread. Why 10 and not 19 (measured below): on one core a query
  gets 18.4 ms at 19 and 20.2 ms at 10 (35 ms at equal priority), but next to one busy process at
  nice 0 on the same core a 512-token e5-small passage takes about 60 s at 19 (0.017 passages/s)
  against 8.5 s at 10 (0.117/s) and 1.5 s at 0: at 19 a 512-token bge-m3 passage (7.5 s alone)
  would take about 7.5 min and miss `PASSAGE_DEADLINE`, be counted as failed and after three
  falls skipped. `SCHED_IDLE` starves the same way; `SCHED_BATCH` buys nothing for queries.
- **Memory (section 6).** Two workspaces over the one mapping: the indexing lane keeps its
  buffers up to `native::KEEP_BYTES` (16 MiB) as before; the query lane releases its buffers
  after a call that left them above `native::QUERY_KEEP_BYTES` (2 MiB). The buffers grow
  linearly, 18.9 KB per token for e5-small and 50.2 KB for bge-m3 (a fresh workspace after one
  input: 0.32 MB / 0.84 MB at 16 tokens, 1.22 / 3.25 MB at 64, 9.69 / 25.7 MB at 512), so every
  query up to about 110 tokens (e5-small) or 41 tokens (bge-m3) keeps them and a longer one
  allocates them again and gives them back; 16 MiB + 2 MiB plus the helper's own memory stays
  under the 20 MB idle row in the worst kept state, measured below.
- **Idle.** Two threads blocked in `read` and `recv` (the helper), a writer and a reader thread
  blocked in `recv` and `read` (the daemon): no timer, no polling.
- **Outputs.** The engine and its arithmetic are unchanged; the vectors are the bytes of the
  single-lane helper (below).

**Measured** (2026-11-26, pilot host: KVM, 3 vCPU Intel Xeon Skylake (IBRS), AVX2 and AVX-512
without VNNI, 7.8 GiB, kernel 6.8.0-149-generic). Helper builds: release as committed (fat LTO,
`ulei-garden` and `ulei-infer` at opt-level 3, no `-C` flags), static musl (the shipped build)
and gnu as control; "before" is the branch of section 8 rebased on E7k (the same engine), "after"
this section. Driver: an uncommitted example linking the daemon's own `pipe::Helper` (so the old
helper gets the daemon's single exchange with a waiting query first, the new one its lanes),
pinned to the third vCPU when the helper is pinned to one or two; the helper pinned with
`taskset`. A continuous stream of 512-token synthetic passages runs on the indexing side; a query
goes out 100 to 500 ms after the previous answer ("uniform phase": 100 to 900 ms for e5-small,
100 to 7 700 ms for bge-m3, longer than a passage). The driver does not pause passages after a
query as the worker does (`worker::QUIET`): in the daemon a query within 2 s of the previous one
meets no passage at all, so the rows are the wait of the first query of a burst. Quiet host
before every run (other load 0.12 to 0.43 vCPU over 10 s); during a run 1.1 to 1.3 busy vCPU
(our process and the rest), up to 1.6 when the two lanes compute at once, steal ≤ 0.75 %. The
"nice +19", "equal", `SCHED_IDLE` and `SCHED_BATCH` rows are an experiment build of the same code
whose increment came from the environment, `SCHED_*` set with `chrt` on the thread.

e5-small, 16-token query (`query: ` and specials included), 200 queries per row, ms:

| Helper cores | before: median / p95 / max | after (nice +10): median / p95 / max | nice +19 | equal priority |
|---|---|---|---|---|
| 1 | 409 / 620 / 753 | 21.3 / 33.2 / 37.8 | 18.4 / 30.0 / 111 | 35.1 / 51.2 / 62.3 |
| 1, uniform phase | 411 / 721 / 832 | 20.5 / 30.7 / 123 | | |
| 2 | 422 / 684 / 816 | 23.2 / 43.6 / 80.3 | 19.7 / 35.4 / 78.9 | 20.8 / 33.3 / 43.3 |
| 3 | 428 / 648 / 769 | 20.0 / 31.3 / 48.2 | 19.4 / 30.1 / 75.2 | 20.0 / 32.1 / 46.8 |
| 1, gnu | 454 / 688 / 813 | 21.4 / 32.4 / 35.1 | 20.4 / 31.1 / 35.6 | |

- One core, other priorities (16-token query, experiment build): nice +5 23.5 / 35.6 / 42.6;
  `SCHED_IDLE` 18.4 / 28.6 / 35.2; `SCHED_BATCH` 34.9 / 55.6 / 61.9 (no better than equal).
- A query alone takes 16 to 18 ms (E7k): with lanes a query on one core costs about 3 ms more
  than alone, at equal priority twice as much.

e5-small, 512-token query (200 queries per row; 100 for the priority variants), ms:

| Helper cores | before | after (nice +10) | nice +19 | equal priority |
|---|---|---|---|---|
| 1 | 1 183 / 1 441 / 2 125 | 811 / 984 / 1 369 | 734 / 838 / 1 255 | 1 152 / 1 470 / 1 754 |
| 2 | 1 105 / 1 351 / 1 690 | | 730 / 877 / 1 128 | 733 / 892 / 1 317 |
| 3 | 1 124 / 1 478 / 1 909 | | 800 / 1 003 / 1 219 | 745 / 900 / 1 196 |
| 1, gnu | 1 165 / 1 496 / 1 944 | | 789 / 908 / 1 039 | |

- One core, `SCHED_IDLE`: 724 / 836 / 1 019. With two or three cores the two forward passes run
  side by side and slow each other (a passage 1.06 to 1.13 s instead of 0.73 s, shared caches
  and memory bandwidth [inferred]); the priority then changes nothing.

bge-m3 (one core unless named; 16-token query = 13 tokens, bge-m3 has no prefix), ms:

| Row | before | after (nice +10) | nice +19 | equal priority |
|---|---|---|---|---|
| 13-token query, 60 queries | 7 429 / 8 688 / 8 870 | 192 / 289 / 344 | 189 / 227 / 294 | 381 / 471 / 530 |
| same, uniform phase, 40 queries | 4 007 / 7 758 / 9 039 | (taken under load, discarded) | | |
| same, 3 cores | 7 094 / 8 429 / 8 883 | | 196 / 260 / 274 | |
| 512-token query, 12 queries | 13 868 / 15 877 / 15 877 | 7 836 / 9 373 / 9 373 | 7 030 / 8 043 / 8 043 | |

- A 512-token bge-m3 passage takes 7.2 to 7.7 s alone on this host, a 512-token query 7.0 s:
  both above `CALL_DEADLINE` (5 s). Before this section the daemon therefore killed the helper on
  every bge-m3 passage of about 350 tokens or more [inferred from about 14.6 ms per token]; with lanes
  a passage has 300 s. A bge-m3 query that long still misses 5 s (open question 18).

Indexing throughput (512-token passages per second; e5-small 60 s per row, bge-m3 120 s, so a
bge-m3 row is ±1 passage = ±7 %), queries of 16 (13) tokens at a fixed rate:

| Row | before | after, nice +19 build |
|---|---|---|
| e5-small, 1 core, no queries | 1.283 | 1.367 |
| e5-small, 1 core, 1 query/s | 1.300 (+1 %) | 1.333 (−2 %) |
| e5-small, 1 core, 10 queries/s | 1.217 (−5 %; only 356 of 600 queries served, each waits for a passage) | 1.100 (−20 %; 600 served, 17.9 ms median) |
| e5-small, 3 cores, no queries | 1.367 | 1.350 |
| e5-small, 3 cores, 1 query/s | 1.317 (−4 %) | 1.383 (+2 %) |
| e5-small, 3 cores, 10 queries/s | 1.083 (−21 %) | 1.417 (+5 %) |
| bge-m3, 1 core, no queries | 0.133 | 0.142 |
| bge-m3, 1 core, 1 query/s | 0.125 (−6 %) | 0.108 (−24 %) |

- On one core the loss is the CPU the queries take (10 × 18 ms = 18 % of the core; bge-m3
  1 × 190 ms = 19 %); with a free core it is none. The same rows of the final build (nice +10)
  were taken under the owner's load (2.9 to 3.0 busy vCPU) and are not used.
- Next to a busy process at nice 0 on the same core (e5-small, 60 s, quiet otherwise): nice +19
  0.017 passages/s, +10 0.117/s, equal 0.650/s.

Memory (static musl helper, `Private_Dirty` = anonymous, KiB; sampled every 5 ms; taken while
the owner's other load ran, which does not change these figures):

| | e5-small | bge-m3 |
|---|---|---|
| after start | 184 | 260 |
| peak: three times a 512-token passage and a 512-token query at once | 19 340 (18.9 MiB) | 50 752 (49.6 MiB) |
| after them (the query lane's buffers released, the passages' kept or released) | 9 764 | 344 |
| then a 16-token query | 10 112 | 1 068 |
| most kept: a passage just under `KEEP_BYTES` (e5-small: 510 tokens; bge-m3: 330), then a query just under `QUERY_KEEP_BYTES` (110; 41 tokens) | 11 848 (11.6 MiB) | 18 720 (18.3 MiB = 19.2 MB) |
| idle after that, 600 s (e5-small) / 300 s (bge-m3) | 11 848, 0 context switches of either thread | 12 596, 0 context switches; the drop is pages swapped out under the owner's load (the host's swap was 2.7 GiB in use), not a release |
| before this section, same sequence: peak / most kept | 9 748 / 9 708 | 25 504 / 16 556 |

Bit identity and stress (e5-small, the 664 synthetic texts, each sent as its own kind):
- sequential and with both kinds at once (a thread per lane), static musl and gnu, before and
  after: all byte-identical to E7k's helper vectors (MD5 `a6fb130c…`);
- stress: 1 500 rounds of a random passage and a random query at once with a random offset of up
  to 3 ms, twice (musl, gnu), every vector compared with the sequential one: 6 000 vectors, 0
  differ.

Size (release, same target directory): helper static musl 836 344 → 918 376 B (+82 032), gnu
741 520 → 809 920 B (+68 400), budget 2 MiB; daemon gnu 2 275 640 → 2 309 432 B (+33 792). The
static musl daemon does not link on the pilot host; CI's budgets job measures it.

### 10. E12a — assurance (no human review before the release; evidence instead)

The owner releases this engine without a human code review, so its correctness rests on checks
that would fail if the code were wrong. E12a puts them into CI (the repository is public:
GitHub-hosted runners are free, including `ubuntu-24.04-arm` and `macos-15`):

- `ci.yml` job `aarch64` (every PR): clippy of the workspace on aarch64 (the NEON kernel was never
  compiled by any job before), `ulei-infer`'s tests with NEON picked by `Kernel::best()`, the MSRV
  check of `ulei-infer` on aarch64.
- `encoder.yml` (PRs touching the engine or the garden): the real e5-small, from public sources
  only: the checkpoint from Hugging Face at a pinned revision, converted in the job to the content
  digest every measurement above used (`ci/encoder_model.py`); the PyTorch fp32 golden dumps and
  the tokenizer parity cases generated in the job with the E0 versions of torch and transformers
  (`ci/encoder_golden.py`, `ci/requirements-encoder-ref.txt`); the small reference files (token
  orders, probe inputs and their vectors, synthetic texts and their vectors, the golden cases'
  texts) committed in `ci/encoder-ref/`. On x86_64, Linux aarch64 and Apple M1: the golden int8
  test, tokenizer parity, every kernel's probe vectors byte for byte, and on Linux the static
  helper's vectors of the 664 synthetic texts.
- `assurance.yml` (manual, weekly, never required): cargo-mutants over `ulei-infer` and the garden
  lines changed since `dc33d0d`, survivors re-tested with the golden and parity tests; cargo-fuzz
  targets in `fuzz/` (its own workspace, not a member: the weight file, the tokenizer, the content
  digest, `verify`, both sides of the helper pipe); ASan, TSan and Miri (one pinned nightly, used
  only by these jobs; everything in the workspace builds on stable and the MSRV); the garden's
  end-to-end scenarios on the static musl binaries (`ci/garden_e2e.py`); bge-m3 on request.
- Outside CI: a 24 h soak and the timing and memory run on a rented 1 GB / 1 vCPU machine.

**What ran (2026-11-26).** Details, run ids and raw numbers: the work directory's
`reports/e12a-assurance.md`.

- **NEON on Linux aarch64, first run**: clippy of the workspace and `ulei-infer`'s tests with NEON
  as the default on `ubuntu-24.04-arm` (asimddp, i8mm, sve2); the golden int8 test there and on
  Apple M1 gives worst cosine 0.999562, median 0.999772, as on x86_64; every kernel's 116 probe
  vectors and the helper's 664 synthetic vectors are byte-identical to the kit's references on
  both ARM hosts. The conversion from the pinned Hugging Face revision reproduces the content
  digest `a06d503f…` byte for byte.
- **Findings fixed** (each a red test first, then the fix): F1 `weights::map::cast` relied on
  `align_to` putting everything in the middle, which its documentation does not promise (now
  `align_offset` and `from_raw_parts`); F2 the daemon read the helper's lines without a bound
  (+261 MB for a 256 MiB line; now 64 KiB); F3 a helper SIGKILLed from outside counted toward
  `PASSAGE_TRIES` and could skip a fact for good (now `Down`, not counted); F4 a wall clock stepped
  back after a passed check kept vectors of a possibly corrupted file (`computed_at` is now never
  before the last passed check's start); F5 a model file replaced by an older format or removed
  under a running helper left `health` ready (a verdict of its own, `Unusable`, takes dense down
  with the reason and keeps the vectors). Mutation survivors that were real gaps got tests.
- **Independent review E13r, fixed** (each a red test first, then the fix): a call that missed
  its deadline handed the pipe on before the helper was marked broken, so with one call at a
  time the next caller took the late answer as its own and the worker could store a query's
  vector under a fact (now the helper is marked broken before the pipe or lane is handed on, in
  both exchanges); with lanes, a fault that ends the pipe while a query and a passage are both in
  flight was charged to the passage (now `Down` for it, not counted toward `PASSAGE_TRIES`; only
  a fault with the passage alone in flight counts); the ready line's `dim` was unbounded (now 1
  to `pipe::MAX_DIM`, 4096, else a malformed ready line, and the answer's size is computed
  checked); the drop after a failed check left the reconciliation made from the dropped vectors
  (section 8).
- **Review E13r, the remaining findings fixed** (each a red test first, then the fix; no
  migration; C7 0.12.0, additive):
  - Finding 3: a check that does not pass (corrupted, unusable, or no result) is followed by the
    next one after `worker::CHECK_RETRY` (60 s), doubling to `worker::CHECK_RETRY_MAX` (1 h),
    never past `verify_interval`; a passed check goes back to the interval. A dense side held
    for a file broken for a moment comes back within minutes, not a day later. The gold runbook
    says to replace a model file only by rename.
  - Finding 5: a passage that the helper is SIGKILLed on from outside three times in a row
    (`worker::DEFER_KILLS`) goes behind the other passages for 300 s, twice its last wait after
    each later kill, at most 6 h; it is never skipped for it (owner ruling). In memory only.
  - Finding 6: the daemon compares files by device, inode and length. A passed check restarts a
    ready helper that maps another file than the one verified (a same-content rename over it);
    a helper restarted onto a file other than the last one verified is held down until a check
    passes on it; a file replaced while the check runs gives no result.
  - Finding 7: `health` adds `vectors_missing`, `passages_skipped`, `passages_deferred` and
    `model_check` (the last check: time, `passed`, `corrupted`, `unusable` or `no_result`, and
    why). `dense` and `lexical_only` keep their meaning.
  - Finding 9: whether a check applies is decided at every check from the model directory as it
    is then (`check::Verdict::NotApplicable` for an ONNX directory, which also ends a hold of an
    earlier verdict), not once at the daemon's start.
  - The helper reads a request line of at most `pipe::MAX_REQUEST` (8 MiB; engine review E13r,
    F6). Without lanes an over-long line gets the protocol's error answer; with lanes, where the
    answer needs the line's id, the helper exits 1 with the reason. The daemon sends one text per
    request from a body of at most 1 MiB (a passage: an entity's name and a fact's text, each
    from its own body), and JSON escaping never lengthens a text past its body's encoding, so its
    longest line is about 2 MiB.
- **Owner decisions**: F7 indexing never progresses under one query a second (`worker::QUIET`;
  decided in 10.1); F8 with e5 the default `reconcile_min_cosine` 0.85 turned
  2 036 of 2 991 new facts into `contradiction` digest items.
- **1 GB machine** (static musl, 1 vCPU, no swap): 2 991 facts indexed in 393 s with a query every
  4 s, short search p95 49.9 ms (target 75 ms), pointers p95 64 ms, every stored vector byte-equal
  to an independent helper, 0 wake-ups of daemon and helper in 600 s once reconciliation ended;
  bge-m3 under a cgroup `MemoryMax` of 512, 192 and 96 MB with no swap: no OOM kill, exact vectors.
- Fuzzing (eight targets, 45 min each), ASan, TSan and Miri found no fault in the product; their
  reports were tooling (rustix's raw syscalls hidden from TSan, the deliberate leak of the mapped
  weight file, Miri's float nondeterminism) and are configured accordingly.

#### 10.1 F7: indexing under a steady query load (decided 2026-11-26)

`worker::QUIET` held every passage back until 2 s had passed without a `search` or `pointers`, so
with one search a second no fact was ever embedded. Decision (lead, for the owner):

- With a helper that announces lanes (section 9), the worker does not hold passages or
  reconciliations back for queries at all. Keeping queries fast while a passage runs is the
  helper's job (query lane, indexing thread at nice +10).
- With a helper without lanes (`onnx`, the old exchange), the quiet wait stays but is capped:
  work that is ready waits at most `worker::QUIET_CAP` (10 s) from the tick that first held it
  back. Then one piece runs and the cap starts again.
- Tests: `indexing_progresses_under_a_query_every_second` (lanes, no longer ignored) and
  `without_lanes_a_query_every_second_defers_a_passage_at_most_the_cap`. The e2e scenario
  `index_query_1s` indexes 12 000 passages (11 955 accepted) on e5-small while one short search
  a second goes out. The daemon and helper are pinned to one CPU.

Measured, static musl binaries of this change, e5-small, one short `search` a second during the
whole indexing:

| Machine | Facts/s | Short search n / p50 / p95 / p99 / max (ms) |
|---|---|---|
| GitHub `ubuntu-24.04`, Intel Xeon 6975P-C, 4 vCPU; daemon and helper `taskset` to one vCPU, driver on the other three; steal 0 % | 20.5 (11 955 in 584 s) | 570 / 21.9 / 57.0 / 72.4 / 104 |
| BitLaunch `nibble-1024`: Intel Xeon Gold 6150 (Skylake-SP, AVX-512 without VNNI), 1 vCPU, KVM, 961 MiB, no swap, kernel 6.8.0-117; the driver on the same vCPU; steal 0 % (vmstat, 451 samples) | 9.5 (11 955 in 1 259 s) | 1 189 / 50.7 / **131.7** / 171 / 269 |

Before this change neither machine indexed anything under this load. For comparison, the 1 GB
truth run of E12a with one query every 4 s and the quiet wait: 7.6 facts/s, short p95 49.9 ms.

- On the 1 GB machine the short-search p95 misses the 75 ms target (and the max misses 150 ms):
  with no quiet wait, a search now always meets work. The GitHub figure (57 ms) does not stand
  in for the 1 GB machine.
- [inferred, not measured] The median doubles against a query alone (about 20 ms, section 9).
  The likely cause is the daemon's own work per fact rather than the helper's indexing thread:
  the worker writes each vector, syncs the matrix and runs `Edge::reconcile_similar` over the
  resident matrix at nice 0, while the passage runs at nice +10.
- The variant "lanes + quiet wait capped at 2 s" was not measured, and the lead dropped it (no
  quiet wait with lanes). [inferred] Under one search a second it would index about one fact
  every 2 to 3 s.

**The worker at the indexing priority (2026-11-26).** The worker thread now raises its own nice
value by `pipe::INDEX_NICE` as it starts, with the helper's `lower_this_thread` (a refusal is one
stderr line). A process and a thread inherit the nice value of the thread that creates them, and
the worker restarts the helper. So the supervisor runs every start on its own thread,
`garden-start`, made at the daemon's priority. That way a restarted helper's query lane and the
daemon's pipe threads keep the daemon's priority. Before this change, a restart from a lowered
worker would have started the whole helper at nice +10, and its indexing lane at +20, capped
at 19.

Before the change, the worker was checked for anything a search needs while it works:
- `matrix::Matrix`: `sync` reads the store before it takes the write lock and holds that lock
  only to drop and append rows. `rebuild` holds it only to swap the rows in.
- The supervisor's slot: held only to hand out the helper or change its state; a start runs
  outside it.
- The helper's lanes: the query lane has its own lock and does not wait for the passage lane;
  the daemon's writer and reader threads are shared.
- The profile lock: the request path takes it only after `resolve` and `seedbed_done`.
- SQLite: WAL mode, so a reading `search` or `pointers` never waits for the worker's write
  transactions.
- One hold remains. `Edge::reconcile_similar` runs its whole reconciliation, including the
  lexical and per-entity vector reads, inside one `BEGIN IMMEDIATE` unit of work. A request that
  writes (`propose`, a `search` that queues a miss) waits for it up to `busy_timeout`. With the
  worker lowered, that wait can stretch (priority inversion). It was left as it is: the unit
  keeps the reconciliation's reads and writes consistent, and splitting it is not a small change.

Measured on fresh BitLaunch `nibble-1024` machines (Intel Xeon Gold 6150, 1 vCPU, KVM,
961 MiB, no swap, kernel 6.8.0-117, steal 0 %). The scenario is `index_query_1s` with 9 000
passages (8 967 accepted), one short search a second, and no byte check on the paid machine (the
e2e job runs it). Runs in the order A, B, A', B'. B' had 4 500 passages, so it is not an exact
repeat of B.

| Run | Worker nice | Facts/s | Short search n | p50 | p95 | p99 | max (ms) |
|---|---|---|---|---|---|---|---|
| A: as in the first part of 10.1 | 0 | 10.3 | 825 | 43.2 | 106.7 | 129.4 | 211 |
| A': repeat | 0 | 10.5 | 815 | 43.5 | 102.5 | 134.1 | 170 |
| B: worker at +10 | 10 | 9.4 | 917 | 35.2 | 94.7 | 114.2 | 187 |
| B': worker at +10, 4 500 passages | 10 | 11.7 | 371 | 32.1 | 61.6 | 74.0 | 83 |

CPU share of the one vCPU during the load, A / B: helper indexing lane 0.56 / 0.51, worker
0.38 / 0.44, helper query lane 0.017 / 0.017, daemon request thread 0.019 / 0.019, measuring
client 0.013 / 0.013.

Per search (A' and B', `/proc/<tid>/schedstat` over 480 s and 300 s windows):

| Thread | A' CPU / run-queue wait (ms) | B' CPU / run-queue wait (ms) |
|---|---|---|
| helper query lane | 18.2 / 5.4 | 17.9 / 4.6 |
| daemon request thread | 18.6 / 6.8 | 13.5 / 2.5 |
| measuring client (Python) | 12.3 / 4.3 | 7.0 / 1.9 |
| daemon pipe reader and writer | 1.0 / 7.6 | 1.2 / 5.2 |

- A against A' differs by 4 ms in p95. With the worker lowered (B), p95 falls by 8 to 12 ms
  and p50 by 8 ms, and indexing loses about 10 % (9.4 against 10.3 to 10.5 facts/s). On
  9 000 passages the p95 is still above the 75 ms target. B' (shorter run, smaller store) is
  under it at 61.6 ms.
- What a search is made of: about 31 ms of its own CPU, the query's forward pass (18 ms) plus
  the daemon's lexical and dense search and RRF (13 to 19 ms). The rest is run-queue wait
  behind the two lowered threads, which still get their CFS share on the one vCPU. The
  measuring client costs another 7 to 12 ms of CPU per search on the same vCPU, which a real
  caller on the same machine would cost too.
- Open: the p95 on a 1 vCPU machine during a long indexing stays around 95 ms. To get under
  75 ms needs less CPU per search (the daemon's own 13 to 19 ms) or a stronger yield from
  indexing (nice 19 or `SCHED_IDLE` for the worker and the indexing lane, which section 9
  rejected for the lane because of starvation next to other load). That is the owner's choice.

## Consequences

- The garden's dense side becomes a static binary like every other daemon. The glibc exception
  of ADR-0047 ends at E7.
- Model RAM drops to what is touched, and idle models stop holding anonymous memory.
- One crate carries the int8 kernels for every future encoder model and for the speech port.
- The repository gains `unsafe` in two modules: the kernels, guarded by fuzz tests and hardware
  runs, and the weight file's `mmap`.
- Each new kernel needs a rented host with its flag before it is enabled.

## Open questions

1. Resolved: the kernels live in the same crate, `crates/infer`, module `kernels`; a speech port
   is a second model module there, not a second crate (library to library dependencies stay
   forbidden, ADR-0072).
2. Resolved: the converter reads the `safetensors` checkpoint. The engine is judged against
   PyTorch fp32, and our quantisation already beats the ONNX int8 export's accuracy in
   simulation (section 3).
3. Does the ONNX Runtime path stay as a fallback feature between E6 and E7, and for how long?
   E6: it stays buildable and selectable (`--embedder auto` picks it for a directory with the
   ONNX file); how long is E7's ADR.
4. i8mm on Apple: which generations expose `FEAT_I8MM` (ADR-0086 section 4's probe answers it per
   machine)? Answered for one: the owner's M3 Pro reports `FEAT_I8MM` 1 (E11m); the other
   generations are not checked, and an `i8mm` kernel is in the deferred backlog (section 5.4).
5. Resolved: the pooling kind (CLS or mean), not in `config.json`, lives in the weight file's
   header, set at conversion (`--pooling`, since E3); the helper reads it from there (E6).
6. A larger question set for the retrieval criterion of E3 (about 10× the current one), as part
   of ADR-0086 card 5.
7. Resolved (owner's decision, 2026-11-25): the E3 bound against PyTorch fp32 for bge-m3 and
   other 24-layer models is min pooled cosine ≥ 0.997 on every text and median ≥ 0.998 (the
   simulation's proposal; B = 32 measured min 0.99799, median 0.99872; the Rust engine 0.998485 on
   the 16 golden cases); e5-small keeps ≥ 0.999 on every text. `tests/golden_int8.rs` takes the
   bound by the model's depth (24 layers or more: the bge-m3 bound) and fails below it (commit
   7d91010): bge-m3 passes with min 0.998485, median 0.998836; e5-small with min 0.999562,
   median 0.999772. Block
   weight scales (about 19 MB) are not needed.
   **Real texts (E11m, 2026-11-26): the bound is met in the median and missed by 0.2 % of the
   texts.** On 6 671 real texts (4 671 passages and 2 000 proxy queries, section 4) bge-m3's
   median pooled cosine is 0.998629, above 0.998, but 14 texts (0.2 %) are below 0.997, the
   lowest 0.995676, and retrieval against fp32 is unchanged. **Proposal, not yet decided by the
   owner:** state the bound for real inputs as 1st percentile ≥ 0.997, median ≥ 0.998 and minimum
   ≥ 0.995 (measured on the passages: p1 0.997563, median 0.998556 on the passages alone and
   0.998629 on all 6 671 texts, minimum 0.995676, a margin of 0.0007 on one sample). The golden
   test's bound stays as it is (every case ≥ 0.997, median ≥ 0.998).
8. Where does the token frequency order live? E3's converter takes it as an input file
   (`vocab_size` little-endian `u32` ids, about 1 MB) and records only its SHA-256 in the
   header. Section 2 asks for the sample to be "recorded with the converter". Open: the order
   file in the repository, or the script and the sample's hashes in pasekahq/paseka-evals
   with the file fetched at conversion.
9. Resolved for e5-small (owner's decision, 2026-11-25): the weight file row is ≤ 130 MB in 10⁶
   bytes, the tokenizer section included (the format-4 file is 127 294 800 bytes = 127.3 MB).
   **Open for bge-m3:** no limit decided; its row stays "≤ 580 MB" in an undecided unit (the file is
   602 605 584 bytes = 602.6 MB = 574.7 MiB: over in 10⁶ bytes, under in MiB). History: the e5-small
   file was 120 474 496 bytes before E2 (over "120 MB" in 10⁶ bytes by 0.47 MB), 127 294 672 with
   the tokenizer section (2.5 MiB piece bytes, 2 MiB `f64` scores, 1 MiB ids, 1 MiB offsets,
   0.2 MiB character map), 127 294 736 with the prefixes (E6).
10. Resolved (owner's decision): the engine's pipeline for E6 is `whitespace-split` (B).
    - It is the tokenization of the original SentencePiece model the checkpoint was trained with:
      on the 664 synthetic texts B gives the same pieces as the SentencePiece library on 664,
      `collapse-spaces` (A, what `tokenizer.json` and today's ONNX helper do) on 376 (the 288
      texts ending in a newline get an extra `▁` under A); on a random 20 000 of the Wikipedia
      texts both give the same pieces on all 20 000. B is not identical to SentencePiece on every
      text: on 5 of the 283 844 Wikipedia texts a near-tie between two segmentations (score gap
      6e-5) goes the other way inside a long text, and on contrived texts (literal `<mask>`,
      Hangul jamo, stacked combining marks, U+0085) it differs; A differs there too.
    - All quality measurements of this ADR (golden dumps, simulation, E3 baselines, E2 retrieval)
      were taken with B.
    - E6 changes the model id, so every stored vector is embedded again (ADR-0047) and the choice
      costs no extra re-embedding. A stays in the converter (`--tokenizer-pipeline
      collapse-spaces`) only to compare with the old helper.
    - Measured on the synthetic set, e5-small int8 B = 32, A against B (`reports/e2-tokenizer-ab.md`
      in the experiment directory): 288 texts differ, all passages ending in a newline, cosine
      A to B min 0.9968, p5 0.9983, median 0.9994; no query differs. Documents A with queries B
      (an old index queried by the new pipeline), against B with B: dR@3 -0.59 [-2.06, +0.88],
      dMRR -0.19 [-1.10, +0.76], 51 of 340 top-3 lists changed; A with A gives the same numbers.
      How many stored texts end in whitespace is not measured.
11. Resolved (E6): the e5 prefixes live in the weight file's header (`meta.prefixes`, format
    `ulei-infer/bert-int8/3`), written by the converter (`--query-prefix`, `--passage-prefix`,
    both required, empty for a model without them), as the pooling is; the helper puts the
    prefix of the request's kind (ADR-0047 section 2) before the text, names no model, and the
    file's SHA-256 covers them.
12. Resolved (owner's decision, E6c): the converter seals a content digest into the weight file
    (section 2, "The content digest"; format `ulei-infer/bert-int8/4`) and the helper takes it as
    the model's identity at start, checking only the preamble, the file's length, the header and
    the tensor table; `ulei-garden-embed verify` hashes the whole file. Ready after start is now
    3 to 12 ms for e5-small and bge-m3 (section 4, E6c). The identity changes once against the
    old file hash. A blob byte that changes on disk after conversion is no longer detected at
    start, only by `verify`; the garden side of that is open question 15.
13. For the owner (E6): C9 still requires `garden.embedder.ort_library`, which the engine
    ignores, and describes `model_dir` as the ONNX files: a C9 change at E7 or before.
14. Resolved (owner's decision, 2026-11-25): the package `ulei-garden` is built at opt-level 3 in
    the release profile, as `ulei-infer` is (`Cargo.toml`, ADR-0001 section 8; commit 4053804).
    The helper was 1.6× the engine alone because fat LTO vectorises the engine's loops only at the
    binary's package's opt-level (sections 4 and 5.2). Measured in one session on the pilot host
    (e5-small, the 116 E4 inputs, one thread, median of 5 rounds after a warm-up, format-4 file;
    quiet host: other load at most 0.10 vCPU before every run, steal at most 0.17 %):

| Build | `ulei-garden` at `"s"` | at 3 | / ONNX Runtime at 3 |
|---|---|---|---|
| helper, glibc | 18.22 s | 11.81 s | 2.22× |
| helper, static musl | 18.20 s | 12.69 s | 2.38× |
| ONNX Runtime 1.28.0 (Python, same ids) | 5.33 s | | 1 |

    - The helper now runs as fast as the engine's own probe (E6b: 11.89 s glibc, 12.49 s musl).
    - Vectors unchanged: the 664 synthetic texts give E6b's bytes, glibc and musl.
    - Size: helper glibc 656 392 → 735 360 B, static musl 754 424 → 832 248 B, ONNX glibc 722 880 →
      803 256 B (budget 2 MiB, static and `ldd` checks pass); the daemon `ulei-garden` shares the
      package: glibc 2 013 368 → 2 325 304 B (+311 936). The static musl daemon does not link on the
      pilot host (no musl C toolchain for the bundled SQLite: `open64`, `__memcpy_chk` undefined),
      so its size against the 2 621 440 B row is first measured by CI's budgets job. The other
      options of E6b (a helper-only profile, explicit SIMD) were not taken.
15. Resolved (owner's decision, 2026-11-25): section 8. The worker checks the weight file with
    `ulei-garden-embed verify` at start and every `garden.schedule.verify_interval` (24 h); a
    corrupted verdict holds the dense side down (lexical-only, no restarts, embedding paused) and
    drops only the vectors computed since the last passed check (new `fact_vectors.computed_at`);
    a passed check brings it back; a corrupted file at start starts the daemon lexical-only
    instead of refusing (amends ADR-0047 section 2). Residual window: at most one period.
16. Resolved (owner's approval of the design, 2026-11-26): section 9. The helper answers on two
    lanes, a query lane and an indexing lane, two threads over the one mapped encoder with a
    workspace each; the indexing thread raises its own nice value by 10. A 16-token e5-small query
    behind a stream of 512-token passages on one core: 409 ms → 21 ms (median), bge-m3 7.4 s →
    0.19 s; vectors unchanged; idle dirty memory within section 6.
17. Resolved (owner's decision, 2026-11-26): **NEON is the default kernel on `aarch64`**:
    `Kernel::best()` selects it when it is available, from the commit that follows this record
    (the ADR is text only; the code change is that commit). Basis: bit-identity on real hardware,
    an Apple M3 Pro (E11m, section 4): the probe vectors of both models and the 664 text-in
    vectors byte-identical to the Linux AVX2 ones, the golden worst cosines equal to the last
    digit, no mismatch in 4 000 000 block-sum cases over both NEON paths and 200 000 linear-layer
    cases, the whole suite green with NEON as the default; it is 2.2 to 3.2× faster than
    portable. **Accepted residual risk:** Linux `aarch64` (Graviton, Ampere) and an `aarch64`
    CPU without `dotprod` have not been run natively (the widening path was only forced on a
    `dotprod` CPU). Those runs, a NEON attention and a faster NEON linear kernel are in section
    5.4. The decision satisfies "no hardware, no kernel" of section 5 for the Apple M3 Pro only.
18. For the owner (found at section 9): a call's deadline in the daemon is 5 s
    (`pipe::CALL_DEADLINE`, ADR-0047 section 2), but on the pilot host a bge-m3 input of 512
    tokens takes 7.0 to 7.7 s. Since section 9 a passage on the indexing lane has 300 s; a bge-m3
    query of about 350 tokens or more still misses 5 s, so the daemon kills the helper, answers
    lexical-only and restarts it. Options: a query deadline by model (or by the input's length),
    or a cut of the query to fewer tokens. The ONNX backend (one lane) keeps 5 s for every call.

## Cards

1. E0: the golden dump script in pasekahq/paseka-evals (PyTorch fp32 + ONNX Runtime int8) for
   e5-small and bge-m3, on a fixed ru/en text set including long, mixed-script and empty inputs.
2. E1 + E2: `crates/infer` (feature `bert`) with the portable forward pass, the tokenizer
   (module `text`, data in the weight file; done for e5-small at E2), both poolings; golden
   tests.
3. E3: the weight format with the frequency-ordered token table and its id → row map,
   `ulei-garden-embed convert` with the recorded frequency sample, block-scale activation
   quantisation (section 3); the E3 acceptance of section 4 on the ADR-0086 sets. The
   simulation scripts and raw results behind sections 2 and 3 are to be published in
   pasekahq/paseka-evals (not done yet).
4. E4: mmap and the buffer pool; RSS (dirty / clean), idle and start-up measured. (The pool
   was done at E6b.)
5. E5a: AVX2 and NEON SDOT kernels ported from the reference engine, with the fuzz test. NEON ran on
   an Apple M3 Pro at E11m (section 4).
6. E5c: AVX-512BW kernel, built right after E5a and measured on the pilot host.
7. E5b: record `lscpu` of the hourly hosts reachable through the BitLaunch account (own KVM and
   resold providers) to find one with AVX-512 VNNI or AVX-VNNI; then the VNNI kernel with the
   signedness correction, verified on that host. Run by the owner (paid). E10r recorded four rented machines (section 4): none has VNNI, so the
   kernel is still waiting for a host.
8. E6: the helper on the encoder, static musl; the CI job beside `garden-onnx` (replacing it at
   E7); the budget row. Done for e5-small (section 4), commit 417fe8c.
9. bge-m3 through E0 to E4.
