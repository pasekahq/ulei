# ADR-0078: The night gardener `ulei-engine night`: the seedbed, the forget queue and the archive history into the garden

Status: accepted
Date: 2026-11-26
Affects: crates/engine, crates/garden, crates/memory, contract:C7, contract:C9, contract:C10, deploy/systemd, deploy/install.sh, ci/release.sh, ci/install_run.py, ci/units_run.py, .github/workflows/ci.yml

## Context

The garden has a seedbed (ADR-0055) and a forget queue (ADR-0051 section 5), and C7 has
`seedbed_take` and `seedbed_done` for the night stage that sorts them. No process calls them
(issue:509d9dc62473f22ed7d7). A deployment holds no facts and about 10,000 unprocessed seedbed
records, and its archive (C10, ADR-0058) holds the whole history of the previous runtime, which
the owner wants sorted into the garden before the cutover (blocker B1, part 5). ADR-0051 names the
night stage as the one that clears the facts a forget reaches (12:39) and that makes the forest's
compressed form from the archive; ADR-0055 section 5 leaves "the night process" to this issue.

The night needs a model. Only the engine calls models, and only through the qb gateway (ADR-0009,
ADR-0012); the garden holds no client of a model gateway (`crates/garden/AGENTS.md`). The engine
already has the C4G client (`gateway.rs`), a C7 client (`garden.rs`) and a C10 client
(`archive.rs`) over the same HTTP reader.

Neither port map nor reference has a night stage: this is an owner-designed addition, not a
port (as ADR-0016 section 15).

## Decision

### 1. Where it lives: a subcommand of `ulei-engine`

`ulei-engine night [--config PATH] [--dry-run] [--stage a|b|c[,...]] [--limit-sessions N]` runs
once and exits. It is code of `crates/engine` (`src/night.rs` and `src/night/`), beside `run` and
`serve`, and reuses the engine's gateway client and its HTTP reader for C7 and C10.

Reasons: a module crate depends only on `ulei-contracts` (ADR-0001 section 3), so a night stage
in the garden or the archive would need its own C4G client, which the garden's rules forbid and
ADR-0009 gives to the engine alone; a shared client crate would be a new class of crate with an
ADR of its own and a move of the engine's reader. A subcommand adds no binary to the release, the
install script or the budgets: `ulei-engine` is 2.8 MiB of its 4 MiB (ADR-0053), and the night
adds code, not dependencies. The daemon path does not reach the night's code, so the `serve` and
`run` budgets are unchanged.

Rejected:
- `ulei-garden night`: a model client in the garden, against ADR-0009 and the garden's rules.
- A binary of its own in a new crate: a fourth copy of the HTTP-over-UDS reader, or a move of the
  engine's into a shared crate that ADR-0001 does not have.
- A binary of its own in `crates/engine`: a second fat-LTO link of most of the engine, a release
  line and a budget for the same code.

### 2. Stages and order

A run performs the stages `b`, `a`, `c` in this order (`--stage` selects some): the forget queue
first, since it costs nothing and an owner's forget must not wait behind a spend limit, then the
seedbed, then the history.

**Stage b, forget.** C7 0.8.0 adds `forget_take` (read only, the untaken rows by `id` after a
cursor) and `forget_done` (per row, in its own unit of work: a queued fact still current is closed
on the transaction axis with no new version, and the row is marked taken; a row already taken
changes nothing; an unknown id is `not_found` before any row). The night takes the queue in pages
of 100 and hands every page to `forget_done`. No model is called. A fact is closed, never deleted,
so `as_of` before the forget still answers it (ADR-0044); "clears" in ADR-0051 section 5 means this.

**Stage a, the seedbed.** `seedbed_take` in pages, oldest first, with a new optional `after` ref
(C7 0.8.0) so a batch left for the next night does not block the ones behind it within a run.
Records with `secret_ref` or `pin` go to `seedbed_done` without candidates: the edge decides their
own candidate, and no model sees a secret pointer or changes the owner's pin. The rest go in
batches of `night.seed_batch` records (default 50), cut earlier when the estimated prompt passes
`night.max_prompt_tokens`, one model call per batch. The answer names every record of the batch
once with its candidates (empty: the record is dropped as noise or a duplicate). Each candidate
keeps the record's `source_ref`, `source_kind`, `observed_at` and `private`, so its provenance is
the record's evidence (ADR-0055 section 6); the model gives entity, kind, attribute, fact, valid
time, TTL kind and confidence. `seedbed_done` with those candidates lets the edge decide: low
confidence, contradictions and ambiguous entities go to the digest as the edge already does.

**Stage c, history.** For every archive source in `night.sources` (the engine's `dialogue` and any
imported ones, by name in config; no source name is in code) the night walks the entries with a
new C10 `list` (section 4) and groups them into sessions: the session of an `external_id` is the
part before its last `night.separator` (default `/`), and an id without one is the session's own
row, left out. A session is rendered as a plain transcript of the messages whose `who` is in
`night.roles` (default `user,assistant`; tool output and system text are left out), in time order,
each cut at `night.message_chars` (default 3,000) because many user messages of an old runtime
are long injected prompts, numbered `[1]`, `[2]` and so on. One model call per session; a session
over `night.max_prompt_tokens` goes in parts at message boundaries. The answer holds the
candidates, each naming the number of the message that states it, and a short summary of the
session. Each candidate is proposed with `source_kind` `raw`, `source_ref` `archive:<id>` of that
message's item and `observed_at` its `at` (ADR-0055 section 6), and the seeded records are handed
to `seedbed_done` at once without candidates, so the edge decides them the same night and stage a
never sends them to the model again. The summary is a typed field of the extraction answer with
the session's time span from the messages' `at`; it goes to the run report until the forest
(ADR-0051 section 1) stores it.

Every write goes through `propose` and `seedbed_done`, the garden's one write path (ADR-0055
section 2). The prompt says that the garden refuses secret values and that a fact may only say a
secret exists; the garden's refusal screen still refuses a value that looks secret.

### 3. The extraction call

One C4G `turn` per batch, session or part on `night.route` (default `small`, the cheap alias of the
example config), with `night.max_tokens` output tokens (default 8,192), no tools, no cache mode,
within `night.timeout` seconds (default 600). The system prompt fixes a strict JSON answer: an
object with exactly the keys of its stage, candidates with exactly `entity`, `kind`, `fact` and
the optional `attribute`, `valid_from`, `valid_to` (dates `YYYY-MM-DD`), `ttl_kind`, `confidence`
and, in stage c, `message`. A code fence around the object is accepted; anything else (no JSON,
another key, a wrong type, an unknown kind, a ref or message number that is not in the request, a
`max_tokens` stop) is malformed. A malformed answer is asked once more; a second malformed answer
leaves the batch or session as it is: its records stay unprocessed, its session stays unsent, and
the run goes on with the next. No record is marked done without an outcome.

A session malformed in three runs is given up: the cursor passes it and the report names it, so
one bad session cannot hold a source back forever; its items stay in the archive.

A gateway error (`quota_exhausted`, `unavailable`, `overloaded`, a cut stream, a timeout) stops
the run with exit 1; what was done stays done.

The prompt estimate is `ceil(ascii / 3.5 + other / 2)` characters, since Cyrillic text takes
about two characters per token where English takes about four.

### 4. C10 `list` and the archive's entry order

C10 0.5.0 adds `list`: the entries of one source after a cursor `seq`, ascending, at most `limit`
(1 to 500), each with its item id, metadata and the head of its indexed text (`text_chars`, at
most 16,384), never the original bytes; a page ends early before 786,432 bytes of body, and
`more` says whether entries follow. It runs no `sync`.

`seq` is a new table of the archive (schema version 4): one row per `(source, external_id)` with
an `AUTOINCREMENT` key, filled at the upgrade in the order the archive already lists entries
(`put_at`, rowid) and for every later entry in the transaction that stores it. The implicit rowid
of `entries` is no cursor, since `VACUUM` may renumber it, and stored rows are never rewritten
(ADR-0076), so the order lives in a table of its own. A deleted entry leaves a gap; a `seq` is
never given twice.

### 5. The cursor and the report

The night keeps its state in the directory `night.state` (`ULEI_NIGHT_STATE` wins; the unit sets
it to its `StateDirectory`), owner-only:

- `cursor.json`, replaced whole through a temporary file, `fsync` and `rename` after every
  session: per source the `seq` all of whose earlier entries are done, the end of the window being
  walked, the sessions of that window already sent, and the failure count of each failed session.
  A window is at most 1,000 entries read again on a resume, so a stopped run resumes at the next
  unsent session and a session is never sent twice. Between a session's last `propose` and the
  cursor's write a kill sends that session again on the next run; `propose` of an equal candidate
  answers the stored record (ADR-0055 section 2), so the garden gets no second record.
- `runs/<start>-<pid>.jsonl`, one JSON line per call (stage, batch or session, its counts, tokens,
  USD and, for a session, its summary and span) and a last `total` line; written as it goes, so a
  killed run keeps its lines.

Stages a and b keep no cursor: the garden's `processed_at` and `taken_at` are theirs.

The one-line summary on stderr: `ulei-engine night: <stop> taken <n> proposed <n> accepted <n>
digest <n> refused <n> malformed <n> cleared <n> sessions <n> tokens <in>/<out> usd <x> in <s> s`.

### 6. Money

`night.max_usd` (default 1.0) bounds a run. The cost of a call is its last C4G `usage` event
priced by `night.price_input`, `price_output`, `price_cache_read` and `price_cache_write` (USD per
million tokens; defaults 1.0, 5.0, 0.1 and 1.25, to be set to the route's model). A call starts
only while the spend so far plus its estimated input cost stays under the limit, and the run stops
once the spend reaches it, so a run passes the limit by at most one call's output. The stop is
clean: the cursor and the garden hold exactly what was done, and the next run goes on from there.
C4G's `GET /v1/model/spend` answers only the gateway's own uid and counts per login, not per
caller, so the night cannot use it. A call without a `usage` event is priced from its estimates.

`--dry-run` calls no model and writes nothing to the garden, the archive or the state directory: it
walks the same pages read only and prints per stage the records, sessions, messages and calls it
would send, the estimated input tokens and the estimated cost, the output taken as a quarter of the
input and at most `night.max_tokens` per call. `--limit-sessions N` ends stage c after N sessions
or parts.

### 7. Unit and timer

`deploy/systemd/ulei-night.service` is `Type=oneshot` with `ExecStart=%h/.local/bin/ulei-engine
night`, `StateDirectory=ulei/night`, writes only that directory, `RestrictAddressFamilies=AF_UNIX`
(the gateway, the garden and the archive by their sockets), the hardening of its neighbours,
`MemoryMax=256M`, `TasksMax=4` and `TimeoutStartSec=6h`. Without `[Install]`: the timer
`ulei-night.timer` (`OnCalendar=*-*-* 03:30:00`, `RandomizedDelaySec=15min`, `Persistent=true`)
starts it. `deploy/install.sh` installs both and prints the timer's enable command as a step of its
own, after a dry run; `ci/units_run.py` starts the service once on its synthetic config.

### 8. C9 0.9.0

Section `night`: `route`, `state`, `max_usd`, the four prices, `max_prompt_tokens` (1,000 to
100,000, default 60,000), `max_tokens`, `timeout`, `seed_batch` (1 to 100), `sources`,
`separator`, `roles` and `message_chars` (200 to 16,384). It also reads `qb.socket`,
`garden.socket` and `memory.socket`; stage c needs the archive only when `sources` names one.

## Consequences

- The seedbed is emptied night by night within the spend limit, and the imported history is
  sorted session by session over as many nights as its budget needs; the runbook
  `docs/runbooks/night-gardener.md` starts with a dry run.
- A live `dialogue` session is sent as the part written before the night and its continuation on
  a later night; the continuation's transcript starts where the last one ended.
- Session summaries wait in the run reports until the forest (ADR-0051) stores them; the
  extraction answer's type does not change then.
- A queued fact that expired before the night is already closed by the expiry pass, which writes
  a current history copy with a `valid_to` in the past; `forget_done` answers it `closed: false`
  and drops its re-check, and the copy answers only questions about a valid time before the
  expiry.
- `ci/night_run.py` checks the night end to end on the released `ulei-engine`, `ulei-garden` and
  `ulei-memory` with a scripted gateway, in the `release` job.
