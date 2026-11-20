# ADR-0044: Garden embedder, hybrid search and its evaluation

Status: accepted
Date: 2026-11-20
Affects: crates/garden, dep:ort, dep:spm_precompiled

## Context

ADR-0042 puts hybrid lexical + dense retrieval into `ulei-garden`; ADR-0043 gives the store an
FTS5 index (bm25 over fact text, entity name and aliases) and stored vectors per model. The
owner's earlier Python prototype of retrieval is the reference of the behaviour: bge-m3 int8
through ONNX on the CPU, CLS pooling, L2 normalisation, at most 512 tokens and no prefixes;
reciprocal rank fusion of the dense and the lexical ranking with the promoted profile
`rrf_k = 0`, both weights 1, 100 candidates per source; a conservative lexical query cleanup.
Retrieval answers on two tiers: passive pointers on every request (no LLM) and the active C7
`garden_search`. The workspace MSRV is 1.89, `deny.toml` bans OpenSSL and vendor SDKs, and CI
builds every feature without network access to model files or runtime libraries.

## Decision

1. `ulei_garden::embed::Embedder` is the seam: `model_id`, `dim`, `embed_query`,
   `embed_passages`, unit-length vectors. `HashEmbedder` (hashed character trigrams, signed,
   L2-normalised) is the deterministic fake for tests and CI.
2. `embed::onnx::OnnxEmbedder` behind the cargo feature `onnx` (off by default) runs bge-m3:
   `onnx/model_quantized.onnx` and `tokenizer.json` read from a model directory given at run
   time, CLS pooling of `last_hidden_state`, L2 normalisation, truncation at 512 tokens, padding
   with id 1, `token_type_ids` fed only if the graph asks for them, batches of 8 sorted by
   length, intra-op threads from the caller and one inter-op thread.
   - `ort =2.0.0-rc.13` (MIT OR Apache-2.0, MSRV 1.88; rc.14 and rc.15 need 1.92) with
     `default-features = false` and `std`, `load-dynamic`: the ONNX Runtime shared library is
     opened from a path given at run time (`ort::init_from`), nothing is downloaded or linked at
     build time, no TLS stack. API level 17 (the crate default); any newer runtime serves it.
   - The tokenizer is our own `embed::tokenizer` (feature `onnx`): it reads `tokenizer.json`
     and runs the `XLM-R` pipeline of bge-m3 — the precompiled `SentencePiece` character map
     (`spm_precompiled =0.1.4`, Apache-2.0, the same normaliser the `tokenizers` crate uses),
     runs of spaces collapsed, Metaspace (`▁` always prepended, split before each `▁`), Unigram
     Viterbi over the vocabulary (unknown characters scored `min - 10`, consecutive unknowns
     fused), `<s> … </s>` around at most 510 pieces. On a 55 564-text sample of a real
     Russian/English library (8.4 M tokens) its ids equal those of `tokenizers 0.23.2` with
     truncation at 512, without a single mismatch.
   - Not `tokenizers`: every 0.x release depends on `paste`, which is unmaintained
     (RUSTSEC-2024-0436, fails `cargo deny`), and pulls `rayon`, `derive_builder`, `rand` and a
     regex engine; 1.0.0-rc.4 drops `paste` but has no truncation or padding yet and does not
     build with minimal features. `fastembed` is not used either: it pins `ort =2.0.0-rc.15`
     (MSRV 1.92) and pulls `hf-hub` and native TLS.
   - The new transitive crates are MIT, Apache-2.0 or ISC (`libloading`) and build on 1.89
     (checked with `cargo +1.89 check --features onnx`).
3. `ulei_garden::search` ranks facts that hold at (`valid_at`, `known_at`):
   - lexical candidates from `Store::lexical`, dense candidates by brute-force dot product over
     `Store::vectors` of the embedder's model; the dense side sees the raw query, the lexical
     side the cleaned one;
   - weighted RRF: each ranking counts an item once at its first rank `r` (from 1) and adds
     `weight / (rrf_k + r)`; ties by id; defaults `rrf_k = 0`, weights 1 and 1, 100 candidates;
   - without an embedder (or with dense weight 0) the search is lexical only;
   - `pointers` is tier (b): up to `n` one-line pointers "there is a fact about <entity>
     (<fact id>)", one per entity, fused score at least a threshold, no fact text;
   - `index_missing` embeds the facts `Store::missing_vectors` lists.
4. Query cleanup ports the prototype's cheap part: e-mail addresses and the Russian request and
   grammar words are dropped, duplicates removed, a fixed list of technical loanwords expanded;
   the original query is kept when nothing would remain.
5. `ulei_garden::eval` and the binary `garden-eval` measure retrieval on a Markdown corpus:
   every file becomes an `item` entity (name and aliases from front matter or the first
   heading), every bounded passage a fact with `source_ref` = the relative path. Passages port
   the prototype's chunker: paragraphs packed up to `--passage-chars` (default 1200), long
   blocks split, each passage repeating path, selected front matter, the `INDEX.md` description
   and its heading trail. Metrics follow the prototype: recall@k = share of gold files among the
   first `k` distinct files, MRR of the first gold file, over rows with
   `expected_source == "garden"` and gold files, overall and per category; warm latency p50/p95
   with model load, ingest and embedding reported apart. Files are ranked by RRF of the
   per-source file rankings, as the prototype fuses files.
6. A synthetic corpus and gold set (fictional people and places, 34 questions, 8 categories)
   live in `crates/garden/tests/fixtures/`; a test runs the harness lexical-only and with the
   fake embedder and holds recall@3, MRR and p50 floors.

## Deviations from the prototype

- The lexical index has one column of `unicode61` tokens (ADR-0043); the prototype's lemma
  column (pymorphy3) and its transliteration column are not ported, and stop words match surface
  forms only, not lemmas.
- Lexical and dense see the same passages (1200 characters); the prototype embedded paragraph
  chunks and searched 2000-character context passages lexically.
- The text embedded per fact is `"<entity name>: <fact text>"` (`Store::missing_vectors`).
- Relative-date expansion of queries ("today", "tomorrow") and the reviewed loanword dictionary
  beyond the fixed list are not ported; the profile's `date_weight` is 0 anyway.
- Added (special) tokens written literally in the input text are not split out before
  normalisation as the `tokenizers` crate does; the parity sample showed no mismatch from it.
- Model artifacts are not checked against pinned SHA-256 sums; `model_id` names the model and
  its settings, not the file hashes.

## Consequences

- CI compiles and lints `onnx` with `--all-features` but never loads a model; the ONNX path is
  exercised only by a local run with the library and the model present.
- Dense ranking decodes every vector from SQLite per query: fine up to some ten thousand
  facts. A resident matrix or an ANN index is the next step beyond that.
- The fused RRF score with `rrf_k = 0` is 1 for a first rank in either source; the pointer
  threshold is on that scale and is not tuned yet.
