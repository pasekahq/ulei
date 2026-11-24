# ADR-0087: Encoder engine for embedders: one parameterised BERT / XLM-R encoder in Rust, int8 weights by mmap, SIMD kernels per instruction set, in place of the ONNX Runtime helper

Status: proposed (draft, not for merge)
Date: 2026-11-24
Affects: crates/garden, crates/infer, dep:serde_json

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
  tokenises it in Rust (`embed/tokenizer.rs`, golden-tested).

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
  - `bert` (cargo feature `bert`): this decision's encoder: embeddings, self-attention, FFN with
    GELU, post-LayerNorm, pooling (CLS or masked mean) and the tokenizer, moved from
    `crates/garden/src/embed/tokenizer.rs`.
- A new model is a new feature-gated module on the same layers; a new kernel goes into
  `kernels` only. The speech port (ADR-0086 open question 3) becomes a second model module
  (`conformer`) and adds its module crate to `users` by its own ADR.
- Its crate class is the library class of ADR-0072: no workspace dependency, linked only by the
  modules in its `users` (today `ulei-garden`).
- A model is a config (the columns above) plus a weight file. No model-specific code paths
  beyond the config.
- Inference only, at most 512 tokens per input as today (ADR-0047).
- Cross-encoder rerankers of the same family (XLM-R) are a later use of the same engine, not
  part of this decision.

### 2. Weight format and conversion

- One file per model: a header (config, tensor table, SHA-256 of the source checkpoint) and
  64-byte aligned blobs, opened by `mmap` read-only.
- Linear weights (Q, K, V, O, FFN in and out) are int8 with a per-output-channel fp32 scale.
- The token embedding table is int8 with a per-row scale. It is read row by row, so only the
  pages holding rows of seen tokens are paged in.
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
- The converter is a subcommand of the helper (`ulei-garden-embed convert`). It reads the
  model's original `safetensors` checkpoint and quantises in Rust; no Python enters the
  repository's runtime or build. The output is deterministic, so its SHA-256 is the model
  identity of ADR-0047 section 2.

### 3. Activations

- Activations are quantised to int8 for each Linear layer with one scale per token row and per
  block of B input channels. The GEMM is integer inside a block (int32 accumulation); the
  block sums are scaled by their block's activation scale and combined in fp32, then scaled by
  the weight's output-channel scale. B = 64 bytes is one AVX-512 register, two AVX2 registers
  or four NEON registers, so the kernel loop of section 5 works block by block. The cost is
  one fp32 multiply-add per row, block and output: K / 64 = 6 per output (24 for FFN out)
  instead of 1.
- B is fixed after the bge-m3 run. That run, which confirms the block scheme on bge-m3 and
  compares block sizes there, is pending (an addendum to the E3 simulation report). On e5-small
  both B = 64 and B = 32 pass; inputs near 512 tokens were not in the simulated set (longest
  203 tokens).
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
- Attention scores, softmax, LayerNorm and GELU run in fp32. They get SIMD only where the
  profile shows them, as in the reference (its profile: GEMM 87 %).
- Activation buffers come from a per-process pool reused across layers and batches, as the
  reference Demucs port does (gitlab.com/asvedr/gemucs, `internal/kernels/pool.go`).

### 4. Milestones and acceptance, in order; none starts before the previous one is green

| Step | Content | Accepted when |
|---|---|---|
| E0 Reference | Golden dumps from the reference (PyTorch fp32 and ONNX Runtime int8) on a fixed ru/en text set: token ids, embedding output, every layer's output, pooled vector. Generated outside the repository (pasekahq/paseka-evals); the repository holds their SHA-256 and a skip when absent. PyTorch fp32 is the reference of every accuracy check; the ONNX Runtime run is only the baseline the engine must not be worse than | the dump script is reproducible; the SHA-256 of the dumps and of the checkpoint are recorded in the repository (still to do) |
| E1 Naive fp32 | Portable Rust forward pass, fp32 weights | every layer max abs diff ≤ 1e-4 against PyTorch fp32; pooled cosine ≥ 0.99999. Done for e5-small (below) |
| E2 Configs | e5 mean pooling and prefixes, bge CLS; tokenizer moved into the crate | e5-small and bge-m3 both pass E1 |
| E3 int8 | Format of section 2, activation quantisation of section 3 | pooled cosine against PyTorch fp32 ≥ 0.999 on every text of the fixed set, including 512-token inputs; on the ADR-0086 section 9.2 sets, the paired-bootstrap 95 % interval of the recall@3 difference against fp32 lies within ±2 points, and the engine is not below the ONNX Runtime int8 run by more than that interval |
| E4 Memory and start | mmap, buffer pool | dirty RSS and clean RSS measured per model (targets in section 6); ready within 100 ms of start with a warm page cache |
| E5 Kernels | Section 5 | each kernel passes the bit-equality fuzz test on real hardware with its flag; speed recorded per host class |
| E6 Helper swap | `ulei-garden-embed` built on the encoder as a static musl binary; same pipe protocol and model identity | the helper's budget row (section 6) holds in CI; `garden-onnx` replaced by an encoder job |
| E7 Removal | `ort` and `spm_precompiled` dropped | its own ADR, after E6 has run on the reference hosts of ADR-0086 section 5 |

e5-small goes through E0 to E6 first, being the smallest and quickest to verify. bge-m3 then
repeats E0 to E4 on the same code.

- **E1 result (e5-small, 2026-11-24, pilot host).** All 16 golden cases pass. Worst layer max
  abs diff 8.5e-6 (512-token case, layer 9) against the 1e-4 bound; worst pooled cosine
  1 − 1.8e-13. Commit 7bf099b; crate renamed to `ulei-infer` in 9bf9385 with identical diffs.
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

### 5. Kernels per instruction set

The int8 GEMM (activations int8 × weights int8 → int32) is the hot spot. One portable
implementation and one kernel per instruction set. The best available kernel is picked once at
start by run-time feature detection. The binaries keep the baseline ISA (ADR-0086 section 2).

| Kernel | Instruction | CPUs | Status | Order |
|---|---|---|---|---|
| portable | plain Rust | all | E1 | reference for every fuzz test |
| AVX2 | `VPMOVSXBW` + `VPMADDWD` (signed path) | x86_64 of the last decade, the x86 baseline | ported from the reference engine | 1: tested on the pilot host now |
| NEON SDOT | `SDOT` | Apple M1+, Neoverse N1 (Ampere), Graviton 2+ | ported from the reference engine | 1: tested on the owner's Apple M3 Pro now |
| AVX-512 VNNI / AVX-VNNI | `VPDPBUSD` (512 or 256 bit) | Cascade Lake, Ice Lake and later Xeons; AMD Zen 4+; Intel Alder Lake+ (AVX-VNNI) | new | 3: the largest gain over AVX2 on a large share of VPS hosts, built once a host we can rent by the hour shows the flag (card 7) |
| AVX-512BW without VNNI | `VPMADDWD` on 512-bit registers | Skylake-SP (the pilot host's class) | new | 2: testable on the pilot host now; wider registers only, Skylake lowers its clock under AVX-512, gain expected in the tens of per cent, to be measured |
| arm64 i8mm | `SMMLA` | Graviton 3+, Neoverse V1/N2; Apple per generation, to verify | new | 4, or 2 if the owner's M3 Pro reports `hw.optional.arm.FEAT_I8MM = 1` |

Rules:
- **Bit-equal.** Every kernel computes the same int32 sums as the portable one. A fuzz test over
  random shapes, including tails (k not a multiple of the vector width) and extreme values,
  proves it. The reference ran 7.4 million cases. The test runs in CI under emulation where
  possible, and on real hardware with the flag before the kernel is enabled.
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
the read-only `mmap` of the weight file (section 2) needs one `unsafe` call; without it the
weights would be heap copies and the memory win of section 6 is lost. So the crate lifts the
denial for two modules only: `kernels`, under the fuzz rule above, and the `mmap` of the weight
file in `weights`. Nothing else. This lifting is part of this decision.

### 5.1 Fused attention, later

Attention scores (Q·Kᵀ, fp32) are the second hot spot once GEMM is fast; in the reference they
were 5 % of the profile. They get an int8 or fp32 SIMD kernel only if E5's profile shows them
above 15 %.

### 6. Budgets (targets; replaced by E4 to E6 measurements before acceptance)

| Model | Weight file | Dirty RSS while embedding | Clean RSS while embedding | RSS idle after 10 min (no pressure) | Ready after start | Passage embed time vs ONNX Runtime on the same host |
|---|---|---|---|---|---|---|
| e5-small | ≤ 120 MB | ≤ 60 MB | ≤ 100 MB | clean pages may stay; dirty ≤ 20 MB | ≤ 100 ms | ≤ 1.5× |
| bge-m3 | ≤ 580 MB | ≤ 150 MB | ≤ 350 MB | as above | ≤ 100 ms | ≤ 1.5× |

The speed row bounds the regression: the engine is not chosen for speed (ADR-0086 section 9.4),
but indexing time is a profile cost.

- The clean-RSS targets assume the frequency-ordered token table of section 2.
- The "≤ 1.5× ONNX Runtime" speed column is provisional: the reference Go engine ran 2.5×
  slower on this host's AVX2 path (RTF 0.86) than on the Apple SDOT path (RTF 0.34).

### 7. Not decided here

- Removing ONNX Runtime from the build (E7, its own ADR).
- The speech-to-text port (ADR-0086 open question 3).
- Rerankers on the same engine.
- Inputs longer than 512 tokens.

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
4. i8mm on Apple: which generations expose `FEAT_I8MM` (ADR-0086 section 4's probe answers it per
   machine)?
5. Where does the pooling kind (CLS or mean) live? It is not in `config.json` (it is in
   sentence-transformers' pooling config or the model card); E1 passes it in. Proposed: the
   weight file's header, set at conversion.
6. A larger question set for the retrieval criterion of E3 (about 10× the current one), as part
   of ADR-0086 card 5.

## Cards

1. E0: the golden dump script in pasekahq/paseka-evals (PyTorch fp32 + ONNX Runtime int8) for
   e5-small and bge-m3, on a fixed ru/en text set including long, mixed-script and empty inputs.
2. E1 + E2: `crates/infer` (feature `bert`) with the portable forward pass, the tokenizer moved from the garden,
   both poolings; golden tests.
3. E3: the weight format with the frequency-ordered token table and its id → row map,
   `ulei-garden-embed convert` with the recorded frequency sample, block-scale activation
   quantisation (section 3); the E3 acceptance of section 4 on the ADR-0086 sets. The
   simulation scripts and raw results behind sections 2 and 3 are to be published in
   pasekahq/paseka-evals (not done yet).
4. E4: mmap and the buffer pool; RSS (dirty / clean), idle and start-up measured.
5. E5a: AVX2 and NEON SDOT kernels ported from the reference engine, with the fuzz test.
6. E5c: AVX-512BW kernel, built right after E5a and measured on the pilot host.
7. E5b: record `lscpu` of the hourly hosts reachable through the BitLaunch account (own KVM and
   resold providers) to find one with AVX-512 VNNI or AVX-VNNI; then the VNNI kernel with the
   signedness correction, verified on that host. Run by the owner (paid).
8. E6: the helper on the encoder, static musl; the CI job replacing `garden-onnx`; the budget row.
9. bge-m3 through E0 to E4.
