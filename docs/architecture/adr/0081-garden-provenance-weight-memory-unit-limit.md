# ADR-0081: Garden provenance weight; the memory unit's limit

Status: accepted
Date: 2026-11-26
Affects: crates/garden, crates/memory, deploy/systemd

## Context

Two failures came out of the first full night run on a deployment (ADR-0078).

**The garden's ranking.** The night walked a whole chat history and proposed about 11,000 facts
with `source_ref = "archive:<item id>"` and `source_kind = raw` (`history_proposal` of the night
gardener). The garden already held about 10,000 facts from the owner's curated documents
(`source_ref = "<path>#<section>"`, also `raw`). The search is RRF with `rrf_k = 0` (ADR-0045
section 2), so a hit's fused score is a sum of `1/rank`, and nothing in it tells the two
provenances apart. A history fact is often a chat restatement of a document fact ("Shoe size 45
EU.") or a stale event of a past conversation ("the meeting is today at 18:00"), and the chat
says it in fewer, denser words, so it wins the lexical rank. On the deployment's gold set,
lexical only, recall@3 fell from 0.393 to 0.304 and MRR from 0.298 to 0.176; in 11 of 56
questions a history fact took the first rank ahead of the document fact.

An offline re-ranking of the live top 30 with a factor `w` on history hits:

| w | recall@3 | MRR |
|---|---|---|
| 1 | 0.304 | 0.176 |
| 0.5 | 0.339 | 0.193 |
| 0.25 | 0.375 | 0.280 |
| 0.1 | 0.446 | 0.324 |

With `rrf_k = 0`, 0.5 only ties a history hit with the next rank. At 0.1 the dev split went
from 0.368 to 0.579 and the unlabeled split from 0.286 to 0.429; the holdout did not change.
Collapsing near-duplicates by token overlap added nothing on top of the weight.

**The archive's unit.** `ulei-memory.service` had `MemoryMax=32M` (ADR-0070 section 2). On an
archive of about 4.8 GB (278,000 items, 5,000 forest records) the cgroup sat at its limit:
`memory.events` `max` near 40,000, no OOM kill. The cgroup charges page cache, so the archive's
index pages were reclaimed all the time. `health` counts `blobs` (over its covering autoindex)
and the current forest records; cold, the forest count alone took 9.5 s and the whole `health`
about 30 s, warm both under 0.05 s. Clients with a 30 s socket timeout failed on `health`.

## Decision

### 1. History evidence weighs less in the garden's ranking

A hit whose evidence is a history archive item, a fact or a seedbed record whose `source_ref`
starts with `archive:`, has its fused RRF score multiplied by `SearchParams::history_weight`
after fusion and before the reranker and the cut to `k`. Every other hit keeps weight 1.
Passive pointers come from the same weighted ranking. The `score` of a C7 hit is the weighted
score; C7 does not change.

- `history_weight` lies in [0, 1]; `SearchParams::validate` refuses NaN and values outside.
  The default is 0.1: with `rrf_k = 0` the weight divides the rank, so a history hit needs a
  ten times better rank than a document hit to pass it, and a history hit still ranks first
  when no document hit matches. At 1 the order is the one before this ADR.
- The provenance is looked up for the fused candidates only (at most twice `candidates`), in
  one store query: `source_ref` of the facts by their unique id, the candidate of the records
  by `seq`. The garden's SQLite has no JSON functions, so a record matches on the text
  `"source_ref":"archive:` of its stored candidate, which a quote escaped inside another field
  never forms.
- It is a class rule on provenance, not a per-query rule. The garden reads no search knob from
  C9 today (`rrf_k`, the weights and `candidates` are constants of `SearchParams::at`), so no
  `garden.history_weight` key is added; a deployment that needs another value is a C9 change
  of its own.
- `search::similar`, the reconciliation of new facts on the worker, compares facts of one
  entity and keeps the unweighted fusion.

### 2. The archive's unit keeps its index pages warm

`MemoryHigh=128M`, `MemoryMax=256M`. The daemon's own memory keeps its 4 MiB VmHWM budget
(ADR-0051 section 7, held by `ci/budgets.sh`); the rest is page cache, which the cgroup charges.
On the measured archive the hot pages are the autoindex of `blobs`, the forest's tables and the
FTS5 indexes `search` reads, some tens of MiB. Above `MemoryHigh` the kernel throttles the unit
and reclaims its cache first, so 128M is the working set it keeps; `MemoryMax` at twice that is
the hard stop for a burst of `put` and `import` parts, far from the host's memory.

`forest_records`, the forest count of `health`, is all records less the superseded ones: records
and their supersession are never removed and a record is superseded once, so two plain counts
over the smallest index of each table replace a lookup in `forest_superseded` per record. The
count of `blobs` stays as it is: it already reads the covering autoindex, which the new limit
keeps warm.

## Consequences

- Garden: on a store without `archive:` evidence nothing changes; the synthetic gold set of
  `garden-eval` has none, so its numbers stay. A curated fact first, a history restatement next
  in the top `k`.
- Memory: a slow `health` with a growing `max` in the unit's `memory.events` means the limit is
  too small for the archive; a drop-in raises `MemoryHigh` and `MemoryMax`
  (`docs/runbooks/memory-archive.md`).
- ADR-0045 (fusion), ADR-0070 section 2 (the unit's limits), ADR-0076 and ADR-0079 (which name
  the old 32M) are amended by this record.

## Alternatives

- **A per-query rule or a dedup of near-duplicates.** Measured, it added nothing on top of the
  weight; a class rule on provenance explains the order to the owner in one sentence.
- **A weight of 0.5 or 0.25.** 0.5 changed nothing with `rrf_k = 0`; 0.25 recovered about half
  of what 0.1 did.
- **Another `source_kind` for history facts.** It would change C7 and the night's proposals and
  leave the stored 11,000 facts as they are; the `archive:` prefix already names the evidence.
- **Keep 32M and cache the counts in memory.** It fixes `health` only; every `search` and
  `forest_search` on a multi-GB archive still reads its indexes from disk.
