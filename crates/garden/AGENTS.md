# AGENTS.md — `ulei-garden`

The garden: typed facts about the owner's life with two time axes and provenance, the edge
(intake queue) in front of it, and hybrid retrieval over it (C7). ADR-0043.

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
  fake embedder and never download models.
- A garden miss is a C1 `garden_miss` event.
- Talk to other modules only through `ulei-contracts` and the wire; a new dependency needs an ADR.
- Check: `python3 -m ci.broad_check` from the repository root.
