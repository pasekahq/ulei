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
   - reject: an empty text, entity or `source_ref`, a blank `attribute`, an inverted
     interval, an ambiguous entity name, an unknown domain, a `never_cache` kind (live data),
     or a value that looks secret in any field: a card number passing Luhn, an IBAN passing
     mod 97, a secret key (`password`, `PIN`, `door code`, `пароль`, `код`, ...) with a value
     after `:` or `=` up to three words later or a PIN-like token (four or more characters
     with a digit) right after it, a well-known key prefix, a long random token. With
     `secret_ref` the text may only mention the secret: the store's rule (no run of four
     digits, no long mixed word) is checked before anything is written, and `secret_ref`
     itself must be a `vault:` reference, since anything else may be the value. Rejected
     candidates are not stored or logged; their queue row keeps only the reason.
   - placement: the candidate holds over [`valid_from`, `valid_to`); without `valid_from` it
     starts now, or where the equal statement it repeats starts. An open-ended version of an
     `attribute` that starts before a later version of it ends where that one begins, so old
     material fills in history and does not replace the present (the store clips the same
     way).
   - `duplicate`: a version the garden believes now has the same normalised text
     (case-folded, whitespace collapsed, trailing punctuation dropped), starts no later and
     ends at the same time. Its `source_ref` stays on the proposal row.
   - reconciliation: the rivals are the versions believed now whose valid interval overlaps
     the candidate's and that set the same `attribute` (the versions the store closes) or
     say the same text (a new interval for the same statement is an update). The candidate
     beats a rival of a lower source rank (owner > raw > internet > agent), or of an equal
     rank that does not start later; the first rival it does not beat makes it a
     `contradiction` with `conflicts_with` naming that rival. A winning candidate is written;
     the store closes overlapping versions of the attribute (ADR-0043), and the edge retracts
     earlier versions of the same text first.
   - `low_confidence`: `confidence` below `EdgeConfig::min_confidence` (default 0.6) goes to
     the digest; it wins over `contradiction` as the reason and keeps the conflicting id.
   - otherwise `accepted`: written with provenance and the lifetime of its `ttl_kind` from
     the store's TTL table. An unknown entity is created with the proposed `kind` (default
     `item`) and an id slugged from its name.
   - `resolve`: the owner's `accept` or `replace` skips the rank rule. A replacement passes
     the same refusal checks; a refused one is the error `ReplacementRefused` and the item
     stays open.
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
   looks secret is not repeated in the miss. A trivial prompt (`search::is_trivial_prompt`,
   ported from the reference agent: empty, a slash command, or one bare greeting or
   acknowledgement) is not searched and yields no miss, on both tiers.
6. Deviations from the research the garden follows, deliberate for this sketch:
   - Conflicts are decided at intake by exact `attribute` equality and equal text, not
     ADD-only intake with a later reconciliation over the top-k similar facts (Mem0-style).
     A candidate without an `attribute` never conflicts ("Ada is vegetarian" against "Ada
     eats meat"). Top-k similarity reconciliation is left for the next step.
   - A duplicate is not counted as a confirmation and facts carry no trust or helpfulness
     counters; C7 has no feedback method. The duplicate's `source_ref` is kept on its
     proposal row so that a `proof_count` with quotes can be derived later.
   - The memory query rewrite of the reference agent is not ported: it asks a model, and the
     garden reaches no model (models only through the gateway; the passive tier uses none).
     It is left for the step that gives the active tier a model through the gateway.
   - The store opens its own transaction per write, so the edge cannot wrap the proposal
     row, a new entity and the fact in one. Instead it runs the store's public checks
     (`Entity::validate`, `Fact::validate`, the expiry of its TTL table) before it writes
     anything, and a proposal row whose write failed is marked `failed`.

## Consequences

- The owner sees only contradictions and doubts; routine facts flow in.
- The pinned core profile (tier a) and the transport of the C7 methods are not part of
  this decision.
- Secret detection is heuristic: it errs towards rejecting, and the proposer can always
  rephrase with `secret_ref`. The store adds its own stricter check on secret-pointer facts.
