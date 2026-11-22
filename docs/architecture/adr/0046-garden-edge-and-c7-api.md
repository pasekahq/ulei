# ADR-0046: The garden's edge and its in-process C7 API (C7 0.2.0)

Status: accepted
Date: 2026-11-20
Affects: crates/garden, contract:C7

## Context

The garden (ADR-0043) accepts facts from many proposers: the owner, extraction from raw
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
2. A candidate enters the garden in two steps (ADR-0055 section 2). C7 `propose` runs the
   reject step below (an absent `entity` passes it there) and writes every candidate it does
   not refuse into the seedbed, the garden's day bed, as an unprocessed record; nothing else
   of this list runs then. When the
   night stage processes the record (`seedbed_done`, ADR-0055 section 5), `ulei_garden::edge`
   decides the record's candidate, or the candidates the night gives for it, in this order,
   with an absent `valid_from` defaulting to the record's write time:
   - reject: an empty text, entity or `source_ref`, a blank `attribute`, an inverted
     interval, an unknown domain, a `never_cache` kind (live data),
     or a value that looks secret in any field (`ulei_garden::secret::looks_secret`): a card
     number passing Luhn, an IBAN passing mod 97, a SNILS or a personal INN passing its
     checksum, a US SSN written `AAA-GG-SSSS` outside the never-issued ranges (area 000, 666
     and 9xx, group 00, serial 0000), a passport (9 or 10 digits), SNILS, INN or SSN number
     after its key (`паспорт`, `passport`, `СНИЛС`, `ИНН`, `SSN`) with labels such as `серия`
     and `номер` between, a phone number of 10 to 15 digits (after `+`, after the trunk `8`
     with eleven digits, with the area code in brackets, or dashed as `912-345-67-89` or
     `555-010-0199`), and a secret key (`password`, `pass`, `pw`, `PIN`, `code`,
     `seed phrase`, `пароль`, `код`, ...) followed within three words by a value after `:`,
     `=` or a dash, a word with a digit or symbol after `is` or `это`, a PIN-like token (four
     or more characters with a digit, or three digits after `cvv`, `cvc`, `pin`, `code`,
     `password`, `пин`, `код` or `пароль`) or digits spelled out in Russian or English words
     (four or more, three after those short keys: "one two three four", "ноль семь ноль
     семь", "сорок два семнадцать"). A number after a date preposition ("changed in 2030")
     is no value; one after a flat, floor, house, year or price word is a value when a secret
     key governs it ("пароль от дома 4821") and is never refused without one ("квартира 12");
     `zip` and `postal` codes and a `vault:` pointer are no values. Also a
     well-known key prefix and a long random token. With `secret_ref` the text may only
     mention the secret: the store's rule (ADR-0044 section 5) is checked before anything is
     written, and `secret_ref` itself must be a `vault:` reference, since anything else may
     be the value. Rejected candidates are not stored or logged; their queue row keeps only
     the reason.
   - entity: `entity` names an entity by id (a merged one stands for its survivor), else by
     name. A kind a name does not identify (event, visit, preference) always gets a new
     entity; a person, place or item, or a candidate without a kind, is looked up among the
     entities of its kind (of those three kinds without one), on the first of three levels
     that finds any: a name or alias equal after case folding and whitespace collapse, an
     equal transliteration (`ulei_garden::translit::key`: tokens of letters and digits,
     Russian and Serbian Cyrillic spelled in ASCII, Latin diacritics folded), or, for a
     transliteration of five characters or more, one insertion, deletion or substitution
     away from one of five or more. One entity found takes the fact, none creates a new
     one. Two or more go to the digest as `ambiguous_entity` with their ids, names and
     kinds in `entity_candidates`, and the processing answers `queued` with that reason; nothing
     is written until `resolve`, unless the candidate would be refused for any of them.
   - placement: the candidate holds over [`valid_from`, `valid_to`); without `valid_from` it
     starts at the record's write time, or where the equal statement it repeats starts. An open-ended version of an
     `attribute` that starts before a later version of it ends where that one begins, so old
     material fills in history and does not replace the present (the store clips the same
     way).
   - `duplicate`: a version the garden believes now has the same normalised text
     (case-folded, whitespace collapsed, trailing punctuation dropped), starts no later and
     ends at the same time. Its `source_ref` stays on the proposal row, and it raises the
     fact's proof count (section 5).
   - reconciliation: the rivals are the versions believed now whose valid interval overlaps
     the candidate's and that set the same `attribute` (the versions the store closes) or
     say the same text (a new interval for the same statement is an update). The candidate
     beats a rival of a lower source rank (owner > raw > internet > agent), or of an equal
     rank that does not start later; the first rival it does not beat makes it a
     `contradiction` with `conflicts_with` naming that rival. A winning candidate is written;
     the store closes and slices overlapping versions of the attribute (ADR-0044). The
     edge retracts the same-text rivals itself, in favour of the new fact
     (`superseded_by`), through `Store::supersede_by`, which keeps copies over the slices
     outside the candidate's interval: the time before it always, the time after it unless
     the candidate took its start from the rival (then it only moves the end). A
     restatement therefore never removes the present or earlier history.
   - `low_confidence`: `confidence` below `EdgeConfig::min_confidence` (default 0.6) goes to
     the digest; it wins over `contradiction` as the reason and keeps the conflicting id.
   - otherwise `accepted`: written with provenance and the lifetime of its `ttl_kind` from
     the store's TTL table. An unknown entity is created with the proposed `kind` (default
     `item`) and an id slugged from its name.
   - top-k reconciliation: a candidate without an `attribute` never conflicts at intake, so an
     accepted one that is not a secret pointer waits in `reconcile_queue` with its candidate,
     and the processing neither embeds it nor waits. Once it has a vector of the helper's model,
     the daemon's worker (ADR-0047 section 6) takes it once: the five current facts of its
     entity that hold at its valid time nearest to now, most like it by RRF of the lexical
     ranking for its text and the dense ranking by cosine to its vector
     (`search::similar`), leaving out facts still waiting themselves, so each pair is weighed
     when its later fact is. The first of them with a cosine of at least
     `EdgeConfig::reconcile_min_cosine` (`garden.edge.reconcile_min_cosine`, default 0.85) and
     another normalised text makes a `contradiction` digest item with `conflicts_with` naming
     it; the new fact stays written until the owner decides. Without a helper, or while it is
     down, nothing is reconciled and the queue keeps the fact.
   - `resolve`: the owner's `accept` or `replace` skips the rank rule. On a contradiction of
     top-k reconciliation `reject` retracts the new fact in favour of its rival
     (`superseded_by`), `accept` retracts the rival in favour of the new fact and keeps it
     outside the new fact's valid interval, and `replace` writes the replacement as below and
     retracts the new fact in favour of it; an `as_of` before the decision answers as before. A replacement passes
     the same refusal checks; a refused one, an ambiguous name included, is the error
     `ReplacementRefused` and the item stays open. `accept` of an `ambiguous_entity` item
     needs `entity`, one of its `entity_candidates`, and writes the fact to that entity;
     without it (`MissingEntity`) or with any other id, or an `entity` given to another
     action or item (`NotACandidate`), the item stays open and the daemon answers
     `400 invalid_request`.
3. The C7 `TtlKind` names are the model's `TtlKind` names; the table itself belongs to the
   store (ADR-0044).
4. The edge keeps its queue in the table `edge_proposal`, which the store's migrations create
   (ADR-0044). A queued row holds the candidate as C7 JSON so the owner sees what was
   proposed; a decided row drops it and keeps the outcome. A queued contradiction of top-k
   reconciliation also names the written fact in `fact_id`. Proposal ids are `p<n>`; the fact
   written for proposal `n` is `f<n>`; a proposal row of the night's processing names its
   seedbed record in `seed`. The processing of one seedbed record or a resolve is one unit of
   work of the store (`Store::atomically`): the proposal row, a new entity, the fact and the versions it closes
   or slices commit together, and a store failure leaves none of them and reaches the caller
   as an error. Before it writes, the edge runs the store's public checks
   (`Entity::validate`, `Fact::validate`, the expiry of its TTL table), so that a refusal
   names its reason; a write the store still refuses (a rival recorded after `now`) rolls
   back alone and is `invalid`, never `secret_value`.
5. `ulei_garden::api::Garden` is the in-process facade speaking C7 types. Retrieval goes
   through the `Retriever` trait, implemented by `api::Hybrid` over the hybrid search of
   `search.rs` and its reranker (ADR-0045). Secret-pointer and private facts are returned only
   with `private`. Pointers pass the `PointerGate` of `ApiConfig::pointers` and return at most
   three, one per entity; finding none queues no miss (ADR-0047 section 3b). A search miss
   never repeats a query that looks secret. A trivial prompt (`search::is_trivial_prompt`,
   ported from the reference agent: empty, a slash command, or one bare greeting or
   acknowledgement) is not searched and yields no miss, on both tiers. A returned fact carries
   `proof_count`: 1 plus the `duplicate` proposals that named it or a version a merge of
   entities copied into it (ADR-0044), so a merge carries the count to the copy on the
   survivor; every other new version starts at 1.
6. Deviations from the research the garden follows, deliberate for this sketch:
   - A confirmation counts only in `proof_count`; facts carry no trust or helpfulness
     counters, C7 has no feedback method, and no quotes are derived from the duplicates,
     whose `source_ref` stays on their proposal rows.
   - The memory query rewrite of the reference agent is not ported: it asks a model, and the
     garden reaches no model (models only through the gateway; the passive tier uses none).
     It is left for the step that gives the active tier a model through the gateway.
   - Person dossiers (a summary card per person refreshed in the background, and a "what is
     known about X" operation) are not built; C7 has no such method yet.
   - The passive tier points at single facts; per-domain one-line abstracts (L0), an
     overview on request (L1) and facts by search (L2) are not built.

## Consequences

- The owner sees only contradictions and doubts; routine facts flow in at the night's
  processing of the seedbed, and the day's candidates are found there until then (ADR-0055).
- The pinned core profile (tier a) and the transport of the C7 methods are not part of
  this decision.
- Secret detection is heuristic: it errs towards rejecting, and the proposer can always
  rephrase with `secret_ref`. The store adds its own stricter check on secret-pointer facts.
