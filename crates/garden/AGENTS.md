# AGENTS.md — `ulei-garden`

The garden: typed facts about the owner's life with two time axes and provenance, the edge
(intake queue) in front of it, and hybrid retrieval over it (C7). ADR-0043; as a live module,
ADR-0047. What it is and how to run it: `README.md`.

The model sees the garden through `ulei-engine` (ADR-0077): the core profile file in the
prompt, C7 `pointers` on owner turns, and C7 `search` and `propose` behind the tools
`garden_search` and `garden_propose`. The second-level providers over the wire are not built
(ADR-0047 section 9): `garden.second_level` accepts only `none`. Both binaries are released,
installed and run by `deploy/systemd/ulei-garden.service` (ADR-0075). Of the other modules,
only the engine calls the daemon today: its turns, and the night gardener `ulei-engine night`
(ADR-0078), run by the user timer `ulei-night.timer`, which calls `seedbed_take`,
`seedbed_done`, `forget_take` and `forget_done`; until it processes them, seedbed records do not
become facts and the digest receives nothing new from them.

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
  `ttl_kind`. A record never gets a vector. While a record waits, its index row follows its
  resolved entity's name, aliases and merge, as the facts' rows do; a processed record keeps
  its names.
- `propose` is the seedbed write, `Edge::seed`, one unit of work (ADR-0055 sections 2 and 4):
  the edge's refusal screen runs, and a candidate it refuses or one already past its expiry is
  `rejected` with only its proposal row. A candidate equal to the one of a stored record not
  yet expired (the text after `edge::normalise`, every other field exactly) writes nothing and
  answers that record. Otherwise one record is written; for a candidate with `entity` and
  `attribute` and without `secret_ref` its entity is the one `Edge::name`, called read only,
  finds, and it disputes the first by `recorded_at` of that entity's versions of the attribute
  believed and valid at the write with another normalised text. The write creates no fact,
  entity, digest item, proposal row or C1 event. The worker's second-level answers and
  rechecks are written by `Edge::seed` too, in process. `Edge::propose` decides a candidate at
  once in a unit of work of its own; the daemon never calls it, and `Edge::process_seed` makes
  the same decision inside the record's unit of work.
- `forget` (ADR-0051 section 5, ADR-0055 section 6) is one unit of work of the store
  (`Store::forget`, `Store::unforget`). A seedbed record is forgotten while one of its rows of
  `seed_marks` is not lifted; a mark is never deleted and a forgotten record gets no second live
  mark. `Store::lexical` and `Store::unprocessed_seeds` leave a forgotten record out, so
  `search`, `pointers` and `seedbed_take` do, and `seedbed_done` answers it `not_found`. Every
  ref queues in `forget_queue`, apart from `reconcile_queue`, the current facts whose
  `source_ref` is the ref or, for `seed:<seq>`, that an `accepted` proposal row naming the
  record in `seed` wrote; a row waiting under the same ref is not queued again, and `undo`
  removes only rows without `taken_at`. `c8:` refs only queue; `archive:` refs are refused.
  Garden facts are never marked: the night clears them through `forget_take` and
  `forget_done`.
- `forget_take` only reads: the rows without `taken_at` after `after` by `id`. `forget_done`
  checks every id before it clears any, then runs `Store::clear_forgotten` per row, one unit
  of work each: a row already taken changes nothing and answers `closed: false`; otherwise its
  fact, when still current, is closed on the transaction axis with no new version (a
  retraction; its pin, index row and vectors stay for `as_of`), its re-check is dropped and the
  row gets `taken_at`. A failed row leaves the rows before it cleared and is answered, counted
  and logged as a failed `seedbed_done` item (`garden: forget_done failed: <class>`); a call
  that closed a fact renders the core profile and wakes the worker, which drops the closed
  versions from the resident matrix and from `reconcile_queue`.
- `seedbed_take` only reads, the records after `after` by `seq` when given. `seedbed_done`
  checks every ref before it processes any, then runs `Edge::process_seed` per record, one
  unit of work each: the edge decides the given candidates, else the record's own, a candidate
  without `valid_from` starting at the record's `written_at`; every `edge_proposal` row it
  writes names the record in `seed`; the record is marked processed with the answers, and a
  processed record answers them again and writes nothing (ADR-0055 section 5). A failed item
  leaves the items before it processed: the daemon answers `store_failed`, counts it in
  `health.failed_proposes` with one stderr line `garden: seedbed_done failed: <class>`, and
  still renders the core profile and wakes the worker for the facts the processed items wrote.
- The places bed (ADR-0080) is written only through `places::Bed`, one unit of work per call; a
  refusal is a `places::Refusal` and writes nothing. Its tables are the store's; a type is its
  `attr_policy` rows, and a key without a row of the place's type is refused. A place written by
  name is the one bed place whose name or alias equals it, two or more are refused; without one,
  it is the one place entity `Edge::name` finds when it has no bed row, otherwise a new entity.
  A found place keeps its status on a `candidate` write and takes `active` or `gone`. A
  `bed_attr` row is the current value of its key with its own `checked_at`, `source` and
  `confidence`; `num` is the number of a `number` or `count` and the low end of a `range`, and
  `due_at` is `checked_at` plus `ttl_days` days, null without a TTL. A newer observation moves
  the row it replaces into `bed_attr_history` with `replaced_at`, an older one goes into the
  history only, and one already stored writes nothing. `bed_entity`'s `lat` and `lon` and the
  entity's `PlaceAttrs` follow the current `location` and `osm_id`. `bed_verdict` is append-only
  with one row per place and `source_ref`; the latest by `said_at`, then `seq`, is the place's
  verdict. `bed_profile` keeps one row per key apart from `ban`. A value of a free form (`list`,
  `hours`, `url`, `text`), a verdict's note, a free-form profile value, a name, `source` or
  `source_ref` that `secret::may_carry_credential` flags is refused. A C7 `places_write` batch
  is one unit of work in which each place and observation is a nested one, so a refusal skips
  only its own write. Bed rows never enter facts, `fact_fts`, vectors, the seedbed, pointers,
  the core profile or a C1 event.
- `places_search` (`places_search::search`) only reads the bed: one SQL query applies the status,
  the bounding box, the rating and price thresholds and the latest verdict, and Rust applies the
  haversine distance, cuisine and bans and the order of ADR-0080 section 5.
- `ulei-garden places import` (`places_import`) is a client of the daemon's socket, never a
  second opener of the live store: it checks a file by the syntax of ADR-0080 section 4 and by
  rehearsing its C7 writes on a `Store::open_in_memory` garden, and writes only a file without a
  refused line, through `places_write`, `places_verdict` and `places_profile` (`client::post`).
- Embedding models sit behind a trait and are optional at run time; tests use a deterministic
  fake embedder, in process or as `ulei-garden-embed --embedder fake` over synthetic artifact
  files, or the `native` backend on a tiny checkpoint converted at run time
  (`tests/tiny_bert`), and never download models. No model weights in the repository. The ONNX path runs in
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
  only libc, libm, ld-linux and libgcc_s. Its second backend `native` (feature `native`,
  ADR-0087 E6) runs the encoder engine of the library crate `ulei-infer` (feature `bert`), the
  one library this crate links (ADR-0072), on the weight file `embed::ENGINE_FILE` in the
  model directory, with the tokenizer, pooling and query and passage prefixes of its header,
  keeping the engine's activation buffers (`bert::Workspace`) from call to call and releasing
  them after a call that left them above `embed::native::KEEP_BYTES` (the idle row of
  ADR-0087 section 6); built with `native` alone it is a static musl binary, `cargo build --release --locked -p
  ulei-garden --features native --bin ulei-garden-embed --target x86_64-unknown-linux-musl`,
  held to the same size row and checked to be static. The helper picks its backend by the model
  file in the model directory (`--embedder auto`, the default; a directory with both files is
  refused), so the daemon's command line is the same for both. `embed::pipe` is the one
  protocol between daemon and helper: the helper's ready line names the model and the SHA-256
  of both artifacts (for `native`, twice the content digest sealed in the weight file, which
  holds the tokenizer; ADR-0087 E6c), which enter the model id of the stored vectors. The
  `native` start reads no blob of the weight file: it checks the preamble, the file's length
  and the header and refuses a file that fails them as "model file corrupted" (`embed::digest`).
  The helper's subcommand `convert` (ADR-0087 section 2) writes the engine's int8 weight file
  from an fp32 `safetensors` checkpoint and seals its content digest; `verify` hashes a
  weight file again against that digest. Both are one-shot file commands, not part of the
  pipe. With the embedder enabled, a helper that does not get ready stops
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
- Hits and pointers of `search` are `store::Indexed`: a record enters RRF through the lexical
  ranking alone and passes the same `PointerGate`. `api` answers a record as a C7 `Fact` or
  `Pointer` with `fact_id` its ref and `unprocessed`, never with `pinned`, `proof_count` or
  `recheck_before_acting`; a record pointer is about its resolved entity as it stands after a
  merge, else its name as written, trimmed and keyed after `edge::normalise`, and a record with
  neither gets none (ADR-0055 section 3). `search::pointers` resolves the entity, name and kind
  of every pointer, for facts and records alike; `api` only formats them.
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
  second level (`second::Source`), never a query that looks secret. A row with a candidate of
  the second level stored as a seedbed record is removed unpublished, so the record answers
  the same question the same day; the worker signs every other row
  as a C1 `garden_miss` with the producer key and publishes it to the inbox itself, retrying
  until the inbox stores it. `pointers` and a `search` with `no_miss` queue no miss (ADR-0047
  section 5).
- An expired fact waits in `rechecks` until a candidate the worker's second level finds for it
  is stored as a seedbed record, as a new version of the same entity and attribute; that clears
  it in the same unit of work, a refused candidate clears nothing, and the version is written
  when the night processes the record (ADR-0047 section 6).
- The pinned core profile is `profile::Profile`: the request path renders it after every
  `resolve` and `seedbed_done` and every `forget_done` that closed a fact, and the worker after
  every tick, both under its one lock, to `garden.core_file` through a 0600 temporary file
  renamed over it. The store keeps the pins in `pins`, one row per pinned version; every copy of
  a pinned version is pinned with it, and a closed version keeps its pin for `as_of`. Only the
  owner pins, and a secret pointer is never pinned or rendered (ADR-0047 section 3a).
- The garden reaches the forest, the internet and language models only over the wire: no TLS
  stack and no client of a model gateway or vendor in this crate; the embedder is the only
  model it runs, in its helper.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
  The current ones: `rusqlite` and `serde_json` (ADR-0044), `rustix` (ADR-0048), `hmac` and
  `sha2` (ADR-0049), `ort` and `spm_precompiled` behind `onnx` (ADR-0045), the library crate
  `ulei-infer` (ADR-0087, ADR-0072).
- Check: `python3 -m ci.broad_check` from the repository root.
