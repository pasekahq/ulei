# ADR-0080: The places bed: typed places with per-attribute freshness in the garden, and C7 0.9.0 `places_search`

Status: accepted
Date: 2026-11-26
Affects: crates/garden, crates/engine, contract:C7

## Context

The owner wants a standing list of places to eat, kept fresh, as a bed of the garden. Garden
facts answer free text (C7 `search`); a question such as "noodles within 2 km of home, open now,
rated at least the owner's threshold" is a filter over typed values that go stale at different
speeds: hours in days, a rating in weeks, a cuisine in a year, the owner's verdict never. A
starting file exists that keeps every field of every place with the source and the date it was
last checked.

What binds the design: ulei never calls a model vendor, and the bed works without a model and
without network in CI; the external source that rechecks a value sits behind an interface with a
fake in tests; a module depends only on `ulei-contracts`; no budget is raised. The garden links no
TLS stack and no model client (`crates/garden/AGENTS.md`).

This record decides the design and defines the wire of `places_search` (C7 0.9.0). The store
migration, the importer, retrieval, grooming and the CLI and engine surface are later cards that
implement it. The reference agent has no places store: this is an owner-designed addition, not a
port (as ADR-0078 section 1).

## Decision

### 1. Where the bed lives: inside `crates/garden`

The bed is code of `crates/garden` (a `places` module) and tables of the garden's store, added by
one new entry at the end of `MIGRATIONS` (ADR-0044). The daemon `ulei-garden serve` serves it over
C7 on its socket; its worker thread grooms it.

Reasons: the daemon is already the only process that opens the store, with the socket, the peer
check, a worker that sleeps until its nearest deadline, the edge's name rule (`edge::normalise`,
`translit::key`), the stemmer `morph` and the secret screen; `model` already has
`EntityKind::Place`, `PlaceAttrs` and `VisitAttrs`. A new module would be a new crate, daemon,
unit, release line and budget rows for about a thousand rows (ADR-0001), and a place known both to
facts and to the bed would cross a wire.

Relation to the rest of the garden:

- Entities. Every bed row is a row of `entities` of the kind its type maps to (type `place`, kind
  `place`). A bed write by name finds the one place of the bed whose name or alias is equal
  after the store's normalisation, and refuses the name of two or more as `ambiguous_place`, so
  a repeated write changes nothing; without one it names the place with `Edge::name`, called
  read only: one place entity it finds without a bed row is adopted, otherwise a new entity is
  written in the same unit of work. `entities.attrs` keeps `PlaceAttrs` (`osm_id`, `lat_lon`) in step with the bed. So a visit
  or a preference fact names the same id as the bed, and the id is `PlaceHit.id`.
- Facts. Bed attributes and verdicts are not facts: they enter no `facts`, `fact_fts`,
  `fact_vectors`, seedbed, pointer, `search` answer, core profile or `garden_miss`;
  `places_search` is their only reader. A recheck is a new observation of a typed value with a TTL
  of its key; as facts, every recheck would be a new version for the night's review, the facts'
  `TtlTable` (`opening_hours` 30 days, `place` 180 days) would not hold the bed's TTLs, and a
  thousand places of a dozen attributes would crowd the pointers of the owner's own facts.
- Time axes. A value has one time, `checked_at`, when it was last checked against its source. A
  replaced value is moved to `bed_attr_history`, never deleted, as the garden deletes nothing.
  `places_search` has no `as_of`: it answers about the world now, and the history serves the
  rating's trend and the audit of a value.
- Sensitivity. The bed is private as a whole: verdicts and the profile are the owner's. The socket
  answers only the daemon's uid, and the engine, the one caller that reaches a model, is the
  owner's own runtime (ADR-0077 section 3). No bed value is secret: the write refuses a value of
  the free forms `list`, `hours`, `url` and `text`, a verdict's note, a profile value of those
  forms, a place name, `source` and `source_ref` that `secret::may_carry_credential` flags; nothing
  of the bed reaches the core profile or a C1 event. That screen is the bed's one rule: it flags,
  outside URLs, a payment card number not written after `+` and an IBAN; a national id after
  its key, a value after a secret key (`password:`, `token=`, `PIN 4821`; after a bare `code` or
  `код` only a value with a digit, so `Dress code: smart casual` passes), a private key block,
  and a word that is
  a well-known API key prefix, a random token of 32 characters or more, or a run of 16 or more
  letters and digits mixing both; in a URL it checks only the pieces between `/`, `?`, `&`, `#`,
  `=`, `;`, `!` and `:`, for a key prefix or a random token. So a map link with a query string,
  a short id or a long numeric one, an address with a postcode, a phone number and non-ASCII text pass, and a key in a
  value, a URL's query included, is refused. The closed forms (`number`, `count`, `range`,
  `status`, `point`, `osm_id`) are not screened: their grammar admits only numbers, signs and
  fixed words.

### 2. Schema

The bed's tables are general: a type (`place` now; doctors or shops later) is a set of
`attr_policy` rows and a mapping to an `EntityKind` in code, and reuses every table. The names of
the request map onto them: `place` is `bed_entity` of type `place` with its `entities` row (`id`,
`name`, `name_norm`), `place_attr` is `bed_attr`, `place_attr_history` is `bed_attr_history`,
`place_verdict` is `bed_verdict`, `food_profile` is `bed_profile` of type `place`.

```sql
CREATE TABLE attr_policy (
    type TEXT NOT NULL,
    key TEXT NOT NULL,
    form TEXT NOT NULL CHECK (form IN ('number', 'count', 'range', 'list', 'hours', 'status',
        'point', 'osm_id', 'url', 'text', 'verdict')),
    ttl_days INTEGER CHECK (ttl_days IS NULL OR ttl_days > 0),
    on_stale TEXT NOT NULL CHECK (on_stale IN ('demote', 'distrust', 'show')),
    PRIMARY KEY (type, key)
) STRICT;
CREATE TABLE bed_entity (
    entity TEXT PRIMARY KEY REFERENCES entities(id),
    type TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('candidate', 'active', 'gone')),
    lat REAL,
    lon REAL,
    source TEXT NOT NULL CHECK (source <> ''),
    created_at INTEGER NOT NULL,
    status_at INTEGER NOT NULL,
    CHECK ((lat IS NULL) = (lon IS NULL))
) STRICT;
CREATE INDEX bed_entity_geo ON bed_entity(type, lat, lon) WHERE lat IS NOT NULL;
CREATE TABLE bed_attr (
    entity TEXT NOT NULL REFERENCES bed_entity(entity),
    type TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL CHECK (value <> ''),
    num REAL,
    checked_at INTEGER NOT NULL,
    source TEXT NOT NULL CHECK (source <> ''),
    confidence REAL NOT NULL DEFAULT 1.0 CHECK (confidence BETWEEN 0.0 AND 1.0),
    due_at INTEGER,
    PRIMARY KEY (entity, key),
    FOREIGN KEY (type, key) REFERENCES attr_policy(type, key)
) STRICT;
CREATE INDEX bed_attr_due ON bed_attr(due_at) WHERE due_at IS NOT NULL;
CREATE TABLE bed_attr_history (
    seq INTEGER PRIMARY KEY,
    entity TEXT NOT NULL REFERENCES bed_entity(entity),
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    checked_at INTEGER NOT NULL,
    source TEXT NOT NULL,
    confidence REAL NOT NULL,
    replaced_at INTEGER NOT NULL
) STRICT;
CREATE INDEX bed_attr_history_key ON bed_attr_history(entity, key, checked_at);
CREATE TABLE bed_verdict (
    seq INTEGER PRIMARY KEY,
    entity TEXT NOT NULL REFERENCES bed_entity(entity),
    verdict INTEGER NOT NULL CHECK (verdict IN (-1, 0, 1)),
    note TEXT,
    said_at INTEGER NOT NULL,
    source_ref TEXT NOT NULL CHECK (source_ref <> ''),
    better_than TEXT REFERENCES bed_entity(entity),
    written_at INTEGER NOT NULL,
    UNIQUE (entity, source_ref)
) STRICT;
CREATE INDEX bed_verdict_latest ON bed_verdict(entity, said_at, seq);
CREATE TABLE bed_profile (
    type TEXT NOT NULL,
    key TEXT NOT NULL CHECK (key IN ('home', 'min_rating', 'budget_for_two', 'currency', 'ban')),
    value TEXT NOT NULL CHECK (value <> ''),
    source_ref TEXT NOT NULL CHECK (source_ref <> ''),
    set_at INTEGER NOT NULL,
    PRIMARY KEY (type, key, value)
) STRICT;
CREATE UNIQUE INDEX bed_profile_one ON bed_profile(type, key) WHERE key <> 'ban';
CREATE TABLE bed_recheck (
    entity TEXT NOT NULL REFERENCES bed_entity(entity),
    key TEXT NOT NULL,
    queued_at INTEGER NOT NULL,
    tries INTEGER NOT NULL DEFAULT 0,
    next_attempt INTEGER NOT NULL,
    PRIMARY KEY (entity, key)
) STRICT;
CREATE INDEX bed_recheck_next ON bed_recheck(next_attempt);
CREATE TABLE bed_dup_pair (
    a TEXT NOT NULL REFERENCES bed_entity(entity),
    b TEXT NOT NULL REFERENCES bed_entity(entity),
    distance_m INTEGER,
    found_at INTEGER NOT NULL,
    settled_at INTEGER,
    PRIMARY KEY (a, b),
    CHECK (a < b)
) STRICT;
```

- `bed_attr` holds the current value of a key, one row per place and key, each with its own
  `checked_at`, `source` and `confidence`. `num` is the value's number for the forms `number` and
  `count` and the low end for `range`, so SQL filters on it; `due_at` is `checked_at` plus
  `ttl_days` days, absent for a key without a TTL, so the stale queue is one indexed range.
- A write of a key moves the row it replaces into `bed_attr_history` with `replaced_at`. An
  observation older than the current `checked_at` goes into the history only.
- A write of a place already in the bed never demotes it: `candidate` leaves its status as it
  is, `active` sets `active`, and only `gone`, from a write or from grooming (section 6), sets
  `gone`; a change sets `status_at`. `source` stays the one of the write that created the row.
- `lat` and `lon` are the indexed copy of the `location` attribute, written in the same unit of
  work; the attribute keeps their `checked_at` and `source`.
- `bed_verdict` is never updated or deleted and has no TTL; the latest by `said_at`, then `seq`,
  is the place's verdict. `better_than` is kept and answered; ranking does not read it, since a
  few pairs give no order over the rest.
- `bed_profile` keeps one row per key, apart from `ban`, which may have many; a write of a single
  key replaces its row. Its values take the forms below: `home` a point, `min_rating` a number,
  `budget_for_two` a range, `currency` and `ban` text.
- No `place_vec`: the vibe term is lexical (section 5). The garden's embedder runs in a helper
  that may be down (ADR-0047 section 1), so a term that exists only while it is up would make the
  order depend on the helper's state, and embedding a query costs a large part of the 100 ms of
  section 5 on its own. A later record may add a vector table if a gold set shows lexical vibe
  misses.

Value forms: `number` a decimal (`4.5`); `count` an integer from 0; `range` `A` or `A-B` in the
profile's currency; `list` terms separated by commas, each compared after `edge::normalise`;
`hours` a subset of the OpenStreetMap `opening_hours` syntax: `24/7`, or rules separated by `;`,
each days (`Mo` to `Su`, ranges `Mo-Fr`, lists `Mo,We`) then intervals `HH:MM-HH:MM` separated by
commas or `off`, an interval that ends past midnight belonging to the day it starts, a later rule
overriding an earlier one for the days it names; anything else is stored as written and reads as
`unknown`; `status` one of `open`, `temporarily_closed`, `permanently_closed`; `point` `lat, lon`
in degrees; `osm_id` `node/<n>`, `way/<n>` or `relation/<n>`; `url` an `https://` URL; `text`
free text. A value that does not fit its form is refused.

### 3. TTL table of type `place`

| key | form | `ttl_days` | `on_stale` |
|---|---|---|---|
| `rating` | number | 30 | demote |
| `review_count` | count | 30 | demote |
| `price_for_two` | range | 30 | demote |
| `hours` | hours | 7 | distrust |
| `operating_status` | status | 14 | demote |
| `cuisine` | list | 365 | demote |
| `location` | point | 365 | show |
| `address` | text | 365 | show |
| `osm_id` | osm_id | never | show |
| `maps_url` | url | never | show |
| `vibe` | list | never | show |
| `warnings` | list | never | show |
| `notes` | text | never | show |
| `verdict` | verdict | never | show |

`on_stale`: `demote` keeps the stale value as the place's value for the filters and marks and
demotes the place (section 5); `distrust` (only `hours`) makes `open_now` not trust the value, and
marks and demotes the place only in a search with `open_now`; `show` only answers `stale` and
`age_days` for the attribute. The `verdict` row has no `bed_attr` rows: it routes the key to
`bed_verdict`, which never goes stale.

`operating_status` is 14 days, the upper end of the range: a permanent closure also arrives
through grooming as a closing date that sets `gone` (section 6), and hours, rechecked every 7 days,
already catch a temporary closure; 14 days halves the recheck calls to a free source with a fair
use limit, and a stale status only demotes and marks a place, never hides it.

### 4. Import

`ulei-garden places import FILE [--dry-run] [--config PATH]` takes the starting file's path. It
checks the whole file first: every line by the format below, then the writes of the whole file
rehearsed through the same C7 methods on a scratch in-memory garden, so the bed's own rules
(keys, value forms, the secret screen) check every value. When any line is refused it writes
nothing and prints one JSON error listing every refused line with its number and rule, in one
run, with and without `--dry-run`, and exits 1. `--dry-run` otherwise prints the counts of the
file (places by status, attribute observations, unconfirmed values, verdicts, profile rows)
without connecting. Otherwise it reads `garden.socket` from `ulei.toml`, sends the places in C7
`places_write` batches (section 7), one unit of work per batch, each place's verdicts through
`places_verdict` after its batch and the profile rows through `places_profile`, and prints one
JSON report: places created, adopted and found, attributes written, replaced, history and
unchanged, unconfirmed values, verdicts written and repeated, profile rows written and
unchanged, and the lines the daemon refused, with exit 1 when there is one. A second import of
the same file changes nothing.

Accepted format, Markdown in UTF-8:

- An `as_of: YYYY-MM-DD` line before the first section, the date of the file.
- Sections `## Profile`, `## Active`, `## Candidates`, `## Gone`, matched without case; a place's
  status is its section. Each place is a `### <name>` heading; a leading `<n>. ` is dropped.
- Every field is one line `- <key>: <value> [<source>, <date>]`, the key one of section 3 (and in
  `## Profile` one of `bed_profile`), the source everything before the bracket's last comma. The
  source of a verdict is its `source_ref`, and its value is `1`, `+1`, `0` or `-1` with an
  optional note after it. The heading's name takes the source `import:<file name>` and the date
  `as_of`.
- A date is `YYYY-MM-DD`; a partial date is accepted: `DD.MM` takes the latest year that puts it
  on or before `as_of`, and `YYYY-MM` or `MM.YYYY` takes the first day of the month. A date is
  read as 00:00 UTC. The earliest reading is taken so a value never looks fresher than it was. A
  date after `as_of` is refused.
- The value `n/a`, the file's mark of an unconfirmed value, writes no row: the key stays unknown
  and is counted as unconfirmed. Any other line without the bracket, or with a source or a date
  missing, is refused, so every stored value has a non-empty `checked_at` and `source`.
- A key given twice in one place keeps the line with the later date as the value and the other in
  the history; two lines of one key and one date are refused, as are two verdicts of one
  `source_ref` in a place, a second line of a profile key other than `ban`, a repeated `ban`
  value and a second heading of one name.
- A `# ` title is allowed before the first section. Any other line that is no heading, no
  `as_of` line and no field is refused, as is a field outside a place heading, a place heading
  outside `## Active`, `## Candidates` and `## Gone`, a section of another name and a place with
  more than 50 fields.

A synthetic example:

```markdown
# Places of Fennwick
as_of: 2031-03-20

## Profile
- home: 0.5000, -25.0000 [owner, 2031-02-01]
- min_rating: 4.2 [owner, 2031-02-01]
- budget_for_two: 40 [owner, 2031-02-01]
- currency: EUR [owner, 2031-02-01]
- ban: celery [owner, 2031-02-01]

## Active
### 1. Lantern Noodle Bar
- osm_id: node/9000001 [fixture-osm, 2031-03-02]
- location: 0.5041, -25.0032 [fixture-osm, 2031-03-02]
- cuisine: noodles, dumplings [fixture-listing, 2031-02-08]
- rating: 4.5 [fixture-maps, 2031-03-08]
- review_count: 412 [fixture-maps, 2031-03-08]
- price_for_two: 30-40 [estimate, 2031-03-08]
- hours: Mo-Sa 12:00-22:00; Su off [fixture-osm, 17.03]
- vibe: quiet, courtyard [owner, 2031-02-08]
- verdict: +1 broth was excellent [chat:msg-9001, 2031-02-28]

## Candidates
### Tidepool Kitchen
- address: Harbour Row 3 [fixture-delivery-app, 2031-03-18]
- cuisine: noodles [fixture-delivery-app, 03.2031]
- rating: n/a
```

### 5. Retrieval: `places_search(filters, near, open_now, limit)`

C7 0.9.0 defines it (`PlacesSearchRequest`, `PlacesSearchResponse`): an operation of C7, since it
is served by the garden daemon on its socket like the rest of C7.

1. The point is `near`, else the profile's `home`; `min_rating` and `max_price_for_two` default to
   the profile's `min_rating` and the upper end of its `budget_for_two`; `status` defaults to
   `active` and `candidate`.
   `open_now` without `local_time`, or `max_distance_m` without a point, is `invalid_request`.
2. One SQL query over `bed_entity` of type `place` with its `bed_attr` rows: the status, the
   bounding box of `max_distance_m` around the point (latitude by metres over 111 195, longitude
   further divided by the cosine of the box's latitude farthest from the equator, and unbounded
   where the box reaches a pole or crosses the antimeridian), `num` of `rating` and of
   `price_for_two` against their thresholds, and no latest verdict of -1. A threshold the request
   gives drops a place without the attribute; a threshold taken from the profile drops only a
   place whose value misses it, so a candidate not yet rated or priced is still found. A distance
   or cuisine filter drops a place without a location or a `cuisine`. A stale value still counts
   as its value.
3. In Rust over the rows the query returns: the haversine distance on a sphere of 6 371 008.8 m,
   dropping a place past `max_distance_m`; `cuisine` against the request's terms, a place kept
   when a term of its `cuisine` names one of them; a place whose `cuisine` names a profile `ban`
   is dropped, and one whose `warnings` name one gets the mark `ban_risk`. A list term names a
   phrase when the phrase's words (runs of letters and digits after `edge::normalise`) appear in
   it in order, so `rice noodles` names `Noodles`. These run in Rust because SQLite's trigonometric
   functions depend on a build flag and the normalisation is the crate's own code.
4. `open_now`: `hours` and `operating_status` are read at `local_time`. Fresh `hours` that say
   closed, or a fresh `temporarily_closed`, drop the place; fresh `hours` that say open answer
   `open`; stale `hours` that say open answer `probably_open`; missing, unread or stale closed
   `hours` answer `unknown`. Open now is never stored.
5. Order, best first, by this key compared term by term:
   1. verdict: 1 above 0 and none;
   2. vibe: the number of distinct `morph` stems of the request's `vibe` words found among the
      stems of the place's `vibe`, `cuisine` and verdict note;
   3. quality: `floor(rating * ln(2 + review_count) * 0.5^s)`, a missing rating counting 0 and a
      missing count 0, where `s` is the number of stale `demote` attributes plus 1 in a search with
      `open_now` when `open` is not `open`;
   4. distance, nearest first, a place without one last;
   5. the id, so the order is total.
   The order is lexicographic because the owner set a strict precedence; a weighted sum would need
   weights tuned on a gold set the bed does not have. The quality is floored so that near ties go
   to distance.
6. The answer has at most `limit` places, each with every attribute, its `age_days` and `stale`,
   the latest verdict, `open` in a search with `open_now`, and marks: `stale` with `key` and
   `age_days` for each attribute counted in `s`, `ban_risk`, and `possible_duplicate` for an
   unsettled `bed_dup_pair`. `maps_url` is on every place: the `maps_url` attribute, else
   `https://www.google.com/maps/search/?api=1&query=<lat>%2C<lon>` from the location, else the
   same URL with the name and the `address` as the query.

The check "under 100 ms on 1000 places without network and without a model" is a budget of
`ci/budgets.sh`, `ULEI_GARDEN_MAX_PLACES_SEARCH_P95_US`, default 100000: `ci/garden_run.py` fills
the release musl daemon with 1000 synthetic places and measures 100 `places_search` calls that mix
the filters, `open_now` and `vibe`, from connect to last byte, without the embedder helper. A
budget is part of the contract (ADR-0001) and runs on the release binary, while a timing in a
debug-built unit test measures the build profile and the runner's noise. The ordering, the marks
and the filters are tests of `crates/garden` on a synthetic fixture.

### 6. Grooming without a model

The daemon's worker runs it in its recheck pass (`garden.schedule.recheck_interval`, ADR-0047
section 6); no timer unit and no CLI step. Its nearest deadline adds only the earliest `due_at` of
`bed_attr` and the earliest `next_attempt` of `bed_recheck`, and never earlier than the next
recheck pass, so an idle bed wakes nothing and a busy one at most once per pass. With the recheck
source `none` the bed adds no deadline: staleness is computed at search time.

- Stale queue: a pass writes a `bed_recheck` row for every attribute with `due_at` before now
  (`checked_at + ttl < now`), takes the places of the oldest rows up to
  `garden.schedule.recheck_batch` that have an `osm_id`, and hands them to the source. A place
  without one stays queued; its stale attributes keep demoting it.
- Source: a trait in the crate, `places::Recheck`, the places' ids and `osm_id`s and the queued
  keys in, observations out (a key, a value, its `checked_at`, `source` and `confidence`, or a
  closing date), its failure a `second::SourceError` by class. Tests and CI use an in-process fake
  only. An OpenStreetMap Overpass adapter by `osm_id` is allowed: it reads the tags
  `opening_hours`, `cuisine`, `check_date` and `check_date:<key>` (the observation's
  `checked_at`), `end_date` and lifecycle prefixes such as `disused:amenity`. It needs HTTPS,
  which the garden does not link, so it is a provider over the wire (as ADR-0047 section 9), and
  the card that builds it writes the record for its process and dependency.
- An observation is a bed write: an equal value moves only `checked_at` (the old row into the
  history), a new value replaces the row, and the `bed_recheck` row is cleared. A failed call
  raises `tries` and sets `next_attempt` from 1 hour doubling up to 7 days, with one stderr line
  `garden: places recheck: <class>`.
- A closing date before now (`end_date`, a lifecycle prefix in place of `amenity`, a deleted
  object) writes `operating_status` `permanently_closed` and sets the place `gone` with
  `status_at`. A gone place keeps its attributes and verdicts, so it is not suggested again.
- Grooming never writes `bed_verdict` or `bed_profile`.
- Dedup runs at every write of a new place (import, candidate): an existing place of the type with
  the same `osm_id`, or within 75 m with a similar name (equal `name_norm`, else equal
  `translit::key`, else a key one edit away when both have five characters or more, the edge's
  rule), is the same place, and the write merges into it as observations. A place within 75 m with
  another name, or of a similar name where either side has no location, is written as a new place
  with a `bed_dup_pair` row, answered as `possible_duplicate` until settled. The garden settles no
  pair by a model; settling is the owner's, through a later surface.

### 7. Surface for the secretary

- Wire: C7 methods of the daemon. `places_search` is C7 0.9.0. The writes are C7 0.10.0:
  `places_write` takes a batch of 1 to 100 places, each named by `id` or `name`, with its
  `status`, `source` and its attributes' observations (`key`, `value`, `checked_at`, `source`,
  optional `confidence`), and answers per place `created`, `adopted`, `found` or `refused` and
  per attribute `written`, `replaced`, `history`, `unchanged` or `refused`, a refusal with its
  rule. The batch is one unit of work: a store failure writes nothing of it, while a refused
  place or attribute writes nothing of its own and leaves the rest of the batch written, and a
  refused place's attributes are not written. Places are written in the request's order, so a
  later one may find an earlier one. `places_verdict` appends one verdict (`place`, `verdict`
  -1, 0 or 1, optional `note`, `said_at`, `source_ref`, optional `better_than`) and answers
  `written` or, for a `source_ref` the place already has, `repeated`, with the row's `seq`.
  `places_profile` (C7 0.11.0) writes one profile row (`key`, `value`, `source_ref`, `set_at`)
  and answers `written` or, for the row as stored, `unchanged`; a refused one is
  `invalid_request`.
- CLI: `ulei-garden places search|add-candidate|verdict|import`, subcommands of the garden binary
  and clients of the daemon's socket `garden.socket`, JSON on stdout (`search` prints the
  `PlacesSearchResponse`). `add-candidate` writes `status=candidate` and needs a `source`;
  `verdict` writes into `bed_verdict` and needs a `source_ref`, the message that carries it.
- Engine: tools `places_search`, `places_add_candidate` and `places_verdict`, registered with
  `garden.socket` set, as `garden_search` is (ADR-0077), their inputs added to C2 by their card.
  Each search of food places by the secretary writes what it finds as candidates with their
  source, and the owner's verdict in a chat goes into `places_verdict` with the message as
  `source_ref`.

Reasons: the behaviour lives once, in the daemon, behind C7, and both surfaces are thin clients.
The engine reaches the garden natively over C7 (ADR-0077); an MCP server of the garden was
rejected (ADR-0047 section 1, option B: a process per turn), and the core's C2 facade is not
built. The CLI serves the import, the owner and the tests without the engine.

### 8. Dependencies, crates and contracts

No new crate, dependency or pin. `Affects:` names `crates/garden` (the bed), `crates/engine`
(its tools) and `contract:C7` (0.9.0, 0.10.0 and 0.11.0, additive). The later cards add, each in a minor
version with its changelog entry: the C2 inputs of the engine tools and the C9 key of the recheck
source (`none` only until an adapter exists). The Overpass adapter's process and TLS dependency
need their own record.

## Consequences

- The bed answers typed questions over places from one SQL query and a short pass in Rust,
  without a model and without network, in the daemon's existing process, store and budgets plus
  one new budget row.
- Every bed place is a garden entity, so candidates from the secretary's searches enlarge the set
  the edge resolves place names in; two places of one name are an `ambiguous_entity` for the
  owner, as for any two place entities.
- Stale values never hide a place: they demote and mark it, and the secretary says how old the
  value is.
- Until a recheck adapter exists, values only age: the bed keeps answering with marks, and the
  owner or the secretary refreshes them by new observations.
- A new bed type is a set of `attr_policy` rows and a type to kind mapping, with no new table.
