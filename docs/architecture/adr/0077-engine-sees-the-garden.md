# ADR-0077: The engine sees the garden: the core profile block, pointers on owner turns, `garden_search` and `garden_propose`

Status: accepted
Date: 2026-11-26
Affects: crates/engine

## Context

Garden v1 serves its three tiers at the wire: the pinned core profile as the file
`garden.core_file`, passive pointers as C7 `pointers`, full facts as C7 `search`, and the seedbed's
write method as C7 `propose` (ADR-0047 section 3, ADR-0055 section 2). ADR-0047 section 9 names
the engine step that lets the model see them: the core profile in the prompt, pointers at the
start of each owner turn, and the tools `garden_search` and `garden_propose`. Part of it is in
`ulei-engine` already: `archive_search` asks C7 `search` first and lifts archive hits into the
seedbed, and `forget` calls C7 `forget` (ADR-0051 sections 5 and 6). Without the rest the garden
is invisible to the model: blocker B1 of the cutover, issue:76b6f0e180339e492edd part 2.

ADR-0047 section 3 decides most of the step. This record states what it leaves open and the two
places where the engine departs from its letter.

## Decision

### 1. The core profile block

- The engine reads `garden.core_file` (C9, ADR-0047 section 4; variable `ULEI_GARDEN_CORE_FILE`)
  at the start of every turn. Its entries, split on the memory files' delimiter `\n§\n`, each
  stripped, empty and repeated ones dropped, form one block under the frame of the MEMORY and
  USER blocks: `GARDEN CORE PROFILE (pinned facts about the owner, read-only) [<p>% —
  <n>/<limit> chars]`, the limit being `garden.core_char_limit` (`ULEI_GARDEN_CORE_CHAR_LIMIT`,
  default 1375, above 0). The daemon keeps the file within the limit; the engine only shows the
  usage.
- An unset key, an absent file or a file without entries is no block and no error. A file that
  exists but is not readable UTF-8 text is one stderr line `ulei-engine: garden core profile:
  <error>; the prompt has no block`, and the turn goes on without the block.
- The block is the last part of the system prompt, after `Conversation started:`. A session's
  prompt is still built once and stored (ADR-0020); each turn puts the block it read in place of
  the one the stored prompt ends with, adds it, or removes it, and stores the prompt again only
  when that changed it.
- This departs from ADR-0047 section 3a, which reads the file only when a new session's prompt is
  built. The owner's session lives for weeks, so a pin of today would reach the model only in its
  next session. As the last part, a changed block costs only the tail of the cached prefix, and an
  unchanged one changes no byte. MEMORY and USER keep the reference's per-session snapshot.
- The block is the reference agent's static memory provider block (`system_prompt_block` in
  `THIRD_PARTY_NOTICES.md#ref-agent`), with the file of the garden as its provider.

### 2. Pointers on owner turns

- At the start of every turn whose input the owner sent (C6 `submit_turn`, `ulei-engine run`
  without `--event`), with `garden.socket` set, the engine sends the prompt, trimmed, to C7
  `pointers` with `private` true and no `k` (so at most three). A blank prompt sends nothing.
  Event turns get no pointers, as the reference skips recall outside the owner's own turns.
- The call has the 150 ms deadline of ADR-0047 section 3b. The connect, the write and every read
  are each bounded by it, and no read goes past 150 ms after the start. A refused, failed or late
  call is one stderr line `ulei-engine: garden pointers: <error>; the turn runs without them`,
  and the turn starts without pointers. A turn is never delayed past the deadline nor failed by
  the garden.
- The pointer lines go into the turn's ephemeral context: after the caller's C6 `context`, joined
  by a blank line, and appended to the turn's input for every request of the turn, never
  journaled (ADR-0047 section 3b). They are ported from the reference's
  `build_memory_context_block`: the `<memory-context>` fence with its system note, then the line
  `Garden pointers: the garden holds facts that may bear on this message. Read them with
  garden_search before you rely on them.` and one `- <hint>` line per pointer. The hint is the
  garden's own one-line text without the fact (C7 `Pointer.hint`). A miss adds nothing.

### 3. `garden_search`

- Registered only when `garden.socket` is set. Input: `q` and `k` of C2 `GardenSearchInput`, and
  `as_of`, a UTC time as RFC 3339 or `YYYY-MM-DD` (a date stands for its last second), as the
  engine's other time arguments take it. C7 `search` takes it as Unix time. C2 is the input of
  the core's MCP facade (roadmap P1) and keeps its shape until that facade is built; the tool
  then adds `as_of` to C2 in a minor version.
- `k` is clamped to 1 to 50; without it the garden's `garden.api.search_k` applies. `private` is
  true, as for the garden tier of `archive_search` (ADR-0051 section 6): the engine is the
  owner's own runtime. A miss is an ordinary garden miss (no `no_miss`).
- The answer is `success` and `facts`, best first: each with `ref` (the fact id or the record's
  `seed:<seq>`), `fact`, `entity`, `kind`, `domain`, `valid_from`, `valid_to`, `source_ref`,
  `source_kind`, `expires_at`, `proof_count`, the flags `pinned`, `recheck_before_acting`,
  `unprocessed` and `disputed` when true, `conflicts_with` and `score`; times as RFC 3339 UTC.
  Absent fields are left out. A `lexical_only` answer adds a `note`. A refused or failed call is
  the tool's error `the garden was not searched: <why>`.

### 4. `garden_propose`

- Registered only when `garden.socket` is set. Input: C2 `GardenProposeInput` (`fact`, `entity`,
  `source_ref`, each required and not blank).
- The candidate goes to C7 `propose` with `source_kind` `owner` in a top-level owner turn (the
  owner's input on the main channel, `Origin::TopLevelOwner`) and `agent` in any other. The
  owner's "remember this" and corrections are what ADR-0055 section 2 names as the tool's source.
  An event's text or a side channel must not be able to write at the owner's rank. No other field
  is set: no `pin`, which the owner sets through the garden's own clients, and no `private`.
- A `seeded` answer is `success`, `status`, `ref` (the record's `seed:<seq>`, which `forget`
  takes) and a `note` that the record is unprocessed and the night review decides. With
  `conflicts_with` it also says that the record conflicts with that fact until the night's
  review. A `rejected` answer is the tool's error `the garden refused the fact: <reason>`, with
  the reason in words for `secret_value`, `live_data`, `invalid`, `owner_rejected` and
  `ambiguous_entity`. Another status (a garden without the seedbed) is passed on with its
  `fact_id` and `reason`. A failed call is `the fact was not proposed: <why>`.

### 5. Descriptions and order

`archive_search` stays the tiered search over the garden, the history and the archive (ADR-0051
section 6), and its description points to `garden_search` for a fact alone. `garden_search`
says to use it first for a fact about the owner and on a pointer, and `archive_search` for past
conversations and originals. `garden_propose` says it is for the owner's "remember" and
corrections and that the night decides. The two tools follow `archive_read` in the tool list.

## Consequences

- With `garden.socket` and `garden.core_file` set, the model sees the pinned core in every turn,
  pointers in owner turns, and searches and proposes over C7. Without them nothing changes.
- An owner turn with the garden configured makes one extra local call of at most about 150 ms
  before its first model step; a garden that is down costs only a failed connect.
- A changed core profile rewrites the stored prompt's tail once, which re-reads that tail of the
  cached prefix on the next request.
- The deviations from ADR-0047 sections 3a and 3c are the re-read of the core file per turn and
  `as_of` beside C2's input; the rest is ADR-0047 section 3 as written.
