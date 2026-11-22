# ADR-0055: The garden's seedbed: the day's unprocessed records, one write path through it, same-day retrieval, the disputed flag, the night batch (C7 0.5.0), and its cards

Status: accepted
Date: 2026-11-22
Affects: crates/garden, contract:C7

## Context

The issue of the seedbed (issue:af784fb6649fa299fc64, sprint item d7) carries three decisions of
the owner of 2026-11-21, quoted below by their time, and five scope items:

- 11:04, the seedbed is the garden's day bed. New facts of the day (what the agent found,
  "remember this", task results) go into a separate bed at once. It takes part in retrieval
  during the day but is not processed: FTS5 only, no dedup, clustering or placement into
  domains. The night process takes the seedbed: dedup, vectors, clustering, reconciliation with
  the garden, placement, contradictions into the digest.
- 11:08, a contradiction with the garden. By default a fresh fact is marked "disputed" until the
  night review. The flag is no reason to write to the owner: when the context shows the fresh
  fact is more accurate, the agent uses it and ignores the flag; when a disputed fact surfaces
  unexpectedly and the context does not settle it, the agent asks the owner in one phrase or sends
  a background agent for data.
- 11:01 to 11:04: writing to the garden is not the agent's task; the agent has only the handle
  `garden_propose` ("remember this", the owner's corrections).

Scope items: (1) the store: append-only records of the day with provenance (source, ref, time),
FTS5 only, no embeddings and no LLM; (2) the way in: `garden_propose` and the other sources of
the day (found by the agent, task results, lifted from the archive) put a record into the
seedbed, the relation to the edge's automatic intake (ADR-0046) is decided here, and no two
parallel write paths remain; (3) `garden_search` and the passive pointers find the records the
same day, marked unprocessed; (4) the disputed flag: a cheap check without an LLM at write time,
a conflict with a current garden fact on the same entity and attribute, visible in the output,
never sent to the owner, never in the digest before the night review; (5) the interface of the
night stage: take a batch, mark it processed. The night gardener itself
(issue:33b6bfbfefaa49d21a4a) and the seedbed's evals (issue:2dd2fcc6e7338d3db34a) are outside.

What exists on main:

- The edge (ADR-0046 section 2) decides every candidate of C7 `propose` at once: refusal, entity
  resolution, placement, duplicate, reconciliation by source rank, doubt, then a write into
  `facts` or a digest item for the owner. The daemon's worker passes the candidates of the second
  level (ADR-0047 section 5) and of rechecks (ADR-0047 section 6) through the same edge.
- The store (ADR-0044) is one SQLite file with a contentless FTS5 table `fact_fts` (text, names,
  stems, translit; rowid the fact's `seq`), ranked by bm25 and filtered by both time axes and
  sensitivity. Search is RRF of that lexical ranking and the dense one (ADR-0045 section 3); a
  passive pointer needs a cosine of at least `pointer_min_cosine` or a lexical score (-bm25) of
  at least `pointer_min_lexical` (`PointerGate`).
- C7 0.4.0 methods: `search`, `pointers`, `propose`, `digest`, `resolve`, `health`. No engine
  tool calls the garden yet (ADR-0047 section 9); C2 has `garden_search` and `garden_propose`.

ADR-0051 needs two things from this record: a seedbed record shape with room for a forget mark
beside it and the ref form C7 `forget` takes (its section 5, card 11), and the seedbed's write
method of C7 for lifting archive hits with source `archive`, ref `archive:<id>`, the item's `at`
and its text bounded to 1,200 characters (its section 6, card 12).

## Decision

### 1. The store of the seedbed

A record is one candidate of the day as it was written, kept in the table `seedbed` of the
garden's store (`garden.store`), created by a new entry of `MIGRATIONS`:

| Column | Meaning |
|---|---|
| `seq` | Integer key from 1; the record's ref is `seed:<seq>`. |
| `candidate` | The C7 `ProposeRequest` as written, as JSON: text (`fact`), provenance (`source_kind`, `source_ref`, `observed_at`) and whatever else the writer gave (`entity`, `kind`, `attribute`, `valid_from`, ...). |
| `entity` | The id of the one entity the write resolved the candidate's name to (section 4), else NULL. |
| `conflicts_with` | The id of the current fact the record disputes (section 4), else NULL. |
| `sensitivity` | `public`, `private` (the candidate's `private`) or `secret` (it has `secret_ref`). |
| `written_at` | The garden's time of the write. |
| `processed_at`, `outcome` | Set once by the night stage (section 5): its time and the edge's answers as a C7 JSON array; NULL until then, both or neither. |

Rows are never deleted; the only change of a row is setting `processed_at` and `outcome` once.
A processed record stays as the provenance of what the night wrote from it (section 5) and keeps
`as_of` answers of the day reproducible, as every fact version does (ADR-0043).

Index: every non-secret record is a row of the existing `fact_fts`, written in the transaction
of the write, with rowid `-seq` (facts have positive rowids) and the same four columns as a
fact: its text, the names (the entity name as written and, when the write resolved one, that
entity's name and aliases), their Russian stems and their transliteration. The index row is
never removed; a query filters records by the transaction axis (section 3). No vector is ever
written for a record: `missing_vectors` and the worker list facts only. A secret-pointer record
gets no index row, as a secret fact gets none (ADR-0044 section 6).

Traces to scope item 1 and the decision of 11:04 (FTS5 only, no embeddings, no LLM).

Reasons: one SQLite file keeps the write, the disputed check against `facts` and the night's
processing in one transaction of one connection; one FTS5 index gives records and facts one bm25
scale, so the `PointerGate` threshold that gates facts gates records too.

Rejected:
- A separate FTS5 table for the seedbed: bm25's IDF is `log((N - n + 0.5) / (n + 0.5))` over the
  rows of the table, clamped to `1e-6` when not positive (section 9), so in a bed of a few
  records every term that occurs in half of them or more scores near zero, and a single record
  scores `1e-6` for any query. `pointer_min_lexical` could then never pass a record, and its
  scores could not be compared with the garden's.
- A separate SQLite file: two transactions for a write that reads `facts` and for a night step
  that writes `facts` and marks the record, and a second file the daemon opens and budgets.
- Writing records as facts with a flag in `facts`: a fact needs a resolved entity, a domain
  check and a placement on the valid-time axis, which is the night's work (11:04); facts would
  carry rows that every fact query must leave out.
- Deleting a record's index row when it is processed: an `as_of` of the day before the night
  would find neither the record nor the fact the night wrote later.

### 2. The way in: one write path through the seedbed

`propose` is the seedbed's write method. Every source of the day calls it: the owner's
"remember this" and corrections through `garden_propose` (`source_kind: owner`), what the agent
found and task results (`agent`), the second level's answers to a miss (`internet`, ADR-0047
section 5), rechecks of expired facts (the source's kind, ADR-0047 section 6), and archive hits
lifted by the engine (`raw`, section 6). `propose` now:

1. runs the edge's refusal screen, unchanged: an empty text or `source_ref`, an empty
   `entity` when one is given, a blank `attribute`, an inverted interval, an unknown domain, a
   `never_cache` kind, a `pin` from another source than the owner or on a secret pointer, a value
   that looks secret, a `secret_ref` that is no vault reference. A refused candidate is answered
   `rejected` with its reason and leaves only the proposal row with the reason, as today; no
   record is written;
2. otherwise writes one record (section 1) with the disputed check of section 4, and answers
   `seeded` with `proposal_id` the record's ref `seed:<seq>` and `conflicts_with` when the record
   is disputed;
3. a write whose candidate equals a stored record's candidate (`fact` compared after
   `edge::normalise`, every other field exactly) writes nothing and answers as the first write
   did, with that record's ref. A retried call and a repeated archive lift are idempotent; two
   different sources or texts saying the same thing stay two records for the night's dedup.

The rest of the edge (entity resolution, placement, duplicate, reconciliation by source rank,
doubt, the write into `facts`, the digest item, the pin, the top-k queue) runs only when the
night stage processes a record (section 5). The second level and rechecks write into the seedbed
too: a miss counts as answered when one of its candidates is stored as a record, so the same
question is answered from the seedbed the same day and no `garden_miss` is published; a recheck
is cleared when one of its candidates is stored as a record, and the expired fact gets its new
version when the night processes it. `resolve` keeps writing through the edge: it is the owner's
decision on a digest item the night stage made, not a second way in for the day's candidates.

After this, a candidate reaches `facts` only through the edge inside the night's processing of a
seedbed record, or through the owner's `resolve` of what that processing queued. This amends
ADR-0046 sections 2 and 4 and its consequences, and ADR-0047 sections 3a, 5, 6 and 7, in place.

Traces to scope item 2, the decision of 11:04 (all new facts of the day go into the bed, the
night reconciles with the garden and puts contradictions into the digest) and the decision of
11:01 to 11:04 (`garden_propose` is the agent's only handle, so the bed's write method is that
handle's C7 method).

Reasons: the owner's decision of 11:04 names reconciliation with the garden and contradictions
into the digest as night work, so the edge's decision moves to the night; the refusal screen
stays at write time because no secret value may be stored anywhere in the garden, the seedbed
included (`crates/garden/AGENTS.md`, ADR-0044 section 5).

Rejected:
- The edge at write time and a copy in the seedbed: two parallel write paths into the garden for
  the same candidate, which scope item 2 forbids, and a fact accepted before the night review
  that 11:08 says is disputed until then.
- The owner's candidates (or those with an `attribute`) straight into `facts`, the rest into the
  seedbed: two write paths again, and 11:04 names "remember this" among what goes into the bed.
- A new C7 method `seed` beside `propose`: two methods writing the same table, while 11:01 to
  11:04 gives the agent one handle and every day source fits `ProposeRequest`.
- The second level and rechecks straight through the edge: they are what the agent found during
  the day, and they would keep a second write path.
- `resolve` `replace` into the seedbed: the owner's decision would wait another night while its
  digest item is already closed.

### 3. Retrieval during the day

`search` and `pointers` take records from the same lexical query over `fact_fts`: a hit with a
negative rowid is the record `-rowid`. A record answers a request whose time (`as_of`, else now)
is at or after its `written_at` and before its `processed_at`; a private record only with
`private`; a secret-pointer record never (it has no index row). Records have no vectors, so they
enter RRF through the lexical ranking alone, then the reranker and the cut to `k` as any hit.
A `search` that finds only records is no miss.

In `search` a record is a C7 `Fact` with `fact_id` its ref `seed:<seq>`, `unprocessed: true`,
`fact` its text, `entity` the resolved entity's id, else the name as written, else the empty
string, `valid_from` the candidate's, else its `observed_at`, else `written_at`, `valid_to`,
`kind`, `domain`, `source_kind` and `source_ref` as written, and `disputed: true` with
`conflicts_with` when the record is disputed. `pinned`, `proof_count`, `expires_at` and
`recheck_before_acting` are absent: they belong to facts.

In `pointers` a record passes the same `PointerGate` through its lexical score and is a
`Pointer` with `fact_id` its ref, `unprocessed: true`, `disputed: true` when disputed, `entity`
the resolved entity's name, else the name as written, `kind` the candidate's, else the resolved
entity's, else `item` (the kind the edge gives a new entity), and the hint `there is an
unprocessed record about <entity> (<kind>)`, or `(<kind>, disputed)`; never its text. One
pointer per entity, a record's entity being its resolved entity, else its name as written after
`edge::normalise`. A record without an entity gets no pointer.

Traces to scope item 3 and the decision of 11:04 (the bed takes part in retrieval during the
day, unprocessed).

Reasons: one lexical query and one fused ranking keep the passive tier's single round trip and
its gate; the ref as `fact_id` is what a caller passes on (to `forget`, ADR-0051 section 5).

Rejected:
- Records in a separate list of `SearchResponse`: the model would have to merge two rankings,
  and a record would never outrank a weaker fact.
- A pointer for a record without an entity: the hint "there is a fact about X" has no X, and
  making `Pointer.entity` or `kind` optional would break every reader of C7 0.4.0.

### 4. The disputed flag

At the write, with the record's transaction, and only for a candidate with `entity` and
`attribute` and without `secret_ref`:

1. Entity: the edge's entity step (ADR-0046 section 2) read only: by id (a merged one stands for
   its survivor), else by name among the entities of its kind on the first of three levels that
   finds any (equal normalised name or alias, equal `translit::key`, one edit apart for keys of
   five characters or more). Exactly one entity found is the record's `entity`; none or two and
   more leave it NULL and the record undisputed. The check creates no entity and queues no
   `ambiguous_entity` item.
2. Attribute: the current versions of that entity with an equal `attribute` (string equality,
   as the store closes versions), believed and valid at the write time, any sensitivity.
3. Value: the record is disputed when it holds at the write time (its `valid_from`, absent = the
   write time, at or before it, and its `valid_to` absent or after it) and the normalised text
   (`edge::normalise`) of such a version differs from its own; `conflicts_with` names the first
   of them by `recorded_at`. An equal text is a restatement, not a dispute.

The flag is stored on the record and shown in `search` and `pointers` (section 3) and in the
answer of `propose`. The write creates no digest item, no `edge_proposal` row, no C1 event and no
other message: the owner hears of a contradiction only when the night's edge queues it in the
digest. The source rank does not lift the flag: an owner's correction is disputed until the night
too.

Traces to scope item 4 and the decision of 11:08 (disputed by default until the night review,
no message to the owner, the agent uses or ignores the flag).

Reasons: the comparison is the garden's own entity resolution and the store's own notion of a
single-valued attribute, so a dispute flagged in the day is the conflict the night's edge would
see; it is a few indexed reads with no model.

Rejected:
- Similarity to current facts for candidates without `attribute` (the top-k reconciliation of
  ADR-0046): it needs the record's vector, and the bed has none (scope item 1).
- The edge's rank rule at write time (a higher rank beats the current fact, no flag): 11:08 marks
  every fresh conflicting fact disputed by default.
- A flag on the current garden fact as well: the issue flags the fresh record; the garden's facts
  change only at night.

### 5. The night stage's batch

Two C7 methods for the night stage, served on the garden socket like the others:

| Method | Input | Output |
|---|---|---|
| `seedbed_take` | `limit` (1 to 100, default 50), optional `until` (Unix time, default now) | `records`: the unprocessed records written at or before `until`, oldest first, each `SeedRecord` with `ref`, `candidate` (`ProposeRequest`), `written_at` and `conflicts_with` when disputed |
| `seedbed_done` | `items`, 1 to 100, each `ref` and optional `candidates` (`ProposeRequest` array) | `items`, each `ref` and `outcomes`, the `ProposeResponse` of every candidate the edge decided |

`seedbed_take` only reads: it leases nothing, so a night stage that stops halfway takes the same
records again. `seedbed_done` processes each item in its own unit of work of the store
(`Store::atomically`): without `candidates` the edge decides the record's own candidate; with
`candidates` it decides those instead (the night's dedup drops a record with an empty array, its
placement gives an entity-less record its facts); then `processed_at` and `outcome` are set. The
edge decides as ADR-0046 section 2 says, with two inputs from the record: an absent `valid_from`
defaults to the record's `written_at` (the day the fact was said, not the night), while what it
writes is recorded at the processing time; each `edge_proposal` row it writes names the record in
a new column `seed`. A record already processed answers its stored `outcome` and writes nothing,
so a retried call is idempotent. An unknown ref fails the call with `404 not_found` before any
item is processed. After `seedbed_done` the daemon renders the core profile and wakes the worker,
as it did after `propose` (ADR-0047 sections 3a and 6).

Nothing in the garden calls these methods on a timer: the night stage is their caller.

Traces to scope item 5 and the decision of 11:04 (the night takes the bed). The night gardener
that calls them is issue:33b6bfbfefaa49d21a4a.

Reasons: "take a batch, mark it processed" with no state between the calls is the least
interface that lets the night run the edge unchanged, drop duplicates and supply candidates for
records it places, and survive its own crash.

Rejected:
- A lease with an expiry on taken records: state and a deadline in the garden for one caller.
- `seedbed_done` as a bare mark with the night writing facts through another method: a second
  way into `facts` beside the processing, which scope item 2 forbids.
- The worker running the edge over the bed at a fixed hour: that is the night gardener, outside
  this issue.

### 6. Fit with ADR-0051

- Section 5 (forget). The ref form `forget` takes for a seedbed record is `seed:<seq>`, the
  `fact_id` of the record in `search` hits, so the `query` shape of `forget` answers it as a
  candidate. The record has a stable key (`seq`) and is never deleted, so card 11 of ADR-0051
  adds its marks as rows of a table `seed_marks` (`id`, `seed` referencing `seedbed(seq)`, `at`,
  `reason`, `provenance`, `lifted_at`, `lifted_provenance`) beside the record, and the record
  needs no column for them. With a live mark the record leaves `search` and `pointers`,
  `seedbed_take` leaves it out and `seedbed_done` answers it `404 not_found`. A fact the night
  wrote from a record carries the record's own `source_ref` (the evidence, `c8:<n>` or
  `archive:<id>`), so a `c8:<n>` ref reaches it by ADR-0051's rule as written; a `seed:<seq>` ref
  reaches the current facts written by proposals whose `seed` column names the record
  (section 5). ADR-0051 section 5 is amended in place to say so.
- Section 6 (lifting). The seedbed's write method is `propose`. A lifted archive hit is
  `{fact: <text bounded to 1,200 characters>, source_ref: "archive:<id>", source_kind: "raw",
  observed_at: <the item's at>}` without `entity`: ADR-0051's source `archive` is the kind `raw`,
  since the archive holds raw material (ADR-0051 section 2) and ranks as raw at the night's
  reconciliation, and the ref names the archive. `ProposeRequest.entity` becomes optional
  (section 7) because a hit names no entity. A repeated lift of the same hit is idempotent
  (section 2), and the record's own ref `seed:<seq>`, never `archive:<id>`, is what `forget`
  marks, so the archive stays untouched (12:39 in ADR-0051). ADR-0051 section 6 is amended in
  place with these fields.

### 7. C7 0.5.0, C2 unchanged

C7 0.5.0, additive by the rules of `contracts/CHANGELOG.md`:
- `ProposeRequest.entity` is no longer required (the edge still refuses a candidate without it
  when it decides one at night: `invalid`);
- `ProposalStatus` gains `seeded`; `ProposeResponse.proposal_id` is the record's ref for it and
  `conflicts_with` names the disputed fact;
- `Fact` gains optional `unprocessed`, `disputed` and `conflicts_with`; `Pointer` gains optional
  `unprocessed` and `disputed`;
- `x-methods` gains `seedbed_take` (`SeedbedTakeRequest`, `SeedbedTakeResponse`, `SeedRecord`)
  and `seedbed_done` (`SeedbedDoneRequest`, `SeedbedDoneItem`, `SeedbedDoneResponse`,
  `SeedbedOutcome`); `ErrorCode` `not_found` also covers an unknown seedbed ref;
- the contract's description says `propose` writes the seedbed and the night's processing hands
  records to the edge.

C2 is unchanged: `garden_propose` keeps its input (the agent names an entity) and its `Ack`;
the flag reaches the model through the engine's `garden_search`, which returns C7 `Fact` fields
(ADR-0047 section 3c). C2's `GardenFact` carries none of C7's later fields; aligning it is the
core facade's work.

### 8. Budgets

No new limit; the rows of the `ulei-garden` section of `ci/budgets.sh` (ADR-0047 section 7)
measure the seedbed, driven by `ci/garden_run.py`:

| What | Measured how | Existing limit |
|---|---|---|
| A seedbed write | wchar per stored `propose` | `ULEI_GARDEN_MAX_WRITE_BYTES` 131,072 B |
| A processed record | wchar per record of `seedbed_done` | `ULEI_GARDEN_MAX_WRITE_BYTES` 131,072 B |
| `search` over facts and records | p50, connect to last byte, on 2,000 facts and the unprocessed records the request loop writes | `ULEI_GARDEN_MAX_SEARCH_P50_US` 20,000 us |
| `pointers` over facts and records | as above | `ULEI_GARDEN_MAX_POINTERS_P50_US` 10,000 us |
| Peak RSS | VmHWM after 10^4 requests (`pointers`, `search`, `propose` into the seedbed) | `ULEI_GARDEN_MAX_RSS_KB` 4,096 kB |
| Size, ready, idle | the daemon with the seedbed code; ready on the filled store; the seedbed adds no deadline, so idle stays 0 | 2,621,440 B, 10,000 us, 0 |

The driver fills the 2,000 facts through `propose`, `seedbed_take` and `seedbed_done` (the night
path), and its request loop's `propose` calls leave their records unprocessed, so `search` and
`pointers` run over both. The latency of `propose` is printed as before and has no limit.

### 9. External facts checked

Checked on 2026-11-22.

| Fact | Source |
|---|---|
| Bundled SQLite is 3.54.1 (`libsqlite3-sys` 0.40.0 in `Cargo.lock`) with FTS5 | `Cargo.lock`, `sqlite3.c` of the crate (`SQLITE_VERSION`), ADR-0051 section 9 |
| FTS5 bm25: IDF of a phrase over `N` rows of the table with `n` rows containing it; in `fts5_aux.c` `idf = log((nRow - nHit + 0.5) / (nHit + 0.5))`, set to `1e-6` when not positive | sqlite.org/fts5.html section 5.1.1; `sqlite3.c` of SQLite 3.54.1 in `libsqlite3-sys` 0.40.0 |
| A one-row FTS5 table scores a matching query `-bm25 = 1e-6` | measured with SQLite 3.45.1, throwaway, not committed |
| An FTS5 table has an implicit `INTEGER PRIMARY KEY` rowid, a 64-bit signed integer; a contentless table needs a non-NULL rowid on insert; contentless-delete tables accept DELETE | sqlite.org/fts5.html sections 1, 4.4.1 and 4.4.2; sqlite.org/lang_createtable.html section 5 |
| Negative rowids in a contentless-delete table with `unicode61 remove_diacritics 2` and `prefix='2 3 4'` are inserted, matched and ranked with positive ones | measured with SQLite 3.45.1, throwaway, not committed |

### 10. Implementation cards, in order

Each card merges alone with the workspace green, tests on synthetic data only, and rewrites in
place every document that describes what it changes (`crates/garden/AGENTS.md`, `README.md`,
the C7 description). "C7 0.5.0" means the next free minor version when card 1 lands.

1. C7 0.5.0 (about 60 min). Goal: the contract of section 7. Touches `contracts/` (schema,
   `index.json`, `CHANGELOG.md`, fixtures), the generated types, and `crates/garden` only where
   the optional `entity` must compile (the edge refuses a candidate without it as `invalid`).
   Check: valid and invalid fixtures of every addition, the generated round trip, `cargo xtask
   contracts`, a propose without `entity` answered `rejected` `invalid` as before card 3.
2. The seedbed store (about 60 min). Goal: section 1 in `crates/garden` (`store`): the migration
   with `seedbed` and `edge_proposal.seed`, `Store` methods to write a record with its index row
   at `-seq`, to read records, to set `processed_at` and `outcome` once, the lexical query
   returning records filtered by the transaction axis and sensitivity, and the refill of
   `fact_fts` including records. Check: a written record is found lexically at once; a secret
   pointer record has no index row; no vector is listed or written for a record; a record is not
   found at a time before its writing or after its processing; a second `processed_at` is refused.
3. `propose` writes the seedbed (about 60 min). Goal: sections 2 and 4 in `crates/garden`
   (`edge`, `api`): the screen at write, the record, the disputed check, `seeded`, the idempotent
   repeat. Check: a "remember this" is `seeded` and writes no fact and no digest item; a
   candidate on a current fact's entity and attribute with another value is disputed with
   `conflicts_with`, an equal value and an ambiguous name are not; a secret value is `rejected`
   and stores no record; a repeated write answers the same ref and adds no row.
4. Records in `search` and `pointers` (about 60 min). Goal: section 3 in `crates/garden` (`api`,
   `search`). Check, through the daemon's socket: a proposed fact is found by `search` and by
   `pointers` right after `propose`, marked `unprocessed`; a disputed one carries `disputed` and
   creates no inbox packet at a fake C1 socket and no digest item; a record without an entity
   gets no pointer; a private record only with `private`.
5. The night batch (about 60 min). Goal: section 5 in `crates/garden` (`api`, `serve`, `edge`):
   `seedbed_take`, `seedbed_done`, the default start at `written_at`, `edge_proposal.seed`, the
   render and the wake after it. Check: a batch is taken oldest first and marked processed; a
   processed record leaves `search` and its fact is found; a second `seedbed_done` of the same
   ref writes nothing; an empty `candidates` array writes no fact; a record disputed in the day
   becomes a digest `contradiction` at processing.
6. The second level and rechecks into the seedbed (about 60 min). Goal: the worker part of
   section 2 in `crates/garden` (`worker`, `miss`), with ADR-0047's sections 5 and 6 as amended.
   Check: a miss answered by the fake source leaves a record, the same question is answered from
   it and nothing is published; a source that finds nothing still leads to a publish; a recheck
   with a stored candidate is cleared and its new version appears only after `seedbed_done`.
7. Seedbed budgets (about 45 min). Goal: section 8 in `ci/garden_run.py`: the fill through the
   night path, the request loop's records left unprocessed, wchar per seedbed write and per
   processed record. Touches `ci/garden_run.py`, `ci/budgets.sh` comments. Check: the `budgets`
   job.

Order: the contract (1) before code that speaks it; the store (2) before the write (3); the write
before retrieval (4) and the batch (5); the batch (5) before moving the worker's writes (6),
since a recheck's new version needs the night path; the budgets (7) last, on the final paths.
ADR-0051 card 11 (forget in the garden) comes after cards 2 and 5, its card 12 (lifting) after
card 3.

## Consequences

- One write path: every candidate of the day is a seedbed record; `facts` grow only when the
  night stage processes records, and the digest receives contradictions and doubts only then.
- Until the night gardener (issue:33b6bfbfefaa49d21a4a) calls `seedbed_take` and `seedbed_done`,
  nothing new enters `facts` and the digest stays empty; the day's records are found by
  `search` and `pointers` as unprocessed for as long as they wait.
- A pin, an expiry and a recheck's new version take effect at the night's processing: the core
  profile shows a fact pinned today after the night.
- A record and the fact the night writes from it are both kept; the record leaves retrieval at
  its processing, the fact enters it, and an `as_of` of the day still answers with the record.
- The disputed flag covers single-valued attributes only; a contradiction without an attribute
  is found at night by the top-k reconciliation of ADR-0046, once the fact has a vector.
