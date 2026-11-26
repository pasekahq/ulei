# ulei-garden

The garden: typed facts about the owner's life with two time axes and provenance, the edge in front
of it that decides what enters, and retrieval over it in three tiers. It speaks C7 to its callers
and C1 (`garden_miss`) to the owner inbox. Decisions: ADR-0043 to ADR-0051 and ADR-0055 in
[`docs/architecture/adr/`](../../docs/architecture/adr/); rules for coding agents:
[`AGENTS.md`](AGENTS.md).

The model sees the garden through `ulei-engine` (ADR-0077): the core profile ends the system
prompt of every turn, an owner turn carries the garden's pointers, and the tools `garden_search`
and `garden_propose` call C7 `search` and `propose`. The second-level providers over the wire are
not built yet (ADR-0047 section 9). The daemon serves any local C7 client on its socket. Both
binaries are in the release and run under the user unit `ulei-garden.service` (ADR-0075; below).

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
- Forget marks of seedbed records and the queue of the facts forgotten refs reach, which the
  night closes with no new version (ADR-0051 section 5; `forget` below).
- The edge: when the night's processing hands it a record, it accepts the candidate on its own
  unless it contradicts a current fact, has low confidence or names an ambiguous entity; those
  wait in the digest until the owner settles them with `resolve`. A new fact without an
  attribute is reconciled later against the most similar facts of its entity (ADR-0046
  section 2).
- The places bed (ADR-0080): places as `place` entities with a status (`candidate`, `active`,
  `gone`) and the source that found them, typed attributes each with its own `checked_at`,
  `source`, `confidence` and a due time from the TTL of its key, the replaced and older values
  in a history, the owner's verdicts and the food profile. Bed rows are not facts: `search`,
  `pointers` and the core profile do not read them; `places_search` (`places_search`) is their
  one reader. Only `places::Bed` writes them, in process and for the C7 methods `places_write`,
  `places_verdict` and `places_profile`, which `ulei-garden places import` calls (below).

## Retrieval tiers

| Tier | Method | Decided in |
|---|---|---|
| Pinned core profile, rendered to `garden.core_file` within `garden.core_char_limit` | written by the daemon | ADR-0047 section 3a |
| Passive pointers, at most three lines without fact text, behind `PointerGate` | `pointers` | ADR-0047 section 3b, ADR-0045 section 3 |
| Full facts with provenance, up to 50 | `search` | ADR-0047 section 3c, ADR-0045 |

`search` and `pointers` fuse a lexical ranking (SQLite FTS5 bm25 over the text, the entity's
names, Russian stems and transliteration) with a dense ranking (bge-m3 int8 cosine) by RRF.
Without the embedder (`health` answers `dense: off`) they are lexical; while its helper is down
they answer lexical with `lexical_only: true`.

A fact or a record whose evidence is a history archive item (`source_ref` starting with
`archive:`, what the night lifts from past conversations) has its fused score multiplied by
`SearchParams::history_weight`, 0.1 by default, before the reranker and the cut to `k`; the hit's
`score` is the weighted one (ADR-0081). With `rrf_k = 0` a history hit needs a ten times better
rank than a hit of other evidence to pass it, and it still ranks first when nothing else
matches. The weight is a constant of the build, not a key of `ulei.toml`.

The seedbed records the night has not processed answer beside the facts (ADR-0055 section 3). A
record has no vector, so it ranks by the lexical side alone, and it answers a request from its
writing until its processing, within its valid interval, before its expiry, while it is not
forgotten and, when private, only with `private`. In `search` it is a fact `seed:<seq>` with
`unprocessed: true`, and with `disputed: true` and `conflicts_with` when it disputes a current fact.
In `pointers` its hint is `there is an unprocessed record about <entity> (<kind>)`, or `(<kind>,
disputed)`, one pointer per entity and none for a record without an entity. A `search` that finds
only records is no miss.

## Running the daemon

```sh
ulei-garden serve [--config PATH]
```

It reads `ulei.toml` in the path order of ADR-0040 section 2, opens the store, binds the
socket and prints one line `ulei-garden ready <socket>`. A failure to start is one stderr line:
exit 2 when the config is refused, which no restart fixes, else exit 1, a
`garden.producer_key_file` the inbox has not created yet included (ADR-0075 section 4). The keys are the `[garden]` section of
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
outside the repository and the release; nothing is downloaded at run time (ADR-0045, ADR-0047
sections 1, 2 and 10).

## Importing a places file

```sh
ulei-garden places import FILE [--dry-run] [--config PATH]
```

A client of the daemon's socket (`garden.socket`): it checks every line of a places file in the
format of ADR-0080 section 4, rehearsing its writes on a scratch in-memory garden, and writes a
file without a refused line through `places_write`, `places_verdict` and `places_profile`. It
prints one JSON object: the report, or an error listing every refused line with its number and
rule (exit 1). `--dry-run` prints the counts of the file and connects to nothing. Converting a
free-form list into the format: [`docs/runbooks/places-import.md`](../../docs/runbooks/places-import.md).

## Release and unit

The release holds `ulei-garden`, static musl like the other daemons, and `ulei-garden-embed`, its
one glibc binary: `ort` opens ONNX Runtime with `dlopen`, which a static musl binary does not
have, and the official ONNX Runtime builds are glibc builds. The release checks that the helper
links only libc, libm, ld-linux and libgcc_s. On the host the helper needs glibc,
`libonnxruntime.so` of an official Linux x64 CPU release and the bge-m3 model directory; the
release ships neither, and the daemon needs none of them while the embedder is off (ADR-0075).

`deploy/install.sh` puts both binaries into `$HOME/.local/bin` and
`deploy/systemd/ulei-garden.service` into the user's unit directory. The unit runs `ulei-garden
serve` with the store and the core profile in `$XDG_STATE_HOME/ulei/garden/` and the socket in
`$XDG_RUNTIME_DIR/ulei/`, `AF_UNIX` only, and starts the helper as its child within its
`MemoryMax=3G` and `TasksMax=32`. It is restarted on a failure, not on exit 2. Setup, the
embedder and the move from a hand-made unit:
[`docs/runbooks/garden-service.md`](../../docs/runbooks/garden-service.md).

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
| `health` | version, state of the dense side, failed proposes (`propose` calls and `seedbed_done` and `forget_done` items the store rolled back), unpublished misses |
| `seedbed_take` | the night stage's batch: up to `limit` unprocessed seedbed records not forgotten written at or before `until` and after the record `after` by `seq`, oldest first; leases nothing |
| `seedbed_done` | hands each record, or the candidates given for it, to the edge and marks it processed; an unknown or forgotten ref is `404 not_found` before any item |
| `forget` | the owner's forget of `refs` with `reason` and `provenance`, or with `undo` its undo: marks or lifts seedbed records and queues or unqueues the facts the refs reach for the night |
| `forget_take` | the night stage's batch of the forget queue: up to `limit` rows not taken with an `id` after `after`, by `id`; leases nothing |
| `forget_done` | clears each row of `ids`: closes its fact if still current, with no new version, and marks the row taken; an unknown id is `404 not_found` before any row |
| `places_search` | places of the bed by status, distance from `near` or the profile's `home`, cuisine, rating and price, without the latest verdict -1 or a banned cuisine, ranked by verdict, vibe, quality with stale demotion, distance and id; each with its attributes' age and staleness, its marks and `maps_url`; `open_now` answers `open: unknown` until `hours` are read |
| `places_write` | a batch of places of the bed by `id` or `name` with their attributes' observations, one unit of work; answers per place and attribute what was written or the rule that refused it |
| `places_verdict` | one verdict of the owner on a place of the bed; a repeated `source_ref` answers its row `repeated` |
| `places_profile` | one row of the bed profile; a key other than `ban` keeps one row, and a row as stored answers `unchanged` |

`seedbed_done` decides each record in its own unit of work: a candidate without `valid_from`
starts at the record's `written_at`, every proposal row names the record in `seed`, and a record
already processed answers its stored outcome and writes nothing (ADR-0055 section 5). A failed
item answers `500 store_failed` and leaves the items before it processed. The night gardener
`ulei-engine night` (ADR-0078), run by the user timer `ulei-night.timer`, calls these two
methods; until it processes them, `search` and `pointers` find the records as unprocessed.

`forget` takes `seed:<seq>`, `c8:<n>` and `c8:session:<id>` refs and refuses `archive:<id>`
(ADR-0051 section 5, ADR-0055 section 6). A seedbed record gets a row of `seed_marks` with the
time, reason and provenance, and is forgotten while one of its marks is not lifted: `search`,
`pointers` and `seedbed_take` leave it out and `seedbed_done` answers it `404 not_found`. `undo`
lifts the live mark, keeping it with the lift's time and provenance; marks are never deleted.
Every ref puts into `forget_queue` one row per current fact whose `source_ref` is the ref or,
for `seed:<seq>`, that an accepted proposal of that record wrote, with the mark's time and
reason; `undo` removes the rows of its refs the night has not taken. A `c8:` ref marks nothing
in the garden: the session journal holds its marks. Garden facts are never marked; the night
gardener `ulei-engine night` (ADR-0078), run by the user timer `ulei-night.timer`, takes
`forget_queue` with `forget_take` and clears it with `forget_done`, and `reconcile_queue` is a
different queue.

`forget_done` clears each row in its own unit of work. A row not taken closes its fact, when
still current, on the transaction axis at the call's time with no version replacing it, drops
the fact's re-check and is marked taken; `closed` is true only when this call closed the fact.
A row already taken answers `closed: false` and changes nothing. The closed version keeps its
pin, index row and vectors for `as_of`; it leaves the core profile, which the daemon renders
after a `forget_done` that closed a fact, and the resident matrix, whose worker it wakes. A
failed row answers `500 store_failed` and leaves the rows before it cleared.

```sh
curl -sS --unix-socket <runtime>/ulei/garden/garden.sock -d '{}' http://localhost/v1/health
```

A `search` miss goes to the miss outbox and is published to the inbox as a C1 `garden_miss`
event signed with the producer key (ADR-0047 section 5, ADR-0049); a `search` with `no_miss`
queues none. The worker first asks the second level and writes what it finds into the seedbed
as `propose` does: a miss with a candidate stored as a record is not published, and an expired
fact's recheck with one is cleared, its new version written when the night processes the
record (ADR-0055 section 2).

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
model written at run time, ADR-0047 section 8); `release` ships both binaries and
`systemd-units` starts the unit without the model and checks `health` and a `search` (ADR-0075).
No model weights and no personal data enter
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
