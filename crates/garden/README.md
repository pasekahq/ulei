# ulei-garden

The garden: typed facts about the owner's life with two time axes and provenance, the edge in
front of it that decides what enters, and retrieval over it in three tiers. It speaks C7 to
its callers and C1 (`garden_miss`) to the owner inbox. Decisions: ADR-0043 to ADR-0050 in
[`docs/architecture/adr/`](../../docs/architecture/adr/); rules for coding agents:
[`AGENTS.md`](AGENTS.md).

The model does not see the garden until issue:76b6f0e180339e492edd: the engine step (the core
profile in the prompt, pointers on every owner turn, the tools `garden_search` and
`garden_propose`), the unit, release and install of `ulei-garden` and `ulei-garden-embed`, and
the second-level providers over the wire (ADR-0047 section 9). Until then the daemon serves any
local C7 client on its socket.

## What it holds

- Entities of six kinds (person, place, event, item, preference, visit) in domains bound to
  Wikidata classes; the 20 starting domains are `src/starting_domains.json` (ADR-0043).
- Facts on two time axes: valid time (`valid_from`, `valid_to`) and transaction time
  (`recorded_at`, `superseded_at`). A change closes the old version and writes a new one, so
  every past answer is reproducible with `as_of` (ADR-0044).
- Provenance on every fact (source kind owner, raw, internet or agent, `source_ref`,
  confidence) and a TTL kind whose row gives `expires_at`. An expired fact waits for the second
  level to find a new version (ADR-0044, ADR-0047 section 6).
- Secrets only by pointer: a secret fact says that the secret exists and names its vault
  entry. No value is stored, logged, indexed or returned (ADR-0043, ADR-0044).
- The seedbed: `propose` writes a candidate as a record of the day once the edge's refusal
  screen passes it, and marks it disputed when it gives another value of an attribute a
  current fact of its entity holds; it writes no fact and tells the owner nothing (ADR-0055
  sections 2 and 4).
- The edge: when the night's processing hands it a record, it accepts the candidate on its own
  unless it contradicts a current fact, has low confidence or names an ambiguous entity; those
  wait in the digest until the owner settles them with `resolve`. A new fact without an
  attribute is reconciled later against the most similar facts of its entity (ADR-0046
  section 2).

## Retrieval tiers

| Tier | Method | Decided in |
|---|---|---|
| Pinned core profile, rendered to `garden.core_file` within `garden.core_char_limit` | written by the daemon | ADR-0047 section 3a |
| Passive pointers, at most three lines without fact text, behind `PointerGate` | `pointers` | ADR-0047 section 3b, ADR-0045 section 3 |
| Full facts with provenance, up to 50 | `search` | ADR-0047 section 3c, ADR-0045 |

`search` and `pointers` fuse a lexical ranking (SQLite FTS5 bm25 over the text, the entity's
names, Russian stems and transliteration) with a dense ranking (bge-m3 int8 cosine) by RRF.
Without the embedder, or while its helper is down, they answer lexical with
`lexical_only: true`.

The seedbed records the night has not processed answer beside the facts (ADR-0055 section 3).
A record has no vector, so it ranks by the lexical side alone, and it answers a request from its
writing until its processing, within its valid interval, before its expiry and, when private,
only with `private`. In `search` it is a fact `seed:<seq>` with `unprocessed: true`, and with
`disputed: true` and `conflicts_with` when it disputes a current fact. In `pointers` its hint is
`there is an unprocessed record about <entity> (<kind>)`, or `(<kind>, disputed)`, one pointer
per entity and none for a record without an entity. A `search` that finds only records is no
miss.

## Running the daemon

```sh
ulei-garden serve [--config PATH]
```

It reads `ulei.toml` in the path order of ADR-0040 section 2, opens the store, binds the
socket and prints one line `ulei-garden ready <socket>`. The keys are the `[garden]` section of
C9 ([`contracts/c9-ulei-toml.schema.json`](../../contracts/c9-ulei-toml.schema.json)),
explained in ADR-0047 section 4; it also reads `inbox.socket` to publish misses. A minimal
file, with placeholder paths:

```toml
[inbox]
socket = "<runtime>/ulei/inbox.sock"

[garden]
store = "<state>/ulei/garden/garden.sqlite3"
socket = "<runtime>/ulei/garden/garden.sock"
producer_key_file = "<inbox-root>/producer.key"
core_file = "<state>/ulei/garden/core.md"
```

The socket's directory belongs to the daemon's account and no other user can enter it.

The embedding model runs only in the helper `ulei-garden-embed`, a glibc build with the
`onnx` feature that loads ONNX Runtime at run time. With `garden.embedder.enabled = true` the
daemon starts it and needs `garden.embedder.helper`, `garden.embedder.model_dir` (holding
`onnx/model_quantized.onnx` and `tokenizer.json` of bge-m3) and `garden.embedder.ort_library`
(`libonnxruntime.so` of an official Linux x64 CPU release). The model and the library stay
outside the repository; nothing is downloaded at run time (ADR-0045, ADR-0047 sections 1, 2
and 10).

## C7 over the socket

One request per connection: `POST /v1/<method>`, HTTP/1.1 with a JSON body of at most 1 MiB,
answered `200` with the method's output or another status with C7 `Error`. Only the daemon's
own uid may connect. The schema and its `x-transport` are
[`contracts/c7-garden.schema.json`](../../contracts/c7-garden.schema.json); the statuses are in
ADR-0047 section 2.

| Method | Does |
|---|---|
| `search` | full facts for a query, with `as_of`, `private` and `k` |
| `pointers` | at most three pointer lines for a prompt |
| `propose` | a candidate into the seedbed: `seeded` with its ref `seed:<seq>` and `conflicts_with` when disputed, or `rejected` by the refusal screen; a repeat of a stored record answers that record |
| `digest` | the items waiting for the owner |
| `resolve` | the owner's decision on a digest item |
| `health` | version, state of the dense side, failed proposes, unpublished misses |
| `seedbed_take` | the night stage's batch: up to `limit` unprocessed seedbed records written at or before `until`, oldest first; leases nothing |
| `seedbed_done` | hands each record, or the candidates given for it, to the edge and marks it processed; an unknown ref is `404 not_found` before any item |

`seedbed_done` decides each record in its own unit of work: a candidate without `valid_from`
starts at the record's `written_at`, every proposal row names the record in `seed`, and a record
already processed answers its stored outcome and writes nothing (ADR-0055 section 5). Until the
night process (issue:509d9dc62473f22ed7d7) calls these two methods, seedbed records do not
become facts and the digest receives nothing new from them; `search` and `pointers` find
them as unprocessed while they wait.

```sh
curl -sS --unix-socket <runtime>/ulei/garden/garden.sock -d '{}' http://localhost/v1/health
```

A `search` miss goes to the miss outbox and is published to the inbox as a C1 `garden_miss`
event signed with the producer key (ADR-0047 section 5, ADR-0049).

## Binaries, budgets and CI

| Binary | Build | Held to |
|---|---|---|
| `ulei-garden` | static musl, without `onnx` | the `ulei-garden` section of `ci/budgets.sh` |
| `ulei-garden-embed` | glibc, with `onnx` | the `ulei-garden-embed` section of `ci/budgets.sh` |
| `garden-eval` | any target; `onnx` for the real model | the CI job `garden-eval` |

The build commands are in the root [`AGENTS.md`](../../AGENTS.md), "Commands"; the limits and
what each measures are ADR-0047 section 7 and the header of `ci/budgets.sh`. CI runs the
`budgets` job, `garden-eval` (recall@3 and warm p50 on the synthetic gold set, ADR-0045
section 5) and `garden-onnx` (the ONNX path and the daemon with its real helper on a tiny
model written at run time, ADR-0047 section 8). No model weights and no personal data enter
the repository or CI.

## The real gold set

Retrieval on the owner's corpus and gold set runs only outside the repository: the owner's
secretary agent follows [`docs/runbooks/garden-gold.md`](../../docs/runbooks/garden-gold.md) on
the owner's host after a merge and reports figures only. The procedure and the targets are
ADR-0050.

## Check

```sh
python3 -m ci.broad_check
```
