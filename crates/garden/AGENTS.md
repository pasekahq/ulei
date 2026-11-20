# AGENTS.md — `ulei-garden`

The garden: typed facts about the owner's life with two time axes and provenance, the edge
(intake queue) in front of it, and hybrid retrieval over it (C7). ADR-0043; as a live module,
ADR-0047.

Read the root `AGENTS.md` first. Rules specific to this crate:

- `model` is the shared vocabulary of the crate; storage, retrieval, the edge and the C7 API
  build on it and do not redefine its types.
- A secret never enters the garden: a fact may only say that a secret exists and point to the
  vault entry (`Sensitivity::Secret`). No code path stores, logs or returns a secret value.
- Facts are never deleted or edited in place: a change closes the old row on the transaction
  axis and writes a new one, so every past answer can be reproduced (`as_of`).
- `store` owns the SQLite schema (ADR-0044): a schema change is a new entry at the end of
  `MIGRATIONS`, never an edit of an applied one. Secret facts never reach `fact_fts` or
  `fact_vectors`.
- Embedding models sit behind a trait and are optional at run time; tests use a deterministic
  fake embedder and never download models. No model weights in the repository.
- The daemon `ulei-garden serve` is the only process that opens the live store; other modules
  reach the garden over C7 on its socket (ADR-0047 sections 1 and 2). Its settings come from
  the `[garden]` keys of `ulei.toml` through `ulei_contracts::config`; a new key is a C9 change.
- The daemon is a static musl binary and links no ONNX Runtime; the model runs only in the
  helper `ulei-garden-embed`, a glibc build behind a pipe. With the embedder enabled, a model
  that does not load stops the start, and a lexical-only answer is always marked as such
  (ADR-0047 sections 1 and 2).
- A request embeds only its own query and never calls the second level or waits for the inbox;
  everything else runs on the daemon's worker thread, which sleeps until its nearest deadline
  (ADR-0047 section 6).
- A garden miss is a C1 `garden_miss` event, signed with the producer key and published to the
  inbox by the garden itself (ADR-0047 section 5).
- The garden reaches the forest, the internet and language models only over the wire: no TLS
  stack and no client of a model gateway or vendor in this crate; the embedder is the only
  model it runs, in its helper.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
