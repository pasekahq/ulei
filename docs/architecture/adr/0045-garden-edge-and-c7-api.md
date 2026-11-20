# ADR-0045: The garden's edge and its in-process C7 API (C7 0.2.0)

Status: accepted
Date: 2026-11-20
Affects: crates/garden, contract:C7

## Context

The garden (ADR-0042) accepts facts from many proposers: the owner, extraction from raw
material, the internet and agents. Most candidates are routine and must not reach the owner;
a contradiction with a currently held fact, or a candidate its proposer doubts, must. Secret
values must never enter. Callers need two retrieval tiers: passive pointers on every request
(no LLM, no fact text) and the active `garden_search` tool. C7 0.1.0 had only `search` and a
bare `propose`.

## Decision

1. C7 0.2.0 is additive: optional fields on `Fact`, `ProposeRequest`, `ProposeResponse`, and
   new definitions for the passive tier (`PointersRequest`, `Pointer`, `PointersResponse`, at
   most three pointers without fact text), the digest (`DigestRequest`, `DigestItem`,
   `DigestResponse`, `ResolveRequest`) and their enums. `x-methods` names `search`,
   `pointers`, `propose`, `digest`, `resolve`; their transport is decided when a daemon serves
   them. A miss stays a C1 `garden_miss` event.
2. `ulei_garden::edge` decides every candidate in this order:
   - reject: an empty text or entity, an inverted interval, an ambiguous entity name, a
     `never_cache` kind (live data), or a value that looks secret (a card number passing
     Luhn, an IBAN passing mod 97, an assignment such as `password: ...` or
     `door code: ...`, a well-known key prefix, a long random token). With `secret_ref` the
     text may only mention the secret. Rejected candidates are not stored or logged; their
     queue row keeps only the reason.
   - `duplicate`: the entity already holds a current fact with the same normalised text
     (case-folded, whitespace collapsed, trailing punctuation dropped).
   - with an `attribute`, a current fact of the same entity and attribute is a conflict
     unless the candidate's source rank (owner > raw > internet > agent) is higher, or equal
     with a `valid_from` that is not older. A winning candidate is written and the store
     closes the old version (ADR-0043); a losing one is a `contradiction`.
   - `low_confidence`: `confidence` below `EdgeConfig::min_confidence` (default 0.6) goes to
     the digest; it wins over `contradiction` as the reason and keeps the conflicting id.
   - otherwise `accepted`: written with provenance and the lifetime of its `ttl_kind` from
     the store's TTL table. An unknown entity is created with the proposed `kind` (default
     `item`) and an id slugged from its name.
3. The C7 `TtlKind` names are the model's `TtlKind` names; the table itself belongs to the
   store (ADR-0043).
4. The edge keeps its queue in its own table `edge_proposal` on the store's connection,
   created with `CREATE TABLE IF NOT EXISTS`. A queued row holds the candidate as C7 JSON so
   the owner sees what was proposed; a decided row drops it and keeps the outcome. Proposal
   ids are `p<n>`; the fact written for proposal `n` is `f<n>`.
5. `ulei_garden::api::Garden` is the in-process facade speaking C7 types. Retrieval goes
   through the `Retriever` trait; the first implementation is lexical over the store's FTS5.
   The hybrid retriever of `search.rs` plugs into the same trait. Secret-pointer and private
   facts are returned only with `private`. Pointers apply a score threshold
   (`ApiConfig::pointer_min_score`) and return at most three; when none passes, the API
   returns a `garden_miss` value the caller signs and publishes through C1. A query that
   looks secret is not repeated in the miss.

## Consequences

- The owner sees only contradictions and doubts; routine facts flow in.
- The pinned core profile (tier a) and the transport of the C7 methods are not part of
  this decision.
- Secret detection is heuristic: it errs towards rejecting, and the proposer can always
  rephrase with `secret_ref`. The store adds its own stricter check on secret-pointer facts.
