# ADR-0079: The forest: session summaries of the night in `ulei-memory`, C10 0.6.0, and its tier in `archive_search`

Status: accepted
Date: 2026-11-26
Affects: crates/memory, crates/engine, contract:C10
Amended by: ADR-0081 (the unit's `MemoryMax` is 256M; `forest_records` counts all records less the superseded ones).

## Context

ADR-0051 decides three layers below the model's context: the archive of raw items (C10,
`ulei-memory`), the garden of typed facts (C7) and between them the forest, "the compressed form,
made by the gardener's night stage" (decision 11:14 to 11:18). The search order is garden, then
forest, then archive, the archive only when garden and forest find nothing; "forget" marks a
forest record as not current (12:39). ADR-0051 section 1 places the forest in `ulei-memory`, in
the daemon and the budget row of the archive, and leaves it unbuilt: `archive_search` uses the
history index as the forest's stand-in (sections 4 and 6).

The night gardener (ADR-0078) now makes the compressed form: stage c sends each session of an
archive source, or each part of a session too long for one call, to the model and gets back the
session's candidates and a typed `summary` of at most 1,200 characters with the time span of its
messages. ADR-0078 writes the summary only to the run report "until the forest stores it". This
is blocker B1 of the cutover, part 6.

What decides the design:

- A session is not sent once. A long session goes in parts at message boundaries (ADR-0078
  section 2); a live `dialogue` session is sent as the part written before the night and its
  continuation on a later night; a cursor reset, or a kill between the last `propose` and the
  cursor's write, sends a part again, and a changed `max_prompt_tokens` may split it otherwise.
- The archive's file is never rewritten: a schema upgrade adds tables only (ADR-0076), and the
  daemon runs within `MemoryMax=256M` (ADR-0081; 32M before) and one connection at a time (ADR-0070).
- The night runs as the owner's uid, as the engine and the daemon do; the socket's peer check is
  by uid (ADR-0051 section 2).
- Neither port map nor reference has a forest: this is an owner-designed addition, not a port
  (ADR-0016 section 15).

## Decision

### 1. A record per sent part, in the archive's file

A forest record is the summary of one unit the night sends: a session, or one part of it. It
holds its `source`, its `session` key (the part of the entries' `external_id` before the last
separator, ADR-0078 section 2), its `text`, its span `from` and `to` (the earliest and latest
`at` of its messages, absent when none has one), its links to the archive entries it compresses
(each entry's `seq` and item `id`), its producer (the night run's id, the report file's name
without `.jsonl`, and the model route) and the time it was stored.

The records live in the archive's SQLite file, in tables that schema version 5 adds: `forest`,
`forest_links`, `forest_superseded`, the FTS5 table `forest_fts` (the archive's tokenizer
`unicode61 remove_diacritics 2`, rowid the record's row, holding its text for `snippet()`) and
`forest_marks`. Triggers refuse every update and delete of a record, its links and its
supersession, and every delete of a mark; a mark's only change is its lift, once. A file of
version 1 to 4 gains the tables at open; no stored row is rewritten.

No day roll-up is made. A day's sessions are each a record with its span, and `forest_search`
filters by span; a roll-up would be a second model call per day over text the forest already
holds and a second record of the same messages that the replace rule of section 2 would have to
relate to its sessions.

Rejected:
- One record per session key, replaced by every later put of that key: a continuation on a later
  night would replace the summary of the session's earlier part, and the parts of a long session
  would replace each other within one run.
- A file of its own for the forest: a second open, a second WAL and a second owner-only check in
  the same daemon, and a link from a record to an entry could not be checked in the put's
  transaction.
- A forest in the garden's store: the garden holds typed facts under its own row, and the
  records link archive entries the garden cannot check.

### 2. Replace, not duplicate

A put supersedes every current record of the same `source` and `session` that links any entry
of the put. The superseded record stays in the file with the record that superseded it and the
time, out of search, as ADR-0044 keeps a closed fact. So a part sent again replaces its record;
a continuation, whose entries are new, stays beside the earlier part; parts of one session never
share an entry and never replace each other; a different split of a session sent again replaces
every record it overlaps. A put equal in text, span and link set to the one current record it
would replace stores nothing and answers that record `duplicate`.

A live forget mark of a superseded record passes to the new record in the put's transaction, so
a rerun never brings a forgotten summary back.

Rejected: dropping the superseded record. The forest keeps what it said and when, as the garden
keeps a closed fact; the rows are small and never searched.

### 3. C10 0.6.0

Additive, the contract rules of ADR-0005:

| Method | Input | Output |
|---|---|---|
| `forest_put` | `source`, `session` (1 to 512 bytes), `text` (1 to 4,000 characters), optional `from` and `to` (together, `from` not after `to`), `links` (1 to 1,000 `{seq, id}`, each `seq` once), `producer` (`run` `<start>-<pid>` or `<start>-<pid>-<n>`, `route`) | `id`, `status` `stored` or `duplicate`, `replaced` |
| `forest_search` | `q` (sanitized as `search`), optional `source`, `since` (the span ends at or after it), `until` (the span starts at or before it), `limit` 1 to 20 (default 10) | `hits`: `id`, `source`, `session`, `text`, `snippet()` marked `>>>` and `<<<`, `from`, `to`, `put_at`, `producer`, `links` (`seq`, `id`, `external_id` of each linked entry still stored) |
| `forest_forget` | `ids` (1 to 100), `reason` (required without `undo`), `provenance` (`s<session>-t<turn>[-n<n>]`), `undo` | one item per id: `id`, the current `record` it resolved to, `marks` written or lifted |

`Health` gains `forest`, the count of current records. Every link of a put must name a stored
entry of the record's source whose item is the link's `id` (`not_found` and `invalid_request`
otherwise), so a record never points at nothing. `forest_search` leaves out superseded and
forgotten records and ends its answer before a hit that would take the body past 786,432 bytes,
the bound of `list` (ADR-0078 section 4); a record's 1,000 links of at most 512-byte external ids
stay under it. A deleted archive item leaves the links of the hits; the record stays.

The producer rule is the archive's: the peer's uid must be the daemon's, checked before a byte is
read. Only the night calls `forest_put`; no engine tool does, and `crates/memory/AGENTS.md` keeps
it so. A writer of another uid is refused `forbidden`.

### 4. The night writes the forest

Stage c stores the summary of each sent unit with `forest_put` after its candidates went to the
garden and before the cursor marks it done: the unit's messages as links, its span, the run's id
and `night.route`. A failed put stops the run as any daemon failure does (exit 1); the unit is
not marked done, and the next run sends it again, which section 2 makes a replace. The run
report's `session` line carries `forest` (the record's id), `forest_status` and, when it replaced
records, `forest_replaced`, instead of the summary. `--dry-run` writes nothing.

### 5. The engine

`archive_search` walks garden, forest, history index, archive. The forest tier is
`forest_search` with the query, the limit, `since`, `until` and `source` (which now narrows the
forest and the archive; `who` and `kind` still narrow the archive only). The history index stays
a tier as it is: it holds the current runtime's own sessions the night has not yet sent. The
archive is asked only when the garden, the forest and the history find nothing. A failed or unset
forest is a `forest_error` line and counts as no records, as the garden tier does (ADR-0051
section 6). A forest hit shows `ref` `forest:<id>`, its source, session, span in RFC 3339 UTC,
its text, the refs `archive:<id>` of the first 20 distinct items it links, which `archive_read`
opens, and the count of its links.

A record of source `dialogue` that links a message forgotten in the journal is left out of the
tier, as an archive hit loses a forgotten dialogue entry (ADR-0051 section 5): the archive takes
every raw row whatever its marks, so the night may summarize a forgotten message, and the filter
at read keeps the summary out of retrieval. Forest hits are not lifted into the seedbed: the
records were made from the archive and their facts went to the garden in the same night.

No separate `forest_search` tool is added: the owner's order of the tiers stays in one tool
(ADR-0051 section 6 rejects three tools left to the model's order), and the tool list does not
grow.

`forget` takes `forest:<id>` refs beside `c8:` and `seed:` refs. The forest refs go first to
`forest_forget` with the reason or `undo` and the provenance `s<session>-t<turn>`; a failure
or an unset `memory.socket` refuses the call before anything is marked, since the forest's marks
live only there. Then the garden and the journal follow as before; when the garden then refuses
the seedbed refs, the refusal says the forest refs were marked before it. `query` lists up to 20
forest records after the history and seedbed candidates, or a `forest` line saying why it could
not. The garden's C7 `forget` never sees a forest ref.

### 6. Budgets

The forest adds no dependency and no process to `ulei-memory`; it keeps its row of
`ci/budgets.sh` (ADR-0051 section 7). The engine adds code to its existing tools and no
dependency.

## Consequences

- After a night, `forest_search` finds the summaries of the sessions it sent, and
  `archive_search` answers from them before it asks the archive; the runbooks
  `docs/runbooks/memory-archive.md` and `docs/runbooks/night-gardener.md` say how to check it.
- A session summarized again replaces its record; its earlier text stays in the file as
  superseded.
- The run reports carry the record ids instead of the summaries; reports of runs before this
  change keep their summaries and are not imported into the forest.
- The history index stays the tier of the current runtime's sessions until the night has sent
  them; a session found in both shows its record and its messages.
