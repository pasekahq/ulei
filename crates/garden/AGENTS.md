# AGENTS.md — `ulei-garden`

The garden: typed facts about the owner's life with two time axes and provenance, the edge
(intake queue) in front of it, and hybrid retrieval over it (C7). ADR-0043; as a live module,
ADR-0047. What it is and how to run it: `README.md`.

The model does not see the garden yet: the engine step (the core profile in the prompt,
pointers on every owner turn, the tools `garden_search` and `garden_propose`), the unit,
release and install of `ulei-garden` and `ulei-garden-embed`, and the second-level providers
over the wire are issue:76b6f0e180339e492edd (ADR-0047 section 9). `garden.second_level`
accepts only `none`, and no other module calls the daemon.

Read the root `AGENTS.md` first. Rules specific to this crate:

- `model` is the shared vocabulary of the crate; storage, retrieval, the edge and the C7 API
  build on it and do not redefine its types.
- A secret never enters the garden: a fact may only say that a secret exists and point to the
  vault entry (`Sensitivity::Secret`). No code path stores, logs or returns a secret value.
- Facts are never deleted or edited in place: a change closes the old row on the transaction
  axis and writes a new one, so every past answer can be reproduced (`as_of`).
- `store` owns the SQLite schema (ADR-0044): a schema change is a new entry at the end of
  `MIGRATIONS`, never an edit of an applied one; the pending entries run in one transaction.
  Secret facts never reach `fact_fts` or `fact_vectors`.
- A seedbed record (ADR-0055 section 1) is never deleted; its only change is setting
  `processed_at` and `outcome` once. A non-secret record is the row `-seq` of `fact_fts`, so
  `Store::lexical` ranks records among facts and filters them by both time axes (written to
  processed), expiry and sensitivity; its expiry is `TtlTable::expiry` of the edge's
  `ttl_kind`. A record never gets a vector.
- Embedding models sit behind a trait and are optional at run time; tests use a deterministic
  fake embedder, in process or as `ulei-garden-embed --embedder fake` over synthetic artifact
  files, and never download models. No model weights in the repository. The ONNX path runs in
  the CI job `garden-onnx` on the tiny model `ci/garden_onnx.py` writes at run time, with the
  ONNX Runtime library of the package pinned there by version and SHA-256 (ADR-0047 section
  10): the ignored tests of `tests/onnx_tiny.rs` with `ULEI_GARDEN_TINY_MODEL_DIR` and
  `ULEI_GARDEN_ORT_LIB` set, then the musl daemon with the real helper.
  `tests/tokenizer_golden.rs` needs the real bge-m3 `tokenizer.json` and stays ignored. The
  real model on the owner's corpus and gold set runs only outside the repository, by
  `docs/runbooks/garden-gold.md` against the targets of ADR-0050.
- The daemon `ulei-garden serve` is the only process that opens the live store; other modules
  reach the garden over C7 on its socket (ADR-0047 sections 1 and 2). Its settings come from
  the `[garden]` keys of `ulei.toml` through `ulei_contracts::config`; a new key is a C9 change.
- The daemon is a static musl binary and links no ONNX Runtime, built without `onnx` by
  `cargo build --release --locked -p ulei-garden --bin ulei-garden --target
  x86_64-unknown-linux-musl` and held to the `ulei-garden` section of `ci/budgets.sh`; the
  model runs only in the helper `ulei-garden-embed`, a glibc build with `onnx` by `cargo build
  --release --locked -p ulei-garden --features onnx --bin ulei-garden-embed --target
  x86_64-unknown-linux-gnu`, held to `ULEI_GARDEN_EMBED_MAX_BINARY_BYTES` and to `ldd` listing
  only libc, libm, ld-linux and libgcc_s. `embed::pipe` is the one protocol between them: the
  helper's ready line names the model and the SHA-256 of both artifacts, which enter the model
  id of the stored vectors. With the embedder enabled, a helper that does not get ready stops
  the start. After the start `embed::supervisor` holds it ready or down: a call that breaks the
  pipe or misses `pipe::CALL_DEADLINE` kills and reaps it, the worker restarts it after a pause
  of 1 s doubling up to 60 s, back to 1 s only for a helper that fell `supervisor::STEADY`
  seconds or more after its restart, and while it is down `search` and `pointers` answer
  lexical with `lexical_only: true`, never `store_failed` for the helper (ADR-0047 sections 1
  and 2).
- `api::Garden` retrieves through `api::Hybrid`, the search of `search` behind `api::Retriever`.
  A reranker reorders the fused hits behind `rerank::Reranker` before the cut to `k` or to
  pointers and cannot add or drop one; this build has only `none`. A pointer needs the evidence
  of `PointerGate`, whose thresholds come from `garden.api.pointer_min_cosine` and
  `garden.api.pointer_min_lexical` (ADR-0045 section 3, ADR-0047 section 4).
- A request embeds only its own query and never calls the second level or waits for the inbox;
  everything else, helper restarts and the passages of facts without a vector of the helper's
  model and the top-k reconciliation of new facts without an attribute
  (`Edge::reconcile_similar`) included, runs on the daemon's worker thread (`worker`), on its
  own store connection, which sleeps until its nearest deadline. The worker embeds current
  versions first, then reconciles, then embeds the closed versions an `as_of` search can reach.
  It sends one passage or reconciles one fact per tick, only with the helper ready and after
  `worker::QUIET` seconds without a `search` or `pointers`, and a waiting query takes the pipe
  before a waiting passage. A passage that failed `store::PASSAGE_TRIES` times under the
  helper's model is skipped, one stderr line without its text, and a failed one goes behind
  the others. The worker and the request path read time only through `worker::Clock`
  (ADR-0047 section 6).
- The dense side of the daemon scans `matrix::Matrix`, the resident f32 rows of the helper's
  model that only the worker writes: it rebuilds them, deleting the vectors of every other
  model, when the helper's model changes and syncs them with the store after every tick that
  changed it. A search known before the last sync, an `as_of` in the past, reads the vectors
  from SQLite (ADR-0047 section 3c).
- The edge resolves a candidate's entity name only among people, places and items (all three
  for a candidate without a kind): an equal normalised name, else an equal `translit::key`,
  else a key one edit away when both have five characters or more. Two or more entities on the
  first level that finds any make an `ambiguous_entity` digest item the owner settles with
  `resolve` and `entity`; the edge never picks one (ADR-0046 section 2). `translit` is the
  crate's only transliteration.
- `fact_fts` holds for every non-secret version its text, its entity's name and aliases (for a
  record: the name as written and the resolved entity's name and aliases), the
  stems of their Russian tokens and their `translit::key`; a query meets each column with the
  same form of its tokens. `morph`, a port of the Snowball Russian stemmer, is the crate's only
  stemmer, without a dictionary. The bm25 column weights and the default of
  `garden.api.pointer_min_lexical` are set together on the synthetic eval: a change of one
  retunes the other (ADR-0047 sections 4 and 8).
- The outermost unit of work of the store starts with `BEGIN IMMEDIATE`: a new writing store
  method opens its transaction through the store's unit, never a deferred one.
- A `search` miss is a row of the miss outbox (`miss`), which holds the query's HMAC under
  the producer key (never an unkeyed hash) and the query only until the worker hands it to the
  second level (`second::Source`), never a query that looks secret. A row the second level
  answers with a fact the edge accepts is removed unpublished; the worker signs every other row
  as a C1 `garden_miss` with the producer key and publishes it to the inbox itself, retrying
  until the inbox stores it. `pointers` queues no miss (ADR-0047 section 5).
- An expired fact waits in `rechecks` until the worker's second level finds a new version the
  edge accepts, of the same entity and attribute; only that clears it (ADR-0047 section 6).
- The pinned core profile is `profile::Profile`: the request path renders it after every
  `propose` and `resolve` and the worker after every tick, both under its one lock, to
  `garden.core_file` through a 0600 temporary file renamed over it. The store keeps the pins in
  `pins`, one row per pinned version; every copy of a pinned version is pinned with it. Only the
  owner pins, and a secret pointer is never pinned or rendered (ADR-0047 section 3a).
- The garden reaches the forest, the internet and language models only over the wire: no TLS
  stack and no client of a model gateway or vendor in this crate; the embedder is the only
  model it runs, in its helper.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
  The current ones: `rusqlite` and `serde_json` (ADR-0044), `rustix` (ADR-0048), `hmac` and
  `sha2` (ADR-0049), `ort` and `spm_precompiled` behind `onnx` (ADR-0045).
- Check: `python3 -m ci.broad_check` from the repository root.
