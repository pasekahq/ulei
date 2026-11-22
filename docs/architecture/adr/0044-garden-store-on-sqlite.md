# ADR-0044: The garden store on SQLite with FTS5

Status: accepted
Date: 2026-11-20
Affects: crates/garden, dep:rusqlite, dep:serde_json

## Context

ADR-0043 gave the garden its vocabulary: entities, domains, facts on a valid-time and a
transaction-time axis with provenance, expiry and sensitivity. The facts need a durable store
that answers "what holds now", "what held then" and "what did the garden believe then", feeds
lexical and dense retrieval, and never lets a secret value into an index. The workspace already
uses bundled SQLite through `rusqlite` (ADR-0007), compiled without FTS5 to save binary size.

## Decision

1. `ulei_garden::store::Store` keeps the garden in one SQLite database through the workspace
   `rusqlite` (bundled). File databases run in WAL mode with `synchronous=NORMAL`; every
   connection has `foreign_keys=ON`. The schema is versioned by `PRAGMA user_version` and
   migrated forward on open, all pending steps in one transaction, so a failed migration leaves
   the store as it was; a newer schema is refused. Every write
   is one transaction, or a savepoint inside the unit of work of `Store::atomically`, which
   commits the writes its closure makes when the closure succeeds and rolls them back when it
   fails; units nest. Tables: `domains`, `entities`, `entity_aliases`, `facts`,
   `fact_vectors`, `rechecks`, the FTS5 table `fact_fts`, `edge_proposal`, the edge's
   queue (ADR-0046), and `seedbed`, the day's unprocessed records (ADR-0055); the migrations
   own every table of the crate.
2. Rows of `facts` are append-only; the only in-place change is closing a version: setting
   `superseded_at` and, when a new version replaces it, `superseded_by` (Graphiti's
   `invalidated_at` and `superseded_by`). A fact may name the single-valued attribute it sets
   (`Fact::attribute`: `address`, `birthday`, `phone`). An open-ended version that starts
   before a current version of the same entity and attribute (old material read late) is
   stored with `valid_to` at the earliest such start: a backfill never replaces a later valid
   version, so "current address = the last valid one" holds whatever order facts arrive in.
   Then the version supersedes, at its `recorded_at`, every current version of the same
   entity and attribute whose validity overlaps it, and the parts of their validity outside
   its interval are written back as closed copies (`<old id>~<recorded_at>`, recorded at the
   new `recorded_at`, vectors copied). A bounded version wins for its interval. Facts without
   an attribute only add. A new version older on the transaction axis than the one it
   replaces is refused.
3. Fact lifetimes are a typed table (`TtlTable`) keyed by `TtlKind`, overridable per kind:
   weather 3 h; FX rates, pharmacies on duty, news 1 day; prices, availability, timetables 7
   days; opening hours and contacts 30 days; product capabilities 60 days (the research says
   30 to 90); laws, visas, taxes, procedures 90 days and re-check before acting; existence of
   a place, address of an institution 180 days; general and personal knowledge never expire.
   `never_cache` ("open now", live arrivals) is refused by the store. A fact without
   `expires_at` gets it from the table at insert.
4. Expiry does not delete. `Store::expired(now)` lists current facts recorded and past
   expiry at `now`;
   `Store::mark_expired(now)` supersedes each at `now`, writes a closed copy with `valid_to`
   at the old expiry and no expiry of its own (so "what was the price last week" stays
   answerable) and queues the expired version in `rechecks` until it is cleared.
5. The invariants of the model are public checks the edge reuses before an insert:
   `Entity::validate` (id, name, attributes fit the kind, preference weight in [0, 1], visit
   rating 1..=5, finite coordinates with latitude in [-90, 90] and longitude in [-180, 180]),
   `Fact::validate` (ids, text, provenance ref, non-empty quote, `proof_count >= 1`,
   `confidence` in [0, 1], `valid_to > valid_from`, `superseded_at >= recorded_at`) and
   `Domain::validate` (slug, label); a QID has at most ten digits. The store adds: known
   entity and domain, no `superseded_at` on a new version, a never-cached kind refused, and an
   expiry after both the observation and the recording (a fact already stale when it arrives
   is refused, not stored invisible). A secret fact needs a non-empty `vault_ref`, keeps no
   quote, its text may not look like a value (anything the edge's detectors find, a run of
   four or more digits written in digits or in words, or a run of 16+ letters and digits
   mixing both) and its provenance ref may not carry a token (a 16+ run mixing letters and
   digits; message ids like `chat:-100123/msg:42` pass). The rule and the edge run the same
   detectors (`ulei_garden::secret`). The heuristic is a guard, not a classifier: prose
   such as "the password is hunter" passes it.
6. Secret facts are stored as written but never enter `fact_fts` or `fact_vectors`
   (`put_vector` refuses them, `missing_vectors` skips them). They are reached only through
   their entity: `find_entities(name or alias)` then `facts_of`, which returns every fact and
   leaves visibility to the caller. `lexical` and `vectors` return private facts only when
   asked and secrets never.
7. Lexical search is a contentless FTS5 table (`contentless_delete=1`, rowid = fact row) over
   four columns: the fact text, the entity's name with aliases, the Snowball Russian stems of
   the Russian tokens of both (`morph`) and the transliteration of both (`translit::key`),
   tokenizer `unicode61 remove_diacritics 2`, prefix indexes 2, 3, 4, ranked by `bm25` with
   the column weights 1.0, 1.0, 0.75 and 0.25 (score = -bm25, higher is better), filtered by
   both time axes and sensitivity. A query becomes its distinct tokens (`translit::tokens`),
   capped at the first 64 (a 20,000-word query took seconds), as quoted prefix terms joined by
   `OR`, as in the retrieval prototype: as written against text and names, the stems of the
   Russian ones against the stems, their transliterations against the transliteration. Every
   version is indexed, so `as_of` searches work; renaming an entity re-indexes its facts.
8. Bundled SQLite is compiled with FTS5 again: `-USQLITE_ENABLE_FTS5` leaves
   `LIBSQLITE3_FLAGS` in `.cargo/config.toml`; FTS3/4 and the other modules of ADR-0007 stay
   off. Measured on the bundled `sqlite3.c` at `-Os`, FTS5 adds 140,447 B of code and data to
   each binary that links SQLite (`ulei-inbox`, `ulei-engine`, and `ulei-web` if it reaches
   SQLite). No size budget changes; the `budgets` job measures them.
9. Vectors are stored per (fact, model) as little-endian f32 blobs with their dimension; one
   model has one dimension. The text to embed is `"<entity name>: <fact text>"`.
10. Provenance and trust (research items 3 and 4: Hindsight, holographic): a provenance keeps
    an optional exact `quote` and a `proof_count`; a fact keeps the source's `confidence` in
    [0, 1], which the edge compares with its doubt threshold. The `facts` table also has
    `trust_score` (default 0.5), `retrieval_count` and `helpful_count` (default 0), as in the
    holographic memory store; nothing writes them yet.
11. The 20 starting domains are data (`src/starting_domains.json`), each bound to a Wikidata
    class checked on wikidata.org; `seed_domains` inserts the missing ones and keeps edits.
    Entity attributes are stored as a small JSON object through `serde_json`.

## Deviations from the retrieval prototype

- Snowball stems in place of the prototype's pymorphy3 lemmas, which have no Rust equivalent
  here (ADR-0047 section 8): forms a stemmer cannot join stay unmatched.
- Stems only of Russian tokens, where the prototype's lemma column repeats every other token,
  so an English word does not count in two columns; the store folds `ё` to `е` itself, which
  `unicode61` does not.
- Four columns (text, names, stems, translit) with the weights 1.0, 1.0, 0.75 and 0.25 in place
  of raw, lemma and translit with 1.0, 0.75 and 0.5, set on the synthetic eval.

## Not decided here (left for the sprint)

- Reconciliation (research item 2, Mem0): the store only closes versions of a named
  single-valued attribute and never replaces a later valid one (decision 2). ADD-only intake
  with top-k reconciliation of a fact without an attribute is the edge's, in ADR-0046 section 2.
- Entity and alias signal (research item 7): name and aliases are one bm25 column of
  `fact_fts`, so entity matches only raise lexical scores. A separate entity-match list for
  RRF (prefix or normalised match over `entities`/`entity_aliases`) is for the sprint, with
  the time signal.
- icm (research item 8, Rust, FTS5 + sqlite-vec + RRF) is the read reference for the schema
  and the fusion; this store was written without it, and the sprint compares the two and
  ports what fits with attribution.
- The feedback tool that updates `trust_score`, `retrieval_count` and `helpful_count`.

## Consequences

- Every past answer is reproducible from the rows: nothing is deleted or rewritten.
- Each binary that links SQLite grows: measured on the musl release builds, +135,552 B in
  `ulei-engine` (about 95 KB of headroom left under its 2,560 KiB budget) and +156,032 B in
  `ulei-inbox`, although neither uses FTS5 and no binary links the garden yet. The owner chose
  SQLite with FTS5 for the garden; building garden binaries with their own `LIBSQLITE3_FLAGS`
  and target directory is the way back if that headroom is needed. ADR-0007 is amended.
- A garden at 10,000 facts opens and answers its first lexical query in tens of milliseconds.
