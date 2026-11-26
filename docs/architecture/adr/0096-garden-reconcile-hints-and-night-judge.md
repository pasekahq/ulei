# ADR-0096: Related-fact hints from the garden, judged by the model at night, applied silently and reversibly

Status: accepted
Date: 2026-11-26
Affects: crates/garden, crates/engine, contract:C7, contract:C9, docs/runbooks/night-gardener.md, docs/ulei.example.toml, ci/night_run.py, ci/judge_eval.py, .github/workflows/encoder.yml, .github/workflows/ci.yml
Amends: ADR-0046 section 2 ("top-k reconciliation"), ADR-0047 section 4 (`garden.edge.reconcile_min_cosine`), section 6 (the worker), card 13, section 9 and its consequences; ADR-0078 sections 2, 5 and 6 (a stage d with its own budget); ADR-0087 section 8 (what a failed content check undoes)

## Context

The worker turns a new fact without an attribute into a `contradiction` digest item when one of
the five facts of its entity most similar to it has a cosine of at least
`garden.edge.reconcile_min_cosine` (0.85) and another normalised text (ADR-0046 section 2,
`Edge::reconcile_similar`). Nobody calibrated 0.85. With multilingual-e5-small it flags 73 to 98
per cent of new facts (E12a: 2,036 of 2,991). On a labelled synthetic set of 503 cases (report
E15) a contradiction sits at the cosine of the best unrelated neighbour under both e5-small and
bge-m3, a paraphrased duplicate above both, and the scale differs by model (bge-m3 puts
unrelated pairs near 0.48, e5 near 0.84): no absolute cosine separates a contradiction from a
compatible fact. Nothing in the product reads the digest (only `curl` in the runbook), so the
items reach nobody. ADR-0047 section 9 left "deciding them by meaning needs a model through the
gateway" unbuilt; the night gardener (ADR-0078) is now a model through the gateway.

E15 measured how well embeddings *select* the earlier fact a new one is related to, without a
threshold: the 5 nearest current facts of the same entity by cosine of the vectors the garden
already stores (`"<entity name>: <fact>"`) contain it in 0.886 of 299 related cases with e5-small
(95 % CI 0.845 to 0.917) and 0.973 with bge-m3; fusing the lexical ranking in, as `similar` does,
costs 2 to 5 points; the bare text without the name prefix costs 7 to 9; the lexical ranking
alone finds 0.629, and 0.06 across languages.

The owner decided (2026-11-26) that embeddings no longer decide that two facts are in conflict.
They only pick the earlier facts a model then judges. The owner does not care about retrieval
internals: agents handle all of it and nothing is pushed to the owner. Developers learn whether
it works from evals and from anonymous aggregate counters. People keep the ability to inspect
and correct on their own initiative, and an agent may offer it, but no routine message such as
"merged ten facts" is ever sent.

## Decision

### 1. Hints, not verdicts, from the garden

- The queue stays what ADR-0046 section 2 feeds: facts the edge accepts without `attribute` and
  without `secret_ref`, in `reconcile_queue` order. A row is `waiting`, then `hinted`; it leaves
  the queue when it is judged, or when its fact is no longer current.
- A waiting fact gets its **hints** once (`Edge::hint`, `edge::HINTS` = 5, a named
  constant): the current facts of its entity that hold at the fact's valid time, other than the
  fact itself, other than facts queued after it (they are weighed when they are judged
  themselves), other than a pair kept by an undo (section 5). With a vector of the helper's model
  for the fact they are ranked **by cosine of the stored vectors, best first, ties by id**, with
  **no threshold and no fusion with the lexical ranking** (`basis` `dense`). Without one they are
  the entity-filtered lexical ranking (`basis` `lexical`): at once when the daemon runs without
  an embedder, and after `edge::HINT_WAIT` (3,600 s after the fact was recorded) when the helper
  is down or the passage was skipped, so a broken helper delays the judge by an hour and no
  more. No per-model number remains. The worker computes the dense hints, after the fact's
  passage; the lexical ones are computed by `reconcile_take` when the night asks for its page
  (`judge::hint_lexically`, at most 500 per call), so a garden without an embedder does no
  background work for them and keeps its idle, RSS and latency budgets (a first version that
  hinted in the worker raised the daemon's VmHWM in `ci/garden_run.py` from about 3.7 to 4.4 MB,
  over ADR-0047's 4 MiB, and starved writes of the request path).
- The hints go into `reconcile_hint (fact, rank, hint, basis, model, computed_at)` and the queue
  row becomes `hinted`. A fact whose entity has no other candidate leaves the queue without
  hints, and no model sees it.
- The worker never writes a digest item. `EdgeConfig::reconcile_min_cosine` is gone (section 7).

### 2. C7 0.14.0 (additive): the judge's take and done

- `reconcile_take {after?, limit 1..100}` first gives lexical hints to the waiting facts due
  for them (section 1), then returns the hinted facts in queue order:
  `{seq, entity, entity_name, fact: Fact, hints: [Fact], basis, embedder?}`, each `Fact` as
  `search` returns it with its `fact_id`. A secret pointer is never queued, never a hint.
- `reconcile_done {judge, items: [{fact_id, verdicts: [{hint, relation, note?}]}], stats?}` with
  `relation` one of `duplicate`, `supersedes`, `contradiction`; a hint without a verdict is
  compatible. `judge` names the judge's model (the gateway's model id, else the route) and the
  prompt version, and is kept on every record. Every `fact_id` must wait `hinted` or have been
  judged, else `not_found` before any item; a hint that is not one of the item's is
  `invalid_request` before any item. Each item is its own unit of work, as in `seedbed_done`.
  A fact judged before answers its stored outcomes and changes nothing.
  - **contradiction**: an `edge_proposal` row `queued` / `contradiction` with `fact_id` the new
    fact, `conflicts_with` the hint and the judge's reason as `note`. It is **recorded, not
    pushed** (section 4).
  - **supersedes**: the rank rule of ADR-0046 section 2 decides. When the new fact may replace
    the hint (a higher source rank, or an equal one and a start not before the hint's), the hint
    is retracted in favour of the new fact and stays believed outside its valid interval, as
    `resolve accept` does; otherwise the verdict is applied as a contradiction.
  - **duplicate**: the new fact is retracted in favour of the hint, **the earlier wording is
    kept**, and the hint's `proof_count` rises by one through a `duplicate` proposal row with
    the new fact's `source_ref`, as for an intake duplicate.
  - Precedence inside one item: every contradiction is recorded; only without one are the
    supersedes applied; only without either is the first duplicate (by hint rank) applied. A
    verdict that does not apply is recorded `not_applied`, one whose hint or new fact is no
    longer current `stale`. A new fact is never both merged and disputed.
- `stats` (optional): the judge calls, failed calls (malformed twice, refused, too long),
  timeouts and tokens since the night's previous `reconcile_done`; `items` may be empty to send
  only them.

### 3. Night stage d, "judge", with its own budget

- `ulei-engine night` runs b, a, c, **d** (`--stage d` alone too). Stage d takes
  `reconcile_take` pages, groups the items by entity in first-seen order and builds calls of
  whole entities up to `night.judge_batch` new facts (default 25, 1 to 100), cut earlier at
  `night.max_prompt_tokens`; an entity with more is cut into parts of `judge_batch`. A call is
  one C4G turn on `night.route` with no tools: the system prompt `night/judge.txt` (version
  `judge-v1`) and JSON lines, per entity one `entity` line, the union of its items' hints once
  as `earlier` lines and its items as `new` lines with the refs of their own hints. The strict
  answer names every new fact once with verdicts only for hints that are not compatible
  (unknown keys, refs, hints of another item or relations are malformed). A malformed answer is
  asked once more and then left; an answer that hits `max_tokens` is split in halves down to one
  fact, as stage a does.
- **Budget**: the judge spends from `night.judge_max_usd` (default 0.5), a line of its own
  beside `night.max_usd`, under the same prices and the same rule of ADR-0078 section 6. Stage d
  runs even when stage a or c stopped at their spend limit, so a history import cannot starve
  it; when its own budget is exhausted the remaining facts stay `hinted` in the garden for the
  next night. Nothing is dropped.
- The run report gains one `judge` line per call (counts only, no fact text) and the totals
  `judged`, `merged`, `superseded`, `contradictions`, `stale`, `judge_failures`,
  `judge_timeouts`, `judge_usd`; the summary line names `judged`. A gateway timeout ends stage
  d, is counted, and the run ends `failed` as for the other stages.
- No hint reaches the agent that proposes a fact in the day; that is a follow-up (section 8).

### 4. Silent, recorded, reversible

- No verdict creates a message, a C1 event or a pushed digest entry. `digest` stays the
  on-demand surface nobody is pushed from; judged contradictions are listed there with the
  judge's `note` (`DigestItem.note`) and `resolve` settles them as before.
- Agents see a conflict where they read facts: `search` marks a written fact in an open judged
  contradiction `disputed` with `conflicts_with` the other fact and `conflict_note` the judge's
  reason, so an agent that uses one of the two can ask the owner in context when it matters.
- Every applied verdict (merged, superseded, contradiction) is an audit row of
  `reconcile_judgement`: both fact ids, the entity, the verdict, what was applied, the judge's
  one-line reason, the time, the judge's model id and the embedder's model id with the hint's
  basis. Stale and not-applied verdicts are rows too, for idempotency, and are not listed.
- `reconcile_review {limit, before?, entity?, relation?, open?}` (C7, read only) lists the recent
  applied verdicts newest first with both texts and their state (`applied`, `open`, `resolved`,
  `undone`), paged by `before`, filtered by entity, relation and open contradictions only. The
  engine exposes it to agents as the tool `garden_review`, registered with `garden_search`.
- `reconcile_undo {change_id}` (C7) reverses one: a merge writes the retracted new fact back as
  a current copy and withdraws its proof row; a supersede writes the retracted earlier fact back
  as a current copy and closes the slices the retraction kept; a contradiction withdraws its
  open digest item. The pair is then **kept** (`reconcile_kept`): it is never offered to the
  judge again, so it is not re-judged into the same change. Facts stay append-only: the copy is
  a new version, `as_of` before the undo answers as before it. An undo of a change already
  undone, or of a contradiction the owner resolved, or of a merge or supersede whose retracted
  fact was closed again since by something else, is `invalid_request` and changes nothing.

### 5. Anonymous aggregate counters, local only

- The garden keeps counters in `reconcile_stats` (no text, no entity, no id): facts hinted
  (dense, lexical), hints offered, facts judged and of them on lexical hints, verdicts by
  relation and compatible hints, applied by kind, stale and not applied, undos by kind, the
  judge's calls, failures, timeouts and tokens in and out from `stats`, and the time from a
  fact's recording to its judgement in buckets (1 h, 6 h, 1 d, 3 d, 7 d, more). `health` shows
  them as `reconcile` with the queue's `waiting` and `hinted` counts.
- Nothing leaves the machine. Exporting them (opt-in telemetry) is a separate future decision
  with its own ADR.

### 6. Re-judging

- A verdict is about two texts, not vectors. A change of embedding model does not re-judge; it
  only changes how the hints of facts still waiting are computed.
- A failed content check of the weight file (ADR-0087 section 8) drops the vectors computed
  since the last passed check and puts the facts whose dense hints were computed since back to
  `waiting`; judged facts stay judged, since the judge read texts. This replaces the withdrawal
  and re-queue of cosine-made items that section 8 described.
- A new judge (route or prompt version) re-judges nothing by itself.

### 7. C9 0.13.0 and the migration

- C9 0.13.0 (additive): `night.judge_max_usd` (a decimal, default `"0.5"`) and
  `night.judge_batch` (1 to 100, default 25). The description of
  `garden.edge.reconcile_min_cosine` says it is no longer read: the daemon accepts it for one
  release with one stderr line `garden: garden.edge.reconcile_min_cosine is no longer read
  (ADR-0096); remove it`. The release after removes it from C9 (a breaking minor bump while
  0.x).
- Store migration 17: `reconcile_queue.state`, `reconcile_hint`, `reconcile_judgement`,
  `reconcile_judged`, `reconcile_kept`, `reconcile_stats`. Every open contradiction item that the
  cosine rule wrote (`edge_proposal` `queued`, reason `contradiction`, `fact_id` not null) is
  closed as `withdrawn` (`digest` never lists it, `resolve` answers it `not_found`). Its facts
  are left as they are and are **not** re-judged. Attribute contradictions (`fact_id` null) and
  decided items are not touched. Facts still in `reconcile_queue` stay `waiting` and get hints.

### 8. Not in this decision, follow-ups

- An advisory hint to the agent that proposes a fact in the day (`ProposeResponse.related`, the
  candidate embedded as a query, E15 measured 0.893 at k = 5 with e5-small).
- An undo tool for agents (`reconcile_undo` is in C7; the engine exposes only the read).
- The removal of `garden.edge.reconcile_min_cosine` from C9 one release after this one.
- Opt-in export of the counters of section 5.
- A full re-judge with a new judge (`--rejudge-since`).

## Evaluation

- **Selection eval** (`crates/garden/tests/reconcile_eval.rs` on
  `crates/garden/tests/fixtures/reconcile-eval/`, the frozen base set of E15: 60 synthetic
  people, 1,785 earlier facts, 419 new ones, 299 of them related; generated by
  `ci/reconcile_eval/gen.py`, which `--check`s the frozen files): recall of the related earlier
  fact within the 5 hints, asserted not to fall below its measured value for the deterministic
  fake embedder (0.599) and for the lexical ranking (0.629, as E15's re-implementation found)
  in every test run, and, in the `encoder.yml` job on the real e5-small file, within 0.02 of
  E15's 0.886 (the garden's own selection on that file measured 0.886 when this was written).
- **Judge eval** (`ci/reconcile_eval/judge-eval.jsonl`, the 419 base cases with E15's e5-small
  hints and gold relations; `ci/judge_eval.py`): contradiction precision, silent-damage rate
  (a `duplicate` or `supersedes` on a compatible pair), recall per relation on cases whose gold
  fact is among the hints, and the supersedes/contradiction confusion. It calls a model, so it
  runs only by hand with a qb gateway socket or an OpenAI-compatible endpoint and key from the
  environment, never in CI.

## Consequences

- The digest gets contradictions only from a model's judgement, with its reason, and pushes
  nothing. Paraphrased duplicates, the common case after a history import (ADR-0081), are merged
  silently and raise `proof_count`; supersedes follow the attribute rank rule. Both are audited
  and reversible.
- The garden stays free of model clients; the engine's night gains one stage, one prompt and one
  budget line. Cost about 180 to 270 input tokens per new fact at k = 5 (E15 estimate).
- Recall is bounded by the hints: with k = 5 about 11 % (e5-small) and 3 % (bge-m3) of related
  earlier facts are never shown to the judge on the E15 set, more on entities with hundreds of
  facts; lexical hints find about 63 %, and almost nothing across languages.
- The judge's accuracy is not measured yet: it needs a run of the judge eval against a model.
- Conflicts are found up to one night late, since hints need the passage vector.
