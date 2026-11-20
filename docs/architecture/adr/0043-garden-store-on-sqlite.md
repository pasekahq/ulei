# ADR-0043: The garden store on SQLite with FTS5

Status: accepted
Date: 2026-11-20
Affects: crates/garden, dep:rusqlite, dep:serde_json

## Context

ADR-0042 gave the garden its vocabulary: entities, domains, facts on a valid-time and a
transaction-time axis with provenance, expiry and sensitivity. The facts need a durable store
that answers "what holds now", "what held then" and "what did the garden believe then", feeds
lexical and dense retrieval, and never lets a secret value into an index. The workspace already
uses bundled SQLite through `rusqlite` (ADR-0007), compiled without FTS5 to save binary size.

## Decision

1. `ulei_garden::store::Store` keeps the garden in one SQLite database through the workspace
   `rusqlite` (bundled). File databases run in WAL mode with `synchronous=NORMAL`; every
   connection has `foreign_keys=ON`. The schema is versioned by `PRAGMA user_version` and
   migrated forward on open, one transaction per step; a newer schema is refused. Every write
   is one transaction. Tables: `domains`, `entities`, `entity_aliases`, `facts`,
   `fact_vectors`, `rechecks` and the FTS5 table `fact_fts`. Other modules of the crate may
   add their own tables to the same connection.
2. Rows of `facts` are append-only; the only in-place change is setting `superseded_at`. A
   fact may name the single-valued attribute it sets (`Fact::attribute`: `address`,
   `birthday`, `phone`). Inserting a version of an attribute supersedes, at the new
   `recorded_at`, every current version of the same entity and attribute whose validity
   overlaps the new one, and writes the parts of their validity outside the new interval back
   as closed copies (`<old id>~<recorded_at>`, recorded at the new `recorded_at`, vectors
   copied). The last write wins for its interval. Facts without an attribute only add. A new
   version older on the transaction axis than the one it replaces is refused.
3. Fact lifetimes are a typed table (`TtlTable`) keyed by `TtlKind`, overridable per kind:
   weather 3 h; FX rates, pharmacies on duty, news 1 day; prices, availability, timetables 7
   days; opening hours and contacts 30 days; product capabilities 60 days (the research says
   30 to 90); laws, visas, taxes, procedures 90 days and re-check before acting; existence of
   a place, address of an institution 180 days; general and personal knowledge never expire.
   `never_cache` ("open now", live arrivals) is refused by the store. A fact without
   `expires_at` gets it from the table at insert.
4. Expiry does not delete. `Store::expired(now)` lists current facts past expiry;
   `Store::mark_expired(now)` supersedes each at `now`, writes a closed copy with `valid_to`
   at the old expiry and no expiry of its own (so "what was the price last week" stays
   answerable) and queues the expired version in `rechecks` until it is cleared.
5. Validation on insert: non-empty id, text and provenance ref; known entity and domain;
   `valid_to > valid_from`; no `superseded_at` on a new version; a never-cached kind refused.
   A secret fact needs a non-empty `vault_ref`, and its text may not look like a value (a run
   of four or more digits, or a 16+ character word mixing letters and digits).
6. Secret facts are stored as written but never enter `fact_fts` or `fact_vectors`
   (`put_vector` refuses them, `missing_vectors` skips them). They are reached only through
   their entity: `find_entities(name or alias)` then `facts_of`, which returns every fact and
   leaves visibility to the caller. `lexical` and `vectors` return private facts only when
   asked and secrets never.
7. Lexical search is a contentless FTS5 table (`contentless_delete=1`, rowid = fact row) over
   two columns, the fact text and the entity's name with aliases, tokenizer
   `unicode61 remove_diacritics 2`, prefix indexes 2, 3, 4, ranked by `bm25` (score = -bm25,
   higher is better), filtered by both time axes and sensitivity. A query becomes its distinct
   lowercased letter/digit runs as quoted prefix terms joined by `OR`, as in the retrieval
   prototype. Every version is indexed, so `as_of` searches work; renaming an entity re-indexes
   its facts.
8. Bundled SQLite is compiled with FTS5 again: `-USQLITE_ENABLE_FTS5` leaves
   `LIBSQLITE3_FLAGS` in `.cargo/config.toml`; FTS3/4 and the other modules of ADR-0007 stay
   off. Measured on the bundled `sqlite3.c` at `-Os`, FTS5 adds 140,447 B of code and data to
   each binary that links SQLite (`ulei-inbox`, `ulei-engine`, and `ulei-web` if it reaches
   SQLite). No size budget changes; the `budgets` job measures them.
9. Vectors are stored per (fact, model) as little-endian f32 blobs with their dimension; one
   model has one dimension. The text to embed is `"<entity name>: <fact text>"`.
10. The 20 starting domains are data (`src/starting_domains.json`), each bound to a Wikidata
    class checked on wikidata.org; `seed_domains` inserts the missing ones and keeps edits.
    Entity attributes are stored as a small JSON object through `serde_json`.

## Deviations from the retrieval prototype

- No lemma column: the prototype lemmatises Russian with pymorphy3, which has no Rust
  equivalent here. Prefix terms cover suffix inflection ("стоматолог" finds "стоматологу"), not
  stem changes ("стоматологи" does not find it).
- No transliteration column; the store folds `ё` to `е` itself, which `unicode61` does not.
- Two columns (text, names) with equal bm25 weights instead of raw/lemma/translit with 1.0,
  0.75, 0.5.

## Consequences

- Every past answer is reproducible from the rows: nothing is deleted or rewritten.
- Each binary that links SQLite grows by about 140 KB; the headroom of `ulei-engine` under
  its 2,560 KiB budget shrinks to roughly 90 KB.
- A garden at 10,000 facts opens and answers its first lexical query in tens of milliseconds.
