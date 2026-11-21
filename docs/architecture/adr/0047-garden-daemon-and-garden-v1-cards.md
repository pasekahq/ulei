# ADR-0047: The garden as a live module: daemon, C7 transport, retrieval tiers, `[garden]` keys and the Garden v1 cards (C7 0.3.0, C9 0.2.0)

Status: accepted
Date: 2026-11-20
Affects: crates/garden, contract:C7, contract:C9, dep:hmac, dep:sha2, dep:rustix

## Context

`ulei-garden` is an in-process library (ADR-0043 to ADR-0046): a store on SQLite with FTS5,
an edge that decides candidates, a hybrid search with a pointer gate, an optional ONNX embedder
and the facade `api::Garden` speaking C7 types. No process serves it, so no other module can
reach it: ADR-0046 section 1 left the transport of the C7 `x-methods` "to when a daemon serves
them", `api.rs` does not serve the pinned core profile (tier a), `garden_pointers` uses the
lexical `Retriever` and a bare score threshold while the hybrid `search::pointers` with its
`PointerGate` stands beside it, and a miss is a `GardenMiss` value without a MAC that nobody
signs or publishes.

What the garden meets around it:

- Modules talk only through `ulei-contracts` and the wire (ADR-0001 section 3); the daemons
  serve their contracts as `POST /v1/<name>` HTTP/1.1 with JSON on a Unix socket, one request
  per connection (C1, ADR-0007; C6, ADR-0008).
- The engine's MCP client speaks Streamable HTTP to loopback TCP servers only; stdio servers
  are not part of that client, and `ulei-engine serve` connects its MCP servers at the start of
  each turn and closes them at its end (ADR-0026).
- The engine builds a session's system prompt once from `engine.soul_file`,
  `engine.memory_file` and `engine.user_file` and restores it while the session has history
  (`prompt::build`, `prompt::restore`); the agent edits the memory and user files with
  `write_file` and `patch` (ADR-0042 section 3). A C6 `submit_turn` may carry `context`,
  appended to each request of that turn and never journaled.
- One `ulei.toml`, checked against the C9 catalogue by `ulei_contracts::config` (ADR-0040,
  ADR-0041).
- C1 packets carry an HMAC-SHA256 under the inbox's one 32-byte producer key (ADR-0007
  section 5); the inbox's single consumer delivers each event into the owner session (ADR-0036).
- Every daemon: at most 2.5 MiB static, 4 MiB peak RSS after 10^4 requests, 10 ms to ready,
  0 wake-ups when idle (ADR-0001). bge-m3 int8 is a 569.7 MB file; the retrieval prototype
  measured 25.3 s for a fresh process to load and warm it and a warm p50 of 95 to 159 ms per
  query on CPU.

## Decision

### 1. Host and transport

| Option | Static size (estimate) | Peak RSS (estimate) | Rules touched |
|---|---|---|---|
| A. Daemon `ulei-garden serve` on a Unix socket: the C7 methods as `POST /v1/<name>`, HTTP/1.1 and JSON, one request per connection, peer uid check; the one process that opens the store; the model in the helper process of the table below | Daemon, static musl, without `ort`: 1.65 MB for the inbox-shaped base (ADR-0007 section 3: JSON, HTTP/1.1 on a Unix socket, peer check, HMAC, bundled SQLite), +140 KB FTS5 (ADR-0044), +200 to 300 KB garden code: 2.0 to 2.1 MB, under the 2,621,440 B daemon row. Helper `ulei-garden-embed`, dynamic glibc: `ort` bindings, the tokenizer, `serde_json`, `sha2`: 0.6 to 1.0 MB | Daemon: about 2.3 to 4 MB, the inbox prototype's figure (2,292 kB at `cache_size=-512`, 3,956 kB at the default) plus a resident matrix of n x dim f32 (section 3c). Helper, only with the model: the weights of bge-m3 int8 (569.7 MB file) and the runtime's arenas, several hundred MB, once per host | ADR-0046 section 1 (the transport is fixed here). ADR-0001's daemon row binds the daemon; the model's RSS is the helper's (section 7). ADR-0001 section 8 (static musl binaries): the helper is the one dynamic binary. New dependencies of the crate: `rustix` (peer uid), `hmac`, `sha2` (section 5). A `[garden]` row in the key table of ADR-0040 section 3. |
| B. MCP stdio server for the engine, the C2 tools `garden_search` and `garden_propose` | As A plus about 40 KB of JSON-RPC framing, `initialize`, `tools/list`, `tools/call` | As A per process, and one process per engine turn: with the model, the load per turn (25.3 s in the prototype) is paid on every turn | ADR-0026: the engine has no stdio MCP client, and connects MCP servers per turn, so the store, the matrix and the model are opened per turn; the passive tier cannot meet any latency budget; no process lives between turns for the expiry, rechecks and miss publishing; a second caller (the core's C2 facade) needs a second process on the same store. |
| C. Both modes of one binary: A, plus `ulei-garden mcp`, a stdio facade forwarding C2 calls to the daemon over its socket | As A plus about 50 KB for the facade | As A for the daemon; under 2 MB per facade process | As A; and C2 is the core's MCP facade (roadmap P1; the core holds the MCP facade, root AGENTS.md), so the garden would carry a second MCP face of the same tools; no client in the deployment speaks stdio MCP to it. |
| D. The library linked into the engine | The engine grows by the garden, +FTS5 already counted (ADR-0044) | Every engine process carries the store and the model | Refused by ADR-0001 section 3 and hard rule 1: a module crate depends on no other module crate. |

A is chosen. Only a resident process can hold the store, the dense matrix and the model between
requests, so only A can answer the passive tier on every turn within a budget, run the expiry,
rechecks and miss publishing between requests, and serve every caller (the engine, the core's
C2 facade) from one store. B pays the model load per turn and needs an engine stdio client this
sprint does not build. C adds a second MCP face to what the core's C2 facade will forward over
C7. D breaks the crate graph.

Where the model runs inside A. The release binaries are static musl (ADR-0001 section 8,
ADR-0042 section 10), and `ort` with `load-dynamic` opens ONNX Runtime with `dlopen`, which a
static musl binary does not have: musl's static `dlopen` is a stub that fails with "Dynamic
loading not supported". The ONNX Runtime releases are glibc builds, which a musl process could
not load even dynamically; there is no official musl build (section 10).

| Option | Size | Rules touched | Verdict |
|---|---|---|---|
| A1. The whole daemon as a dynamic `x86_64-unknown-linux-gnu` build with `onnx` | about the size of A, without libc | ADR-0001 section 8 and ADR-0042 section 10 for the daemon itself: every deployment, with or without the model, runs a glibc-bound daemon; an ONNX Runtime fault kills the store's process | Refused |
| A2. ONNX Runtime linked statically into the musl daemon | ONNX Runtime's Linux x64 CPU archive alone is 8.3 MB compressed (1.23.1), several times the 2,621,440 B row | ADR-0001's size row; no maintained musl build of ONNX Runtime or musl archive of `ort`'s prebuilt binaries was found (section 10) | Refused |
| A3. The daemon static musl without `ort`; the model in a helper `ulei-garden-embed`, a dynamic glibc build with `onnx` that `dlopen`s the official ONNX Runtime, started by the daemon and spoken to over a pipe | daemon as A, helper 0.6 to 1.0 MB | ADR-0001 section 8 for the helper only; the helper is installed and started only where the model is enabled | Chosen |

A3 keeps every deployment's daemon inside its budgets and static, isolates the one dynamic,
crash-prone part, and follows the engine's precedent of a helper binary for what the daemon
must not link (`ulei-web`, ADR-0015). The pipe costs a copy of 4 KB per 1024-dimension vector.

### 2. The daemon and the wire

- `crates/garden/src/main.rs` builds the binary `ulei-garden`; `ulei-garden serve [--config
  PATH]` reads `ulei.toml` through `ulei_contracts::config` (path order of ADR-0040 section 2),
  takes the `[garden]` keys of section 4 and `inbox.socket`, opens the store at `garden.store`
  (migrating it), seeds the domains, binds `garden.socket` and writes one ready line to stdout.
  `garden-eval` stays a separate binary of the crate.
- Socket: mode 0600 in a directory of the daemon's uid that no other user can enter, as the
  engine binds its C6 socket; at most 107 bytes. Every connection's peer uid (`SO_PEERCRED`
  through `rustix`, as `ulei-inbox`) must equal the daemon's uid, else `403 forbidden` and close.
- One request per connection: `POST /v1/<method>` for each C7 `x-methods` entry, a JSON body of
  at most 1 MiB, the method's input; `200` with its output; any other status carries C7 `Error`
  (`code`, `message`): `400 invalid_request` (body not the input schema), `404 unknown_method`,
  `404 not_found` (a `resolve` of an unknown or closed item), `409 replacement_refused`,
  `413 too_large`, `500 store_failed`. Every read and write of a connection has a 5 s deadline.
- The accept loop serves one connection at a time on its own store connection; requests are
  short and the owner is one caller. A second thread, the worker (section 6), holds its own
  connection on the same WAL database.
- `health` (C7 0.3.0) answers `version`, `dense` (`off` without the embedder, `ready`, `down`),
  `dense_error` while it is `down`, `failed_proposes` (proposes rolled back by a store failure
  since start, each also one stderr line `garden: propose failed: <error class>` without the
  candidate) and `unpublished_misses` (rows waiting in the miss outbox, section 5). It is the
  operator's view of the daemon and the source of a later `garden:` line in `ulei status`.
- The model: with `garden.embedder.enabled`, `serve` starts the helper `garden.embedder.helper`
  (`ulei-garden-embed`, section 1) before it binds the socket, with the model directory, the
  ONNX Runtime library and the thread count as arguments, and waits for the helper's ready line,
  which carries the model's identity and dimension, for at most `garden.embedder.load_timeout`
  seconds. A helper that cannot be started, exits, or sends no ready line in time stops the
  start: one stderr line `garden: embedder: <reason>` and exit 1, nothing bound, so the
  supervisor and `ulei status` see the daemon down. The daemon never starts lexical-only when
  the model is enabled.
- After the start, a helper that exits or misses the 5 s deadline of a call is restarted by the
  worker after 1, 2, 4 and up to 60 s. Until it is back, `health` answers `dense: down` with the
  reason, every `search` and `pointers` answer carries `lexical_only: true` (C7 0.3.0), and the
  daemon writes one stderr line when the dense side goes down and one when it is back. A caller
  therefore always sees a lexical-only answer as such.
- The pipe to the helper carries one call at a time: newline-delimited JSON requests, each
  answer a JSON header line with the count and dimension followed by the vectors as
  little-endian f32. A query of `search` or `pointers` never waits behind a batch of the
  worker: the worker sends one passage per call and only when no request has arrived for 2 s,
  and a waiting query takes the pipe before the worker's next passage. A query therefore waits
  at most for the one passage in flight (at most 512 tokens); card 21's procedure measures that
  wait against the 150 ms deadline of section 3b.
- C7 0.3.0, additive, fixes the transport as `x-transport` and adds `Error` (section 8, card 1).
- `garden-eval` and tests keep using the library in-process; nothing else opens the store file.

### 3. The three tiers

#### a. The pinned core profile

- A fact is pinned when the owner proposes it with `pin: true` (C7 0.3.0 `ProposeRequest.pin`,
  `Fact.pinned`). The edge accepts `pin` only from `source_kind: owner`; from any other source,
  and on a secret-pointer fact, the candidate is `invalid`. A version that closes a pinned
  version of the same entity and attribute inherits the pin, so a new address stays in the
  core. The owner unpins with a propose of the same entity and attribute and `pin: false`: a pin
  change is an update, never a `duplicate`.
- The daemon renders the core to `garden.core_file` at start and after every write that changes
  a pinned current fact: one entry `<entity name>: <fact text>` per pinned current fact,
  public and private, ordered by entity name, then by the time the fact was first pinned;
  entries joined by `\n§\n`, the entry delimiter of the engine's memory files; written to a
  temporary file in the same directory with mode 0600 and renamed over the old one, so a reader
  sees the old or the new file, never a part.
- The length budget is `garden.core_char_limit` characters (default 1375, the engine's default
  `user_char_limit`). Entries are taken in order while the joined text fits; the rest are left
  out, and the render writes one stderr line `garden: core profile over its budget, <n> facts
  left out`. Nothing is truncated inside an entry.
- How it reaches the prompt: the engine reads `garden.core_file` when it builds a new session's
  prompt, as a read-only block next to MEMORY and USER. This is the engine step of section 9:
  `ulei-engine` reads no garden file today. Pointing `engine.user_file` at the core file instead
  is refused: the agent edits the user file with `write_file` and `patch`, and every render
  would overwrite those edits.

#### b. Passive pointers on every request

- The garden serves `pointers` (C7) with no model call beyond the query embedding, no LLM, at
  most three lines without fact text, behind the hybrid `PointerGate` (card 9).
- The caller is the engine, at the start of every owner turn: it sends the prompt to
  `pointers` with a deadline of 150 ms, and passes the lines in the turn's ephemeral context
  (the mechanism of C6 `SubmitTurnRequest.context`: sent with each request of the turn, never
  journaled); a late or failed answer starts the turn without pointers.
- The engine side is not part of Garden v1. It ports the reference agent's memory provider
  hooks (the per-turn prefetch, the static prompt block and the memory tools, around
  `agent/memory_provider.py`), and the root AGENTS.md requires a port-map entry for every such
  behaviour; none of the sprint's remaining work d1 to d7 touches the engine. Garden v1 ends at
  the wire with the latency budget of section 7; section 9 names the engine step.
- `pointers` publishes no miss (amending ADR-0046 section 5): a passive miss happens on most
  prompts and would put an inbox event into the owner session for each of them, so
  `garden_pointers` returns no miss and writes no outbox row.

#### c. `garden_search` to the model

- The path: the model calls the engine's built-in tool `garden_search`, whose input is C2
  `GardenSearchInput`; the engine calls C7 `search` on the garden socket and returns the facts
  as the tool result, `pinned`, `recheck_before_acting` and the provenance fields included.
  `garden_propose` follows the same path to C7 `propose`. Both tools are the engine step of
  section 9, beside the core block and the pointers.
- Until then any wire client reaches `search`; the core's C2 facade (roadmap P1) forwards to the
  same methods.
- The dense side keeps a resident f32 matrix of the current versions' vectors of the active
  model, updated by the worker after each embedded passage and on every superseded version; an
  `as_of` search before now falls back to decoding vectors from SQLite (ADR-0045). At 10^4
  facts x 1024 dimensions the matrix is 40 MB and a brute-force scan about 10^7 multiply-adds
  per query. No ANN index is built: `instant-distance` 0.6.1 and `hnsw_rs` 0.3.x (both MIT OR
  Apache-2.0, checked on crates.io and docs.rs on 2026-11-20) are the candidates once the garden
  passes about 10^5 facts, each with its own ADR then.

### 4. The `[garden]` section of C9 0.2.0

C9 0.2.0 adds these keys; kinds are those of ADR-0040 section 2, decimals are written as strings
(`"0.6"`). `ulei-garden serve` is their only reader and also reads `inbox.socket`.

| Key | Kind | Default | Meaning |
|---|---|---|---|
| `garden.store` | string | required | The SQLite file of the store. |
| `garden.socket` | string | required | The C7 socket, at most 107 bytes. |
| `garden.producer_key_file` | string | required | The 32-byte C1 producer key; the deployment points it at the inbox root's `producer.key`. Owner-only, as the inbox checks it (ADR-0007). |
| `garden.domains_file` | string | built-in list | JSON in the format of `src/starting_domains.json`; replaces the 20 starting domains as the seed. Every start adds the domains of the seed the store lacks and keeps every stored domain as it is, so a changed label, class or parent of a stored slug in the file is ignored. A seed with a cycle among its parents or a parent it does not hold stops the start. |
| `garden.core_file` | string | no render | The rendered core profile (section 3a). |
| `garden.core_char_limit` | integer | 1375 | The core profile's budget in characters. |
| `garden.reranker` | string | `none` | The reranker behind its trait; Garden v1 has no implementation, so `none` is the only value accepted. |
| `garden.second_level` | string | `none` | The second-level provider (section 5); Garden v1 accepts only `none`, the fake is injected by tests through the library. |
| `garden.edge.min_confidence` | integer or string | `"0.6"` | `EdgeConfig::min_confidence`. |
| `garden.edge.reconcile_min_cosine` | integer or string | `"0.85"` | Least cosine at which a new fact and a current fact of the same entity are reconciled (card 13). |
| `garden.api.search_k` | integer | 5 | Facts of a `search` without `k`, 1 to 50. |
| `garden.api.pointer_min_cosine` | integer or string | `"0.5"` | `PointerGate::min_cosine`; card 9 sets the default it tunes. |
| `garden.api.pointer_min_lexical` | integer or string | `"5.0"` | `PointerGate::min_lexical`; card 9 sets the default it tunes. |
| `garden.embedder.enabled` | boolean | false | Run bge-m3 through ONNX Runtime in the helper; a model that does not load stops the start (section 2). |
| `garden.embedder.helper` | string | required when enabled | The `ulei-garden-embed` binary (section 1). |
| `garden.embedder.model_dir` | string | required when enabled | Holds `onnx/model_quantized.onnx` and `tokenizer.json`; the helper reads it. |
| `garden.embedder.ort_library` | string | required when enabled | `libonnxruntime.so` of an official Linux x64 CPU release (a glibc build), opened by the helper. |
| `garden.embedder.threads` | integer | 2 | Intra-op threads of the helper. |
| `garden.embedder.load_timeout` | integer | 120 | Seconds `serve` waits for the helper's ready line at start. |
| `garden.ttl.<kind>` for `weather`, `fx_rate`, `duty_pharmacy`, `news`, `price`, `availability`, `timetable`, `opening_hours`, `contacts`, `product`, `law`, `place`, `general`, `personal` | integer or string | the row of ADR-0044 decision 3 | Seconds above 0, or `"never"`. `never_cache` has no key: live data is refused, not cached. |
| `garden.schedule.expire_interval` | integer | 60 | Least seconds between two expiry passes (section 6). |
| `garden.schedule.recheck_interval` | integer | 3600 | Least seconds between two recheck batches. |
| `garden.schedule.recheck_batch` | integer | 8 | Rechecks handed to the second level per batch. |
| `garden.schedule.maintain_interval` | integer | 604800 | Seconds between two maintenance passes. |

The pointer thresholds are tunables of the API that depend on the model and the corpus, so they
live in the file; `MAX_POINTERS` (3) and the 50-fact cap of `search` are fixed by C7.

### 5. `garden_miss` and the second level

- A `search` that finds nothing for a non-trivial query writes one row into the garden's
  `miss_outbox` table (`ulei_garden::miss`), wakes the worker and answers at once. The row is
  keyed by the 64-bit FNV-1a hash of the normalised query (`edge::normalise`), written as 16
  lowercase hex digits; a query with a row younger than one day, or with a row not yet
  published, gets no second row, so repeating a question does not repeat the miss. The row
  holds the hash and the packet's payload, never the query itself: a query that looks secret
  (`secret::looks_secret`) gets a payload that does not repeat it. Published rows older than
  one day are removed when a miss is queued.
- The worker hands each new row to the second level, a trait in the crate
  (`ulei_garden::second::Source`: a query and `now` in, candidate C7 `ProposeRequest`s with
  `source_kind`, `source_ref`, `observed_at` and `ttl_kind` out). Its candidates go through the
  edge like any proposal, so they are written with provenance and the lifetime of their
  `ttl_kind` (or queued, or refused); the next same question is answered from the garden. A
  provider that finds something settles the row without publishing.
- When the provider finds nothing, fails, or is `none`, the worker publishes the miss: the C1
  `garden_miss` packet of the row (`source_ts` the second of the miss, `trust`
  `untrusted-source-data`), its MAC the HMAC-SHA256 of the canonical JSON of the six other
  fields under the key read from `garden.producer_key_file` at start, `POST /v1/publish` to
  `inbox.socket`, and only a `200` naming the id marks the row published. `serve` reads the key
  file and `inbox.socket` before it opens the store: a key file that is a symlink, belongs to
  another uid, is open to the group or others or does not hold exactly 32 bytes, or a missing
  key, stops the start with one stderr line naming the key. The id is `garden:miss:<first
  publish second>:<hash of the normalised query>`, kept on the row from the first attempt, so a
  retried publish is the same packet and the inbox's `INSERT OR IGNORE` makes it idempotent.
  After the first refused or failed publish of a pass the worker stops sending, and every due
  row it did not publish is retried after 1, 2, 4 and up to 60 minutes, counted by the row's
  own failed attempts; a failed publish writes no stderr line, `health.unpublished_misses`
  counts the waiting rows. The outbox keeps at most 1,000 unpublished rows: a new row over it
  drops the oldest unpublished one with one stderr line `garden: miss outbox holds 1000
  unpublished misses, dropped the oldest`.
- The garden signs because it observed the miss; the key is the inbox's one producer key, as
  for every C1 producer (ADR-0007). The engine never sees a miss value.
- Providers talk only over the wire: the forest through C6 `raw_sessions` of the runtime that
  serves it, the internet through C6 `submit_turn` on a dedicated session of the engine, whose
  web tools (`ulei-web`) fetch and whose qb route reads; the garden links no TLS and no client
  of a model gateway or vendor. Garden v1 builds the trait, its fake and the `none` provider;
  the two wire providers are named in section 9.

### 6. Background work

- One worker thread of `ulei-garden serve` does everything that is not a request: restarting
  the helper, embedding facts the store lists as missing vectors, updating the resident matrix,
  the expiry pass, rechecks, the second level, miss publishing and maintenance. The request path
  embeds only its own query, never a passage, never calls the second level and never waits for
  the inbox.
- The worker (`ulei_garden::worker`) starts after the bind on its own store connection and
  sleeps on a condition variable until the nearest deadline or a wake from a request (a new
  fact, a new miss). Deadlines: the earliest `expires_at` of a current fact, but not sooner than
  `garden.schedule.expire_interval` after the last pass; the next outbox retry; the next recheck
  batch when rechecks wait and a provider is set; the next maintenance pass, every
  `garden.schedule.maintain_interval` from the start. Between deadlines it wakes 0 times. Its
  time comes from the `worker::Clock` trait, so tests drive it on an injected clock.
- A failed pass is one stderr line `garden: worker: <error class>`; the pass is due again after
  its interval and requests are served as before.
- The request connection and the worker's connection write to the same WAL database. The
  outermost unit of work of the store (`Store::atomically` and every writing store method)
  starts with `BEGIN IMMEDIATE`, and both connections have a 5 s `busy_timeout`: a writer waits
  for the other instead of failing with `SQLITE_BUSY_SNAPSHOT` when a transaction that read
  first tries to write.
- Expiry: `Store::mark_expired(now)` (ADR-0044 decision 4). A copy whose `valid_to` is at or
  before its `recorded_at`, whether written by the expiry or by a slicing supersede, carries no
  `expires_at`: it is history, and an expiry on it would close and recheck it again. A slice
  still valid when it is recorded (the part after a bounded new version, or before one that
  starts later) keeps the original's `expires_at`, since it still answers for the present.
- Facts of a kind with `recheck_before_acting` (`law`) are returned with
  `Fact.recheck_before_acting` true (C7 0.3.0), so the model is told to re-check them before
  acting.
- Rechecks: each batch takes up to `garden.schedule.recheck_batch` rows of `rechecks`, asks the
  second level for the fact's text, passes its candidates through the edge as a new version and
  then calls `clear_recheck`; a recheck that finds nothing stays queued.
- Retention: closed copies are kept without limit, as every version is (ADR-0043): `as_of`
  answers stay reproducible. Nothing is deleted, so there is nothing for `VACUUM` to reclaim and
  it is never run; the maintenance pass runs the FTS5 `optimize` merge and `PRAGMA optimize`.
- Idle budget: 0 voluntary context switches of all threads in 10 s with no deadline due
  (section 7).

### 7. Budgets

`ci/budgets.sh` gains a `ulei-garden` section, driven by a new stdlib `ci/garden_run.py` on the
runner disk: a synthetic `ulei.toml`, a fresh store filled through C7 `propose` with 2,000
fictional facts, `garden.embedder.enabled` false, a fake C1 socket. The release build adds the
static daemon, `cargo build --release --locked -p ulei-garden --bin ulei-garden --target
x86_64-unknown-linux-musl`, and the dynamic helper, `cargo build --release --locked -p
ulei-garden --features onnx --bin ulei-garden-embed --target x86_64-unknown-linux-gnu`.

| Variable | Measures | First limit | From |
|---|---|---|---|
| `ULEI_GARDEN_MAX_BINARY_BYTES` | static release binary | 2,621,440 B | ADR-0001 daemon row |
| `ULEI_GARDEN_MAX_RSS_KB` | VmHWM after 10^4 requests (pointers, search, propose) | 4,096 kB | ADR-0001 daemon row |
| `ULEI_GARDEN_MAX_READY_US` | median spawn to the first `200` over restarts on the filled store | 10,000 us | ADR-0001 daemon row |
| `ULEI_GARDEN_MAX_IDLE_WAKEUPS` | voluntary context switches of all threads in 10 s idle | 0 | ADR-0001 |
| `ULEI_GARDEN_MAX_POINTERS_P50_US` | p50 of `pointers`, connect to last byte | 10,000 us | a tenth of the 100 ms target: the lexical path without the query embedding, the model's share |
| `ULEI_GARDEN_MAX_SEARCH_P50_US` | p50 of `search`, connect to last byte | 20,000 us | a fifth of the 100 ms target, for full facts and up to 50 rows |
| `ULEI_GARDEN_MAX_WRITE_BYTES` | wchar per accepted `propose` | 131,072 B | the inbox's per-event figure (ADR-0007) |
| `ULEI_GARDEN_EMBED_MAX_BINARY_BYTES` | release binary of the helper, which must be dynamically linked against glibc's own libraries only (`ldd` lists nothing else) | 2,097,152 B | the engine's helper row (`ulei-web`, ADR-0015) |

The daemon section checks that `ulei-garden` is static, as for the other daemons; the helper is
the one binary checked for the opposite. ADR-0001's 4 MiB binds the daemon; the model's RSS is
the helper's and is measured with the latency on the real corpus outside the repository (card
21), not in CI. A limit is lowered when a card measures well below it and never raised. If a
binary passes its limit, the lever is the package's `opt-level`, as for the engine (ADR-0027).

### 8. Implementation cards, in order

Each card merges alone with the workspace green, tests on fakes and synthetic data only (no
model downloads, no weights in the repository), and rewrites in place every document that
describes what it changes. A card that adds a dependency to the crate carries the short ADR
naming it that the ADR gate needs (as ADR-0041 did for C9), pointing to the section here. Items
of the sprint's remaining work are named d1 to d7 with their part.

1. C7 0.3.0 and C9 0.2.0 (about 100 min). d5: the contract bump with changelog and fixtures.
   C7: `x-transport` and `Error` (section 2), the method `health` with `Health` (section 2),
   `SearchResponse.lexical_only`, `PointersResponse.lexical_only`, `ProposeRequest.pin`,
   `Fact.pinned`, `Fact.proof_count`, `Fact.recheck_before_acting`, `DigestReason`
   `ambiguous_entity`, `DigestItem.entity_candidates`, `ResolveRequest.entity`. C9: the keys of
   section 4 with `docs/ulei.example.toml` lines. Schemas, fixtures and generated types only; no
   code reads them. Tests: valid and invalid fixtures of every addition, the generated round
   trip, `cargo xtask contracts`. Budget: none.
2. The daemon and the C7 transport (about 120 min). d5: the transport of the C7 `x-methods`.
   `ulei-garden serve`, the config with `garden.store` and `garden.socket`, the bind, the peer
   check (`rustix`, short ADR), the HTTP/1.1 server, the methods on `api::Garden` and `health`
   (`dense: off`, both counters 0 until cards 7 and 16 feed them), the error codes, the ready
   line. Tests: every method over a temporary socket, each error code, a foreign uid refused
   (through the peer check's seam), an unknown key in `ulei.toml` refused. Budget: none yet
   (card 4).
3. The garden's settings from `ulei.toml` (about 90 min). d1: starting domains overridable by
   config (`garden.domains_file`). d2: TTL from `ulei.toml` (`garden.ttl.*` into `TtlTable`).
   d3: `EdgeConfig` and `ApiConfig` from `ulei.toml` (`garden.edge.*`, `garden.api.*`). Tests:
   synthetic files for every key, a bad value naming its key, a domains file replacing the
   seed. Budget: none.
4. Garden budgets (about 110 min). d5: the garden's budgets: `ci/garden_run.py`, the daemon rows
   of section 7 in `ci/budgets.sh` with the static check, the daemon's musl release build line
   (without `onnx`) in AGENTS.md and CI. Tests: the driver's stdlib fake C1 socket and synthetic
   facts. Budget: the new `ulei-garden` section.
5. Domain and entity integrity (about 110 min). d1: merge of entities (a merge closes the
   merged entity's facts and rewrites them on the survivor, with lineage) and dedup by aliases
   of the entities a name identifies (person, place, item; events, visits and preferences
   never resolve by name); no cycles in the domain hierarchy (refused on upsert and in a
   domains file); every entity reference in attributes (the visit's `place` and `with`, the
   preference's `holder` and `object`) exists. Tests: store tests for each refusal and for a
   merge with `as_of` before it. Budget: none.
6. Secret detection (about 90 min). d4: numbers spelled out in Russian and English, short PINs
   after a key word, national id shapes (with checksums where they have one), phone numbers;
   the store's secret-pointer heuristic and the edge's `looks_secret` share the detectors.
   Tests: positives and near-miss negatives (dates, prices, addresses, postal codes), all
   fictional. Budget: none.
7. Propose in one transaction (about 110 min). d3: `propose` in one store transaction (the
   queue row, a new entity and the fact commit or roll back together); the observable failed
   state: a rolled-back propose leaves no row, the proposer gets `500 store_failed`, the daemon
   counts it in `health.failed_proposes` and writes one stderr line `garden: propose failed:
   <error class>`, so the stored `failed` status and its marking are removed; `proof_count` from
   duplicates (1 plus the `duplicate` rows naming the fact, returned as `Fact.proof_count`).
   Tests: an injected store failure leaves no rows, answers `store_failed` and raises
   `failed_proposes` in `health` by one; duplicates raise `proof_count`. Budget:
   `ULEI_GARDEN_MAX_WRITE_BYTES`.
8. Russian stems and transliteration in FTS (about 120 min). d5: Russian morphology and
   transliteration in FTS with column weights from the eval. A new `MIGRATIONS` entry rebuilds
   `fact_fts` with the columns text, names, stems and translit; stems come from the Snowball
   Russian stemmer ported into `crates/garden/src/morph.rs` (BSD-3-Clause, Martin Porter and
   Richard Boulton; notice in `THIRD_PARTY_NOTICES.md`); translit ports the prototype's letter
   table (`lexical_normalize.TRANSLIT`, `TOKEN_TRANSLIT`); column weights start at the
   prototype's 1.0, 0.75, 0.5 and are set from the synthetic eval. Tests: stem-change queries
   ("стоматологи" finds "стоматологу"), a Latin query finding a Cyrillic name, the eval floors.
   Budget: binary size and the latency lines.
9. Hybrid retrieval behind the API (about 120 min). d5: the hybrid retriever of `search.rs`
   behind `api::Retriever`, so `garden_pointers` uses `PointerGate` and `pointer_min_score`
   goes; the reranker behind a trait, off by default (`garden.reranker = "none"`); the
   thresholds `min_cosine` and `min_lexical` retuned on the synthetic set for pointer
   precision, with the new defaults in C9's description and section 4's keys. Tests: unrelated
   prompts get no pointers and related ones do, with the fake embedder; the reranker seam with a
   fake that reorders. Budget: the latency lines.
10. The embedder helper and model identity (about 120 min). d5: SHA-256 of the model and
    tokenizer files in the model's identity (`sha2`, ADR-0049), computed by the helper at load,
    so a changed artifact re-embeds. With it, the helper of section 1 and its pipe: the binary
    `ulei-garden-embed` (`--embedder fake|onnx`, `onnx` only with the feature), the protocol,
    the ready line with identity and dimension, `garden.embedder.*`, the start refusal, the
    restarts, `health.dense`, `lexical_only`, the query priority over the worker's passages, and
    the worker thread of section 6 with the embedding of missing vectors. Tests: against the
    helper with `--embedder fake`: a start refused when the helper exits or sends no ready line,
    `dense: down` and `lexical_only` after the helper is killed and `ready` after its restart, a
    query served before a queued passage, a changed fake artifact changing `model_id`. Budget:
    `ULEI_GARDEN_EMBED_MAX_BINARY_BYTES` and the helper's gnu release build line.
11. Resident matrix (about 90 min). d5: a resident dense matrix in place of reading vectors
    from SQLite per query (section 3c), kept by the worker. Tests: the matrix follows inserts
    and supersedes; `as_of` still finds old versions from SQLite. Budget: RSS and the latency
    lines.
12. Entity resolution beyond exact names (about 100 min). d3: resolution of entity names by
    normalised form, transliteration (card 8's table) and edit distance 1 on names of five
    characters or more; an ambiguous name goes to the digest as `ambiguous_entity` with its
    candidates and the reason, and `resolve` takes the chosen `entity`. Tests: a Latin spelling
    resolves to a Cyrillic entity; two near names give a digest item. Budget: none.
13. ADD intake with top-k reconciliation (about 120 min). d3: intake adds the candidate, then
    the worker reconciles it against the five most similar current facts of its entity through
    the hybrid retriever; a pair above `garden.edge.reconcile_min_cosine` with different
    normalised texts and no `attribute` is a digest `contradiction` (the conflicts of candidates
    without `attribute`); `resolve` `reject` retracts the new fact, `accept` the rival. Without
    an embedder there is no reconciliation. Tests: "Ada is vegetarian" against "Ada eats meat"
    with a fake embedder that puts them close. Budget: none.
14. The core profile (about 110 min). d5: the pinned core profile with its length budget
    (section 3a): `pin` at the edge, the inherited pin, the render to `garden.core_file`, the
    budget cut and its stderr line. Tests: render order, the cut, the atomic replace, a pin from
    a non-owner refused. Budget: none.
15. Expiry, law facts, history copies (about 110 min). d2: the `mark_expired` scheduler at the
    nearest expiry; facts of kind `law` returned with `recheck_before_acting`; history copies
    without `expires_at`; the retention of closed copies and the maintenance pass in place of
    `VACUUM` (section 6). Tests: on an injected clock, a pass at the nearest expiry and none
    before it; a history copy never expires, a slice still valid keeps its expiry. Budget:
    `ULEI_GARDEN_MAX_IDLE_WAKEUPS`.
16. Publishing `garden_miss` (about 120 min). d6: signing and publishing `garden_miss` through
    the inbox (section 5): the outbox, the C1 client, the HMAC (`hmac` and `sha2`, ADR-0049), the
    retries, the one-day coalescing, `health.unpublished_misses`; the
    pointers' miss removed. Tests: against a fake C1 socket in the test, the MAC verifies under
    the test key, a refused publish is retried and counted as unpublished, a repeated query
    publishes once. Budget: none.
17. The second level (about 110 min). d6: the `Source` trait with a fake; the answer written to
    the edge with provenance and TTL; an end-to-end test on fakes: a miss, the fake finds a
    fact, it is written with `source_kind: internet` and its TTL, the same question is
    answered from the garden and nothing is published; a fake that finds nothing leads to a
    publish. Budget: none.
18. Rechecks (about 90 min). d2: the flow rechecks, fresh lookup, new version,
    `clear_recheck`, on the worker at `recheck_interval`. Tests: an expired internet fact comes
    back as a new version from the fake source; a recheck that finds nothing stays queued.
    Budget: none.
19. Retrieval eval in CI (about 110 min). d7: a CI job with recall@3 and p50 thresholds on the
    synthetic set (`garden-eval --embedder fake` against the floors of
    `tests/eval_synthetic.rs`); the passage size chosen on the synthetic eval and every passage
    embedded (no `--dense-head` default). Tests: the job itself; eval unit tests. Budget: none.
20. The ONNX path in CI (about 120 min). d7: a CI job that really runs the ONNX path of the
    release on a tiny model: a stdlib script writes a tiny ONNX graph (embedding gather to
    `last_hidden_state`) and a small `tokenizer.json` of the bge-m3 pipeline shape at run time;
    the job downloads one pinned official ONNX Runtime Linux x64 release with its SHA-256, builds
    `ulei-garden-embed --features onnx` for `x86_64-unknown-linux-gnu`, starts the musl
    `ulei-garden serve` with `garden.embedder.enabled` on the tiny model and checks `health`
    `dense: ready` and a dense hit; it also runs the embedder tests with feature `onnx`. Weights
    are never committed. Tests: unit-length vectors, CLS pooling, truncation, the daemon with the
    real helper. Budget: none.
21. `as_of` vectors and the real gold (about 90 min). d7: embedding superseded versions for
    `as_of` (the worker embeds closed versions after current ones); the procedure for the real
    gold outside the repository (`docs/runbooks/garden-gold.md`: the helper, the model and
    runtime directories, `garden-eval --embedder onnx` built for `x86_64-unknown-linux-gnu`, the
    daemon with `garden.embedder.enabled`, the report fields) with the targets p50 at most 100 ms
    on CPU, recall@3 against the synthetic floors, the helper's RSS with the model, and a query's
    wait behind one passage (section 2). Tests: an `as_of` dense hit on a closed version. Budget:
    none in CI.

Order: the host (2) comes before everything that runs in it; settings (3) and budgets (4)
before the cards they measure; stems and translit (8) before resolution (12); the hybrid
retriever (9) and the helper with the worker (10) before the matrix (11) and reconciliation
(13); the helper (10) before the ONNX job (20) and the real gold (21); the expiry scheduler (15)
before miss publishing (16), the second level (17) and rechecks (18).

### 9. Not in Garden v1, named next steps

- The engine step: read `garden.core_file` into a new session's prompt, call `pointers` at the
  start of each owner turn into the turn's ephemeral context (section 3b), and the tools
  `garden_search` and `garden_propose` over C7 (section 3c). It ports the reference agent's
  memory provider hooks and is cut when the port map has its entry.
- The second-level providers over the wire: `engine` (C6 `submit_turn` on a dedicated session)
  and `forest` (C6 `raw_sessions`, once a runtime serves it).
- The unit, release and install of `ulei-garden` and `ulei-garden-embed` and a `garden:` line
  in `ulei status` from `health` (ADR-0040 section 5). Their shape against ADR-0042 section 10:
  `ulei-garden` joins the static musl binaries of the release; `ulei-garden-embed` is the one
  `x86_64-unknown-linux-gnu` binary in the same release directory and `SHA256SUMS`, built on
  the release runner, whose glibc is then the oldest glibc the helper runs on, and checked to
  need only glibc's own libraries; neither ONNX Runtime nor the model is shipped, both stay
  owner-provided paths of section 4. The install script copies the helper like the other
  binaries; a deployment without the model never starts it.
- Outside Garden v1 by the sprint issue and not needed by any card above: domain clustering,
  the recommender, background watchers, a GPU profile, a local or API reranker, L0/L1/L2
  abstracts, query rewriting by a model, person dossiers, a feedback method in C7, the
  loanword dictionary, an own inverted index. Card 13 decides contradictions without a model,
  by similarity; deciding them by meaning needs a model through the gateway and is not built.

### 10. External crates and models checked

Checked on 2026-11-20 by web search of crates.io, docs.rs, Hugging Face, PyPI, the musl mailing
list and the vendors' pages; the three workspace crates also against `Cargo.lock` and their
manifests.

| Name | Version | License | Use here |
|---|---|---|---|
| ONNX Runtime | 1.27.0 newest seen (Maven Central, 2026-06-30), 1.26.0 on PyPI | MIT | The official Linux x64 CPU release, opened by the helper at run time; card 20 pins one release with its SHA-256. |
| ONNX Runtime on Linux libcs | the PyPI wheels are tagged manylinux, glibc 2.27 and 2.28 or later; the 1.23.1 Linux x64 CPU archive is 8.3 MB compressed; no official musl or Alpine build, and its build expects glibc | MIT | Why the helper is a glibc build (section 1, A1 to A3). |
| musl `dlopen` in a static binary | a stub: `dlerror` reports "Dynamic loading not supported"; the musl maintainers call static `dlopen` not possible | MIT (musl) | Why the musl daemon links no `ort` (section 1). |
| `ort` prebuilt binaries | release archives named by target triple; no musl archive found (2.0.0-rc.12 release page) | MIT OR Apache-2.0 | Why A2 is refused. |
| `ort` | 2.0.0-rc.13 (newest on docs.rs), already pinned by ADR-0045 | MIT OR Apache-2.0 | Unchanged. |
| bge-m3, ONNX int8 export `onnx/model_quantized.onnx` | the ONNX conversion `Xenova/bge-m3` on Hugging Face, 569.7 MB | MIT, inherited from the base model | Run time only, never in the repository. |
| Snowball Russian stemmer (`russian.sbl`) | as shipped in `rust-stemmers` 1.2.0 | BSD-3-Clause | Ported into `morph.rs` (card 8); no dependency. |
| `rust-stemmers` | 1.2.0, released 2019 | MIT or BSD-3-Clause | Not used: it builds all of its languages into the binary where the garden needs one stemmer, and has had no release since 2019. |
| `rsmorphy` | 0.4.0, released 2018 | MIT OR Apache-2.0 | Not used: a lemmatiser with a separate dictionary crate (`rsmorphy-dict-ru`), edition 2015, described by its author as early stage, no release since 2018. |
| `instant-distance` | 0.6.1 | MIT OR Apache-2.0 | Not used (section 3c). |
| `hnsw_rs` | 0.3.x | MIT OR Apache-2.0 | Not used (section 3c). |
| `hmac` | 0.13.0 (`Cargo.lock`) | MIT OR Apache-2.0 | The MAC of `garden_miss` (card 16). |
| `sha2` | 0.11.1 (`Cargo.lock`) | MIT OR Apache-2.0 | The MAC and the model identity (cards 10, 16). |
| `rustix` | 1.1.6 (`Cargo.lock`) | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | The peer uid of the socket (card 2). |

Transliteration needs no crate: the prototype's letter table is ported, and the FTS5 tokenizer
already strips Latin diacritics (`remove_diacritics 2`).

## Consequences

- The garden runs as one daemon per host and is the only process that opens its store; every
  other module reaches it over C7 on its socket.
- Garden v1 serves all three tiers at the wire (the core as a file, pointers and search as C7
  methods) and publishes signed misses through the inbox; the model sees the garden only after
  the engine step of section 9.
- The lexical index changes from the prototype's lemma column to Snowball stems: stem changes
  that a stemmer cannot join (suppletive forms) stay unmatched, a deviation from the prototype
  recorded in card 8.
- Contradictions without an `attribute` are found by similarity and decided by the owner, not by
  a model.
- The model runs in the helper, the one dynamic glibc binary; the daemon stays static and held
  to ADR-0001, and the helper's RSS with the model comes from the procedure of card 21.
- With the embedder enabled the daemon starts only with a loaded model, and a lexical-only
  answer after a helper failure is marked as such in the answer and in `health`.
