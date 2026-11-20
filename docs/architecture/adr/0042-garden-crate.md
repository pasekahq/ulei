# ADR-0042: The garden and its retrieval as the crate `ulei-garden`

Status: accepted
Date: 2026-11-20
Affects: crates/garden, crates/memory, dep:ulei-contracts

## Context

ADR-0002 placed the garden, retrieval and the forest in one module, `ulei-memory`, which is
still an empty skeleton. The garden is now designed in detail: a small fixed set of entity
kinds, domains bound to Wikidata classes, two time axes, provenance, expiry of internet facts,
secrets only by reference, an edge (intake queue) that accepts facts on its own and leaves only
contradictions and doubts to the owner, and hybrid lexical + dense retrieval behind C7. The
forest (cold archive of raw sessions) has a different life cycle: append-only bulk data read
through C6 `raw_sessions`, rebuilt from the runtime, no reconciliation.

## Decision

1. A new class-2 crate `ulei-garden` in `crates/garden` holds the garden, its edge and its
   retrieval, and speaks C1 (`garden_miss`) and C7. Like every module it depends only on
   `ulei-contracts`.
2. `ulei-memory` keeps the forest. The row "memory (garden + retrieval + forest as one
   module)" of ADR-0002 now reads as two class-2 modules: `ulei-garden` and `ulei-memory`.
3. `ulei_garden::model` is the vocabulary of the crate: `EntityKind` (person, place, event,
   item, preference, visit), `Domain` with an optional Wikidata QID, `Fact` with `valid_from`,
   `valid_to`, `recorded_at`, `superseded_at`, `Provenance`, a `TtlKind` whose row of the
   `TtlTable` gives `expires_at` (ADR-0043 decision 3) and `Sensitivity` (public, private,
   secret by vault reference only).
4. Storage, embedder, search, the edge and the C7 API are separate modules of this crate; each
   new dependency they need comes with its own ADR.

## Consequences

- A fact is never edited in place: a change closes the old version on the transaction axis,
  so `as_of` answers are reproducible.
- No secret value can be stored: the model has no field for it.
- Clustering of new domains, recommenders built on preferences and visits, background
  watchers and GPU profiles are outside this crate's first version.
