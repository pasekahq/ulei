# ADR-0045: Garden embedder, hybrid search and its evaluation

Status: accepted
Date: 2026-11-20
Affects: crates/garden, dep:ort, dep:spm_precompiled

## Context

ADR-0043 puts hybrid lexical + dense retrieval into `ulei-garden`; ADR-0044 gives the store an
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
     fused), `<s> … </s>` around at most `max(max_tokens, 2) - 2` pieces. Its work is bounded
     by the budget, not by the input: it reads only the first `4 × (budget + 1) × longest piece`
     bytes of the text and segments a word in windows of 1024 bytes, stopping when the budget
     is full. Both cuts change ids only for text whose first such bytes yield fewer pieces than
     the budget (long runs of spaces, removed control characters or fused unknowns) and for
     words longer than 1024 bytes, where Viterbi sees each window alone. On a 55 564-text sample of a real
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
   - `SearchParams::validate` rejects a non-finite or negative `rrf_k` or weight and two zero
     weights before any work; the query is bounded first: above 4000 characters only its first
     3000 and last 900 are searched;
   - lexical candidates from `Store::lexical`, dense candidates by brute-force dot product over
     `Store::vectors` of the embedder's model; the dense side sees the raw query, the lexical
     side the cleaned one;
   - weighted RRF: each ranking counts an item once at its first rank `r` (from 1) and adds
     `weight / (rrf_k + r)`; ties by id; defaults `rrf_k = 0`, weights 1 and 1, 100 candidates;
   - without an embedder (or with dense weight 0) the search is lexical only;
   - a reranker (`rerank::Reranker`) reorders the fused hits before they are cut to `k` or
     gated into pointers; it cannot add or drop a hit, and `NoReranker` keeps the fused order;
   - `pointers` is tier (b): up to `n` one-line pointers "there is a fact about <entity>
     (<fact id>)", one per entity, no fact text. RRF scores depend on ranks only and the dense
     side always returns its nearest facts, so a threshold on the fused score cannot tell an
     unrelated prompt from a related one. A hit therefore becomes a pointer only with evidence
     from one source: its cosine at least `min_cosine` or its lexical score (-bm25) at least
     `min_lexical` (`PointerGate`, defaults 0.3 and 4.6); the reranked order only orders the
     hits that pass. The defaults are the middle of the widest range of each threshold on the
     synthetic set where no unrelated question gets a pointer and the most related ones get a
     pointer to a gold file. The cosine default holds for the fake embedder, not for bge-m3,
     and the scale of bm25 grows with the corpus, so both follow the deployment through
     `garden.api.pointer_min_cosine` and `garden.api.pointer_min_lexical` (ADR-0047 section 4);
   - `is_trivial_prompt` runs in front of `pointers` and is public for the C7 layer: empty
     text, a slash command, or a bare acknowledgement or greeting followed only by spaces and
     punctuation gets no pointers;
   - `index_missing` embeds the facts `Store::missing_vectors` lists: current versions only. A
     version superseded before it was indexed never gets a vector, so an `as_of` search finds it
     lexically only; vectors of indexed versions stay and serve `as_of` on both sides.
4. Query cleanup ports the prototype's cheap part: e-mail addresses and the Russian request and
   grammar words are dropped, duplicates removed, a fixed list of technical loanwords expanded;
   the original query is kept when nothing would remain.
5. `ulei_garden::eval` and the binary `garden-eval` measure retrieval on a Markdown corpus:
   every file becomes an `item` entity (name and aliases from front matter or the first
   heading), every bounded passage a fact with `source_ref` = the relative path. Passages port
   the prototype's chunker: paragraphs packed up to `--passage-chars` (default 1200), long
   blocks split, each passage repeating path, selected front matter, the `INDEX.md` description
   and its heading trail. The default is the largest size up to 1200 whose recall@3 and MRR on
   the synthetic set, lexical-only and with the fake embedder, equal the best of the sizes
   measured: larger passages mean fewer embeddings, and the embedder reads at most 512 tokens
   of a passage. Every passage is embedded; `--dense-head N` limits that to the first `N`
   passages of each document. Metrics follow the prototype: recall@k = share of gold files
   among the first `k` distinct files, MRR of the first gold file, over rows with
   `expected_source == "garden"` and gold files, overall and per category; warm latency p50/p95
   with model load, ingest and embedding reported apart. Files are ranked by RRF of the
   per-source file rankings, as the prototype fuses files. `pointers` behind a `PointerGate`
   (`--pointer-min-cosine`, `--pointer-min-lexical`, default the garden's) is asked for every
   scored row and every row with `expected_source == "none"`; each mode reports the scored rows
   with a pointer to a gold file, those with pointers to other files only, and the `none` rows
   with any pointer.
6. A synthetic corpus and gold set (fictional people and places, 44 questions: 8 scored
   categories and 10 unrelated questions with `expected_source == "none"`) live in
   `crates/garden/tests/fixtures/`; a test runs the harness lexical-only and with the fake
   embedder and holds recall@3, MRR and p50 floors, and no pointer for an unrelated question at
   the default gate. The CI job `garden-eval` runs the
   release binary on them in both modes with `--min-recall` and `--max-p50-ms`, which fail the
   run (exit 1) when any reported mode misses a threshold; the thresholds sit in the job and are
   not below the test's floors.

## Ported from the reference agent

The research list of the owner (garden and retrieval items) names two small pieces of the
reference agent (`ref-agent` in `THIRD_PARTY_NOTICES.md`) to port as they are:

- `is_trivial_prompt` (`agent/memory_provider.py`): ported with its word list and trailing
  punctuation set. Deviation: Russian acknowledgements and greetings (`да`, `ок`, `спасибо`,
  `привет`, `понятно`, ...) are added, because the owner writes Russian and the English list
  alone would let "спасибо" fetch pointers.
- `rewrite_memory_query` (`plugins/memory/query_rewrite.py`) asks an auxiliary LLM to turn the
  message into one English question about the user and validates the answer. Tier (b) runs
  without an LLM on every request, so the rewrite itself is not ported; tier (c) receives a
  query the model already wrote. Only its input bound is taken (`_bounded_user_message`: above
  4000 characters the first 3000 and the last 900), without the "middle omitted" marker,
  whose words would become search terms. The cheap cleanup of section 4 comes from the
  prototype instead and stays.

## Relation to the research list

- RRF over four signals (FTS5, bge-m3, entities/aliases, time): two rankings are fused.
  Entity names and aliases are columns of the FTS5 index (ADR-0044), so they count inside the
  lexical ranking, not as a ranking of their own; a separate entity ranking (exact name/alias
  hits via `Store::find_entities`) and a time ranking (a date window parsed from the query) are
  left for the sprint. `rrf` takes any number of rankings, so both are additions, not a
  redesign.
- L0/L1/L2 levels of domains: pointers here are per fact, as the owner's product decision asks
  ("there is a fact about X" with the fact id). The one-phrase L0 abstract per domain, which
  the research list puts into the prompt as the passive tier, needs domain summaries that do
  not exist yet; it is left for the sprint and would sit next to the per-fact pointers (domain
  line first, fact pointers under it), not replace them.
- icm (Rust, Apache-2.0) was read as the reference for FTS5 + vectors + RRF. Its `rrf_fuse`
  uses `k = 60`, normalises by the best possible score and adds a date-window ranking; it keeps
  the raw bm25 and similarity next to each fused hit. We keep the prototype's `rrf_k = 0`
  profile, which the owner's eval promoted; the idea of carrying raw per-source scores is what
  the pointer gate uses. No code was copied, so no attribution is needed.

## Deviations from the prototype

- The lexical index has one column of `unicode61` tokens (ADR-0044); the prototype's lemma
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
- The pointer gate is on raw per-source scores whose scale depends on the model and the
  corpus; its thresholds are untuned until the sprint measures them on the gold set.
