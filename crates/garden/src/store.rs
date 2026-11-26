//! SQLite storage of the garden: domains, entities with aliases, fact versions on two time
//! axes, an FTS5 index and embedding vectors (ADR-0044), and the tables of the places bed that
//! [`crate::places`] writes (ADR-0080).
//!
//! Rows of `facts` are append-only; the only in-place change is closing a version
//! (`superseded_at`, `superseded_by`). A new version of a single-valued attribute supersedes the
//! overlapping current versions and writes closed copies of the parts of their validity it does
//! not cover; a backfilled one ends where a later current version starts. A copy already
//! history when written (`valid_to` at or before `recorded_at`) has no `expires_at`; a slice
//! still valid then keeps the original's. Secret facts never enter the FTS index or the vector
//! table; they are reached only through their entity ([`Store::find_entities`] then
//! [`Store::facts_of`]).
//!
//! The seedbed holds the day's unprocessed candidates as records (ADR-0055 section 1): rows of
//! `seedbed`, never deleted, whose only change is setting `processed_at` and `outcome` once. A
//! non-secret record is the row `-seq` of `fact_fts`, so one lexical query ranks records among
//! facts; a record never gets a vector. A record is forgotten while one of its rows of
//! `seed_marks` is not lifted, and the marks are never deleted; the facts a forgotten ref
//! reaches wait in `forget_queue` until the night closes them with no new version and marks
//! the row taken (ADR-0051 section 5).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Deref;
use std::path::Path;

use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};
use serde_json::{Value, json};

use crate::edge::{secs, seed_ref, ttl_kind};
use crate::model::{
    Attrs, Domain, Entity, EntityId, EntityKind, Fact, FactId, Hardness, ModelError, PlaceAttrs,
    Polarity, PreferenceAttrs, Provenance, Sensitivity, SourceKind, Times, Ttl, TtlKind, TtlTable,
    UnixTime, VisitAttrs, WikidataQid,
};
use crate::wire::c7;
use crate::{morph, translit};

/// Page cache of 256 KiB, no memory map and temporary tables on disk for every file
/// connection: the daemon keeps two of them inside the 4 MiB peak RSS of ADR-0047 section 7.
const MEMORY_BOUND: &str =
    "PRAGMA cache_size = -256; PRAGMA mmap_size = 0; PRAGMA temp_store = FILE;";

/// Schema migrations; entry `i` moves `PRAGMA user_version` from `i` to `i + 1`.
const MIGRATIONS: &[&str] = &[
    r"
CREATE TABLE domains (
    slug TEXT PRIMARY KEY,
    label TEXT NOT NULL,
    qid TEXT,
    parent TEXT REFERENCES domains(slug)
) STRICT;
CREATE TABLE entities (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    name_norm TEXT NOT NULL,
    domain TEXT REFERENCES domains(slug),
    attrs TEXT
) STRICT;
CREATE INDEX entities_name ON entities(name_norm);
CREATE TABLE entity_aliases (
    entity TEXT NOT NULL REFERENCES entities(id),
    ord INTEGER NOT NULL,
    alias TEXT NOT NULL,
    alias_norm TEXT NOT NULL,
    PRIMARY KEY (entity, ord)
) STRICT;
CREATE INDEX entity_aliases_norm ON entity_aliases(alias_norm);
CREATE TABLE facts (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    entity TEXT NOT NULL REFERENCES entities(id),
    text TEXT NOT NULL,
    domain TEXT REFERENCES domains(slug),
    attribute TEXT,
    ttl_kind TEXT NOT NULL,
    valid_from INTEGER NOT NULL,
    valid_to INTEGER,
    recorded_at INTEGER NOT NULL,
    superseded_at INTEGER,
    source_kind TEXT NOT NULL,
    source_ref TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    expires_at INTEGER,
    sensitivity TEXT NOT NULL CHECK (sensitivity IN ('public', 'private', 'secret')),
    vault_ref TEXT,
    superseded_by TEXT,
    quote TEXT,
    proof_count INTEGER NOT NULL DEFAULT 1 CHECK (proof_count >= 1),
    confidence REAL NOT NULL DEFAULT 1.0 CHECK (confidence BETWEEN 0.0 AND 1.0),
    trust_score REAL NOT NULL DEFAULT 0.5,
    retrieval_count INTEGER NOT NULL DEFAULT 0,
    helpful_count INTEGER NOT NULL DEFAULT 0,
    CHECK (valid_to IS NULL OR valid_to > valid_from),
    CHECK ((sensitivity = 'secret') = (vault_ref IS NOT NULL))
) STRICT;
CREATE INDEX facts_entity ON facts(entity, recorded_at);
CREATE INDEX facts_current_attribute ON facts(entity, attribute) WHERE superseded_at IS NULL;
CREATE INDEX facts_expiry ON facts(expires_at)
    WHERE superseded_at IS NULL AND expires_at IS NOT NULL;
CREATE TABLE fact_vectors (
    model TEXT NOT NULL,
    fact INTEGER NOT NULL REFERENCES facts(seq),
    dim INTEGER NOT NULL,
    v BLOB NOT NULL,
    PRIMARY KEY (model, fact)
) STRICT;
CREATE TABLE rechecks (
    fact TEXT PRIMARY KEY REFERENCES facts(id),
    marked_at INTEGER NOT NULL
) STRICT;
CREATE VIRTUAL TABLE fact_fts USING fts5(
    text, names,
    content = '', contentless_delete = 1,
    tokenize = 'unicode61 remove_diacritics 2', prefix = '2 3 4'
);
",
    r"
ALTER TABLE entities ADD COLUMN merged_into TEXT REFERENCES entities(id);
",
    // The edge's queue. A schema 2 store may already hold it, created by the edge; its rows
    // without an outcome (`pending`, `failed`) are proposes whose write failed, which leave no
    // row.
    r"
CREATE TABLE IF NOT EXISTS edge_proposal (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    status TEXT NOT NULL,
    reason TEXT,
    candidate TEXT,
    conflicts_with TEXT,
    fact_id TEXT,
    source_ref TEXT,
    created_at INTEGER NOT NULL,
    resolved_at INTEGER
);
CREATE INDEX IF NOT EXISTS edge_proposal_open ON edge_proposal(status, id);
CREATE INDEX edge_proposal_duplicates ON edge_proposal(fact_id) WHERE status = 'duplicate';
DELETE FROM edge_proposal WHERE status IN ('pending', 'failed');
",
    // The miss outbox of `crate::miss`; `next_attempt` is NULL once the row is published.
    r"
CREATE TABLE miss_outbox (
    seq INTEGER PRIMARY KEY,
    query_hash TEXT NOT NULL,
    payload TEXT NOT NULL,
    missed_at INTEGER NOT NULL,
    event_id TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt INTEGER,
    published_at INTEGER,
    CHECK ((published_at IS NULL) = (next_attempt IS NOT NULL))
) STRICT;
CREATE INDEX miss_outbox_query ON miss_outbox(query_hash, missed_at);
CREATE INDEX miss_outbox_due ON miss_outbox(next_attempt) WHERE published_at IS NULL;
",
    // History copies (`valid_to` at or before `recorded_at`) drop the expiry they took from
    // the version they were cut from; a copy is found by that version, closed when the copy
    // was recorded, with the same text and observation.
    r"
UPDATE facts SET expires_at = NULL
WHERE expires_at IS NOT NULL AND valid_to IS NOT NULL AND valid_to <= recorded_at
  AND EXISTS (
    SELECT 1 FROM facts o
    WHERE o.seq < facts.seq AND o.superseded_at = facts.recorded_at AND o.text = facts.text
      AND o.source_ref = facts.source_ref AND o.observed_at = facts.observed_at
  );
",
    r"
CREATE TABLE maintenance (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last_pass INTEGER NOT NULL
) STRICT;
",
    // The query of a miss that does not look secret, until the worker asks the second level.
    r"
ALTER TABLE miss_outbox ADD COLUMN query TEXT;
",
    // The ids of the entities an `ambiguous_entity` item's name could name, as a JSON array.
    r"
ALTER TABLE edge_proposal ADD COLUMN entity_candidates TEXT;
",
    // Facts the edge accepted without an attribute, with their candidate as C7 JSON, until the
    // worker reconciles them with the facts most similar to them.
    r"
CREATE TABLE reconcile_queue (
    fact TEXT PRIMARY KEY REFERENCES facts(id),
    candidate TEXT NOT NULL
) STRICT;
",
    // Failed embeddings of a passage under a model; at `PASSAGE_TRIES` the passage is skipped.
    r"
CREATE TABLE passage_failures (
    model TEXT NOT NULL,
    fact INTEGER NOT NULL REFERENCES facts(seq),
    failures INTEGER NOT NULL CHECK (failures >= 1),
    PRIMARY KEY (model, fact)
) STRICT;
",
    // The lexical index gains the Snowball stems and the transliteration of text and names;
    // [`migrate`] refills it in Rust.
    r"
DROP TABLE IF EXISTS fact_fts;
CREATE VIRTUAL TABLE fact_fts USING fts5(
    text, names, stems, translit,
    content = '', contentless_delete = 1,
    tokenize = 'unicode61 remove_diacritics 2', prefix = '2 3 4'
);
",
    // The versions in the pinned core profile and when their chain was first pinned.
    r"
CREATE TABLE IF NOT EXISTS pins (
    fact TEXT PRIMARY KEY REFERENCES facts(id),
    pinned_at INTEGER NOT NULL
) STRICT;
",
    // The seedbed and the record each proposal of the night's processing came from.
    r"
CREATE TABLE seedbed (
    seq INTEGER PRIMARY KEY,
    candidate TEXT NOT NULL,
    entity TEXT REFERENCES entities(id),
    conflicts_with TEXT REFERENCES facts(id),
    sensitivity TEXT NOT NULL CHECK (sensitivity IN ('public', 'private', 'secret')),
    written_at INTEGER NOT NULL,
    valid_from INTEGER NOT NULL,
    valid_to INTEGER,
    expires_at INTEGER,
    processed_at INTEGER,
    outcome TEXT,
    CHECK (valid_to IS NULL OR valid_to > valid_from),
    CHECK ((processed_at IS NULL) = (outcome IS NULL))
) STRICT;
CREATE INDEX seedbed_unprocessed ON seedbed(written_at, seq) WHERE processed_at IS NULL;
ALTER TABLE edge_proposal ADD COLUMN seed INTEGER;
",
    // Forget marks of seedbed records, and the facts forgotten refs reach until the night
    // reconciles them; apart from `reconcile_queue`.
    r"
CREATE TABLE seed_marks (
    id INTEGER PRIMARY KEY,
    seed INTEGER NOT NULL REFERENCES seedbed(seq),
    at INTEGER NOT NULL,
    reason TEXT NOT NULL,
    provenance TEXT NOT NULL,
    lifted_at INTEGER,
    lifted_provenance TEXT,
    CHECK ((lifted_at IS NULL) = (lifted_provenance IS NULL))
) STRICT;
CREATE UNIQUE INDEX seed_marks_live ON seed_marks(seed) WHERE lifted_at IS NULL;
CREATE TABLE forget_queue (
    id INTEGER PRIMARY KEY,
    fact_id TEXT NOT NULL REFERENCES facts(id),
    ref TEXT NOT NULL,
    marked_at INTEGER NOT NULL,
    reason TEXT NOT NULL,
    queued_at INTEGER NOT NULL,
    taken_at INTEGER
) STRICT;
CREATE UNIQUE INDEX forget_queue_waiting ON forget_queue(ref, fact_id) WHERE taken_at IS NULL;
",
    // The places bed and the TTL policy of type `place` (ADR-0080 sections 2 and 3).
    r"
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
INSERT INTO attr_policy (type, key, form, ttl_days, on_stale) VALUES
    ('place', 'rating', 'number', 30, 'demote'),
    ('place', 'review_count', 'count', 30, 'demote'),
    ('place', 'price_for_two', 'range', 30, 'demote'),
    ('place', 'hours', 'hours', 7, 'distrust'),
    ('place', 'operating_status', 'status', 14, 'demote'),
    ('place', 'cuisine', 'list', 365, 'demote'),
    ('place', 'location', 'point', 365, 'show'),
    ('place', 'address', 'text', 365, 'show'),
    ('place', 'osm_id', 'osm_id', NULL, 'show'),
    ('place', 'maps_url', 'url', NULL, 'show'),
    ('place', 'vibe', 'list', NULL, 'show'),
    ('place', 'warnings', 'list', NULL, 'show'),
    ('place', 'notes', 'text', NULL, 'show'),
    ('place', 'verdict', 'verdict', NULL, 'show');
",
    // When each vector was computed, 0 (before any check of the model file, so trusted) for the
    // vectors stored before, and the last content checks of the encoder engine's weight file
    // (ADR-0086 open question 15): a failed check drops the vectors computed since the last
    // passed one.
    r"
ALTER TABLE fact_vectors ADD COLUMN computed_at INTEGER NOT NULL DEFAULT 0;
CREATE TABLE model_checks (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    passed_at INTEGER,
    digest TEXT,
    failed_at INTEGER,
    failure TEXT,
    CHECK ((passed_at IS NULL) = (digest IS NULL)),
    CHECK ((failed_at IS NULL) = (failure IS NULL))
) STRICT;
",
];

/// Failed embeddings after which a passage is skipped: a helper that crashed on or refused the
/// same text this often will do it again, and each crash costs a restart.
pub const PASSAGE_TRIES: i64 = 3;

/// The schema version whose migration also re-normalises the stored names in Rust
/// ([`renormalise`]): SQLite cannot fold Unicode case or collapse runs of whitespace.
const RENORMALISED: usize = 2;

/// The schema version whose migration refills `fact_fts` in Rust ([`reindex_all`]): SQLite has
/// no Russian stemmer. The refill runs after the last migration, when the seedbed exists.
const REINDEXED: usize = 11;

/// bm25 weights of the `fact_fts` columns text, names, stems and translit, set on the synthetic
/// eval.
const COLUMN_WEIGHTS: &str = "1.0, 1.0, 0.75, 0.25";

const FACT_COLS: &str = "f.id, f.entity, f.text, f.domain, f.attribute, f.ttl_kind, \
    f.valid_from, f.valid_to, f.recorded_at, f.superseded_at, f.source_kind, f.source_ref, \
    f.observed_at, f.expires_at, f.sensitivity, f.vault_ref, f.superseded_by, f.quote, \
    f.proof_count, f.confidence";

/// SQL form of [`Times::holds`] over alias `$a`, whose transaction axis runs from `$recorded` to
/// `$superseded`, with `?1` = `valid_at` and `?2` = `known_at`.
#[rustfmt::skip]
macro_rules! holds {
    ($a:literal, $recorded:literal, $superseded:literal) => {
        concat!(
            $a, ".valid_from <= ?1 AND (", $a, ".valid_to IS NULL OR ?1 < ", $a, ".valid_to)",
            " AND ", $a, ".", $recorded, " <= ?2",
            " AND (", $a, ".", $superseded, " IS NULL OR ?2 < ", $a, ".", $superseded, ")",
            " AND (", $a, ".expires_at IS NULL OR ?2 < ", $a, ".expires_at)"
        )
    };
}

/// Visibility filter over alias `$a` with `?3` = `include_private`; secrets never pass.
#[rustfmt::skip]
macro_rules! visible {
    ($a:literal) => {
        concat!("(", $a, ".sensitivity = 'public' OR (?3 AND ", $a, ".sensitivity = 'private'))")
    };
}

/// [`Fact::holds`] over alias `f`.
const HOLDS: &str = holds!("f", "recorded_at", "superseded_at");

/// A seedbed record over alias `s` holds from its writing to its processing.
const SEED_HOLDS: &str = holds!("s", "written_at", "processed_at");

const VISIBLE: &str = visible!("f");

const SEED_VISIBLE: &str = visible!("s");

/// A seedbed record over alias `s` that no live mark forgets.
const SEED_KEPT: &str =
    "NOT EXISTS (SELECT 1 FROM seed_marks m WHERE m.seed = s.seq AND m.lifted_at IS NULL)";

const SEED_COLS: &str = "s.seq, s.candidate, s.entity, s.conflicts_with, s.sensitivity, \
    s.written_at, s.valid_from, s.valid_to, s.expires_at, s.processed_at, s.outcome";

/// An error of the store.
#[derive(Debug)]
pub enum StoreError {
    /// SQLite failed.
    Sqlite(rusqlite::Error),
    /// The input breaks a rule of the garden; the text says which.
    Invalid(String),
    /// No such fact, entity or domain.
    NotFound(String),
    /// A stored row cannot be decoded.
    Corrupt(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "sqlite: {e}"),
            Self::Invalid(s) => write!(f, "invalid: {s}"),
            Self::NotFound(s) => write!(f, "not found: {s}"),
            Self::Corrupt(s) => write!(f, "corrupt row: {s}"),
        }
    }
}

impl StoreError {
    /// A short name of the kind of failure, free of any stored or proposed text.
    pub fn class(&self) -> &'static str {
        match self {
            Self::Sqlite(e) => sqlite_class(e),
            Self::Invalid(_) => "invalid",
            Self::NotFound(_) => "not_found",
            Self::Corrupt(_) => "corrupt_row",
        }
    }
}

/// [`StoreError::class`] of a SQLite failure.
pub fn sqlite_class(e: &rusqlite::Error) -> &'static str {
    match e.sqlite_error_code() {
        Some(ErrorCode::ConstraintViolation) => "sqlite_constraint",
        Some(ErrorCode::ReadOnly) => "sqlite_readonly",
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => "sqlite_busy",
        Some(ErrorCode::DiskFull) => "sqlite_full",
        Some(ErrorCode::SystemIoFailure) => "sqlite_io",
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => "sqlite_corrupt",
        _ => "sqlite",
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ModelError> for StoreError {
    fn from(e: ModelError) -> Self {
        Self::Invalid(e.to_string())
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, StoreError> {
    Err(StoreError::Invalid(msg.into()))
}

/// The garden's storage on one SQLite database.
pub struct Store {
    conn: Connection,
    ttl: TtlTable,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

/// The last content checks of the model file on record (ADR-0086 open question 15).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelChecks {
    /// When the last passed check began and the content digest it found.
    pub passed: Option<(UnixTime, String)>,
    /// When the last failed check began and why, unless a check passed since.
    pub failed: Option<(UnixTime, String)>,
}

/// A stored vector with the times and the visibility of its version.
#[derive(Clone, Debug, PartialEq)]
pub struct ResidentVector {
    /// The version's row in `facts`.
    pub seq: i64,
    /// The version.
    pub fact: FactId,
    /// The version's times.
    pub times: Times,
    /// Whether it is private.
    pub private: bool,
    /// The vector.
    pub vector: Vec<f32>,
}

/// The `source_ref` prefix of evidence that is an item of the history archive (C10): what the
/// night lifted from past conversations rather than what the owner curated.
pub const HISTORY_REF_PREFIX: &str = "archive:";

/// What a row of `fact_fts` indexes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Indexed {
    /// A fact version, at the rowid of its `seq`.
    Fact(FactId),
    /// The seedbed record with this `seq`, at rowid `-seq`.
    Seed(i64),
}

impl Indexed {
    /// The fact version, when this is one.
    pub fn as_fact(&self) -> Option<&FactId> {
        match self {
            Self::Fact(id) => Some(id),
            Self::Seed(_) => None,
        }
    }
}

/// A record of the seedbed: one candidate as it was written (ADR-0055 section 1).
#[derive(Clone, Debug, PartialEq)]
pub struct SeedRecord {
    /// Its key from 1; its ref is `seed:<seq>`.
    pub seq: i64,
    /// The candidate as written.
    pub candidate: c7::ProposeRequest,
    /// The one entity the write resolved the candidate's name to.
    pub entity: Option<EntityId>,
    /// The current fact the record disputes.
    pub conflicts_with: Option<FactId>,
    /// Private with the candidate's `private`, a secret pointer with its `secret_ref`.
    pub sensitivity: Sensitivity,
    /// The garden's time of the write.
    pub written_at: UnixTime,
    /// The candidate's `valid_from`, else its `observed_at`, else `written_at`.
    pub valid_from: UnixTime,
    /// The candidate's `valid_to`.
    pub valid_to: Option<UnixTime>,
    /// The candidate's expiry by the TTL table; `None` = no expiry.
    pub expires_at: Option<UnixTime>,
    /// When the night stage processed it and the edge's answers.
    pub processed: Option<(UnixTime, Vec<c7::ProposeResponse>)>,
}

/// A ref `forget` takes (ADR-0051 section 5). Each reaches the current facts whose
/// `source_ref` is its text; a record also reaches those its accepted proposals wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgetRef {
    /// The seedbed record with this `seq`, which the garden marks.
    Seed(i64),
    /// A `c8:` ref, as given; the session journal holds its marks.
    Journal(String),
}

impl ForgetRef {
    /// The ref as written.
    pub fn text(&self) -> String {
        match self {
            Self::Seed(seq) => seed_ref(*seq),
            Self::Journal(r) => r.clone(),
        }
    }

    fn seed(&self) -> Option<i64> {
        match self {
            Self::Seed(seq) => Some(*seq),
            Self::Journal(_) => None,
        }
    }
}

/// What [`Store::clear_forgotten`] did with one row of the forget queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cleared {
    /// The row's fact version.
    pub fact: FactId,
    /// True when this call closed the version.
    pub closed: bool,
}

/// What a `forget` or its undo changed for one ref.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Changed {
    /// Marks written or lifted.
    pub marks: usize,
    /// Queue rows written or removed.
    pub queue: usize,
}

/// A forget mark of a seedbed record; never deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedMark {
    /// Its key.
    pub id: i64,
    /// The record's `seq`.
    pub seed: i64,
    /// When it was made.
    pub at: UnixTime,
    /// Why the owner asked.
    pub reason: String,
    /// The owner's command that made it.
    pub provenance: String,
    /// When it was lifted and by which command; `None` while it is live.
    pub lifted: Option<(UnixTime, String)>,
}

/// A row of the forget queue: a current fact a forgotten ref reached, for the night.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgetQueued {
    /// Its key from 1.
    pub id: i64,
    /// The fact version.
    pub fact: FactId,
    /// The ref that reached it.
    pub r#ref: String,
    /// The time of the mark.
    pub marked_at: UnixTime,
    /// The reason of the mark.
    pub reason: String,
    /// When it was queued.
    pub queued_at: UnixTime,
    /// When the night took it.
    pub taken_at: Option<UnixTime>,
}

impl Store {
    /// Opens or creates the database at `path` in WAL mode with a bounded page cache and
    /// migrates it.
    ///
    /// # Errors
    ///
    /// SQLite errors, or [`StoreError::Invalid`] for a schema newer than this code.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 5000;",
        )?;
        conn.execute_batch(MEMORY_BOUND)?;
        Self::init(conn)
    }

    /// Opens a fresh in-memory database.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        migrate(&mut conn)?;
        Ok(Self {
            conn,
            ttl: TtlTable::default(),
        })
    }

    /// The connection; other modules of the crate may keep their own tables in it.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Schema version (`PRAGMA user_version`) of the open database.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// Runs `f` as one unit of work: what it writes through the store commits when it returns
    /// `Ok` and is rolled back when it returns `Err`. Units nest; an inner unit that fails rolls
    /// back only its own writes. The outermost unit holds the write lock from its start.
    ///
    /// # Errors
    ///
    /// The error of `f`, or the SQLite failure that kept the unit from starting, committing or
    /// rolling back.
    pub fn atomically<T, E: From<StoreError>>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E> {
        let depth = Depth::begin(&self.conn)?;
        let out = match f(self) {
            Ok(v) => match depth.end(&self.conn) {
                Ok(()) => return Ok(v),
                Err(e) => Err(e.into()),
            },
            Err(e) => Err(e),
        };
        depth.undo(&self.conn)?;
        out
    }

    /// The TTL table used to fill missing expiries and reject never-cached facts.
    pub fn ttl_table(&self) -> &TtlTable {
        &self.ttl
    }

    /// Replaces the TTL table (a config override).
    pub fn set_ttl_table(&mut self, t: TtlTable) {
        self.ttl = t;
    }

    /// Inserts or replaces a domain; its parent must already exist and must not descend from it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a bad slug, an empty label, an unknown parent or a parent
    /// that would close a cycle in the hierarchy.
    pub fn upsert_domain(&mut self, d: &Domain) -> Result<(), StoreError> {
        d.validate()?;
        let tx = Unit::begin(&self.conn)?;
        if let Some(p) = &d.parent {
            if !exists(&tx, "SELECT 1 FROM domains WHERE slug = ?1", p)? {
                return invalid(format!("domain {}: unknown parent {p}", d.slug));
            }
            let mut chain = vec![d.slug.clone(), p.clone()];
            while let Some(last) = chain.last().filter(|s| **s != d.slug) {
                if chain.len() > MAX_DOMAIN_DEPTH {
                    return Err(StoreError::Corrupt(format!(
                        "domain {}: ancestors do not end",
                        d.slug
                    )));
                }
                let up: Option<String> = tx
                    .query_row("SELECT parent FROM domains WHERE slug = ?1", [last], |r| {
                        r.get(0)
                    })
                    .optional()?
                    .flatten();
                let Some(up) = up else { break };
                chain.push(up);
            }
            if chain.last() == Some(&d.slug) {
                return invalid(format!(
                    "domain {}: parent {p} closes the cycle {}",
                    d.slug,
                    chain.join(" -> ")
                ));
            }
        }
        tx.execute(
            "INSERT INTO domains (slug, label, qid, parent) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(slug) DO UPDATE SET label = ?2, qid = ?3, parent = ?4",
            params![
                d.slug,
                d.label,
                d.wikidata.as_ref().map(WikidataQid::as_str),
                d.parent
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The domain with `slug`, if any.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn domain(&self, slug: &str) -> Result<Option<Domain>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT slug, label, qid, parent FROM domains WHERE slug = ?1",
                [slug],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(slug, label, qid, parent)| {
            let wikidata = qid
                .map(|q| WikidataQid::parse(&q).map_err(|e| StoreError::Corrupt(e.to_string())))
                .transpose()?;
            Ok(Domain {
                slug,
                label,
                wikidata,
                parent,
            })
        })
        .transpose()
    }

    /// Inserts the domains of `ds` that are not stored yet, in order, in one transaction;
    /// stored ones are left as they are, whatever `ds` says about them. Returns how many were
    /// inserted.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a cycle among the parents `ds` names, and as
    /// [`Store::upsert_domain`]; nothing is inserted then.
    pub fn seed_domains(&mut self, ds: &[Domain]) -> Result<usize, StoreError> {
        check_hierarchy(ds)?;
        let tx = Unit::begin(&self.conn)?;
        let mut n = 0;
        for d in ds {
            d.validate()?;
            if let Some(p) = &d.parent
                && !exists(&tx, "SELECT 1 FROM domains WHERE slug = ?1", p)?
            {
                return invalid(format!("domain {}: unknown parent {p}", d.slug));
            }
            n += tx.execute(
                "INSERT OR IGNORE INTO domains (slug, label, qid, parent) VALUES (?1, ?2, ?3, ?4)",
                params![
                    d.slug,
                    d.label,
                    d.wikidata.as_ref().map(WikidataQid::as_str),
                    d.parent
                ],
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Every domain, by slug.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn domains(&self) -> Result<Vec<Domain>, StoreError> {
        let slugs: Vec<String> = self
            .conn
            .prepare("SELECT slug FROM domains ORDER BY slug")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        slugs
            .iter()
            .filter_map(|s| self.domain(s).transpose())
            .collect()
    }

    /// Inserts or replaces an entity with its aliases and re-indexes its facts under the new
    /// names; returns the id it stored or resolved to. For a kind a name identifies
    /// ([`EntityKind::is_identity`]), a new id whose name or an alias equals ([`norm`]) the name
    /// or an alias of exactly one other entity of its kind writes nothing and resolves to it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] if [`Entity::validate`] fails, the domain is unknown, an entity
    /// the attributes name is missing, merged or of the wrong kind (a preference's holder and
    /// a visit's `with` are people, a visit's place is a place), the entity was merged, or, for
    /// a kind a name identifies, its names match two or more other entities of its kind (a new
    /// id) or any (a stored id).
    pub fn upsert_entity(&mut self, e: &Entity) -> Result<EntityId, StoreError> {
        self.write_entity(e, e.kind.is_identity())
    }

    /// [`Store::upsert_entity`] that never resolves names: the entity is written under its own
    /// id even when another one bears its name, as documents of a corpus do.
    ///
    /// # Errors
    ///
    /// As [`Store::upsert_entity`], name matches aside.
    pub fn upsert_entity_unresolved(&mut self, e: &Entity) -> Result<(), StoreError> {
        self.write_entity(e, false).map(drop)
    }

    fn write_entity(&mut self, e: &Entity, resolve: bool) -> Result<EntityId, StoreError> {
        e.validate()?;
        let tx = Unit::begin(&self.conn)?;
        let id = &e.id.0;
        let standing = standing(&tx, &e.id)?;
        if let Standing::Merged(into) = &standing {
            return invalid(format!("entity {id}: merged into {into}"));
        }
        if let Some(d) = &e.domain
            && !exists(&tx, "SELECT 1 FROM domains WHERE slug = ?1", d)?
        {
            return invalid(format!("entity {id}: unknown domain {d}"));
        }
        check_refs(&tx, e)?;
        let namesakes = if resolve {
            namesakes(&tx, e)?
        } else {
            Vec::new()
        };
        match (standing == Standing::Live, namesakes.as_slice()) {
            (_, []) => {}
            (false, [one]) => return Ok(EntityId(one.clone())),
            (false, _) => {
                return invalid(format!(
                    "entity {id}: its names match {} entities of kind {}: {}",
                    namesakes.len(),
                    e.kind,
                    namesakes.join(", ")
                ));
            }
            (true, _) => {
                return invalid(format!(
                    "entity {id}: its names already name {} of kind {}; merge instead",
                    namesakes.join(", "),
                    e.kind
                ));
            }
        }
        let attrs = encode_attrs(&e.attrs);
        tx.execute(
            "INSERT INTO entities (id, kind, name, name_norm, domain, attrs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET kind = ?2, name = ?3, name_norm = ?4, domain = ?5,
                 attrs = ?6",
            params![id, e.kind.as_str(), e.name, norm(&e.name), e.domain, attrs],
        )?;
        write_aliases(&tx, &e.id, &e.aliases)?;
        reindex_entity(&tx, &e.id)?;
        tx.commit()?;
        Ok(e.id.clone())
    }

    /// The entity with `id` as stored, a merged one included.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn entity(&self, id: &EntityId) -> Result<Option<Entity>, StoreError> {
        load_entity(&self.conn, id)
    }

    /// The entity `id` stands for now: itself, or the entity it was merged into.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn survivor(&self, id: &EntityId) -> Result<Option<Entity>, StoreError> {
        match standing(&self.conn, id)? {
            Standing::Unknown => Ok(None),
            Standing::Live => load_entity(&self.conn, id),
            Standing::Merged(into) => load_entity(&self.conn, &EntityId(into)),
        }
    }

    /// Entities not merged away whose name or an alias equals `name_or_alias` after [`norm`],
    /// by id.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn find_entities(&self, name_or_alias: &str) -> Result<Vec<Entity>, StoreError> {
        let ids: Vec<String> = self
            .conn
            .prepare(
                "SELECT id FROM entities WHERE merged_into IS NULL
                   AND (name_norm = ?1
                     OR id IN (SELECT entity FROM entity_aliases WHERE alias_norm = ?1))
                 ORDER BY id",
            )?
            .query_map([norm(name_or_alias)], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        ids.into_iter()
            .filter_map(|id| load_entity(&self.conn, &EntityId(id)).transpose())
            .collect()
    }

    /// Every name and alias of the entities not merged away, with the entity's id and kind, by
    /// id.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable kind.
    pub fn entity_names(&self) -> Result<Vec<(EntityId, EntityKind, String)>, StoreError> {
        let rows: Vec<(String, String, String)> = self
            .conn
            .prepare(
                "SELECT id, kind, name FROM entities WHERE merged_into IS NULL
                 UNION ALL
                 SELECT e.id, e.kind, a.alias FROM entity_aliases a
                 JOIN entities e ON e.id = a.entity WHERE e.merged_into IS NULL
                 ORDER BY 1",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        rows.into_iter()
            .map(|(id, kind, name)| {
                let kind = EntityKind::parse(&kind)
                    .ok_or_else(|| StoreError::Corrupt(format!("entity {id}: kind {kind}")))?;
                Ok((EntityId(id), kind, name))
            })
            .collect()
    }

    /// Merges entity `from` into `into` of the same kind at transaction time `at`: the name
    /// and aliases of `from` become aliases of `into`, every current version about `from` is
    /// closed at `at` and rewritten on `into` (vectors included) with `superseded_by` naming
    /// the copy, attributes that name `from` name `into`, and the unprocessed records resolved
    /// to `from` are indexed under the names of `into`. `from` keeps its row and its history but
    /// is no longer found by name. Returns the ids of the copies.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown id; [`StoreError::Invalid`] for one entity, two
    /// kinds, a merged entity, a current version of `from` recorded after `at`, or a
    /// single-valued attribute both hold now over overlapping validity.
    pub fn merge_entities(
        &mut self,
        from: &EntityId,
        into: &EntityId,
        at: UnixTime,
    ) -> Result<Vec<FactId>, StoreError> {
        if from == into {
            return invalid(format!("entity {}: cannot merge into itself", from.0));
        }
        let tx = Unit::begin(&self.conn)?;
        let (gone, kept) = (live_entity(&tx, from)?, live_entity(&tx, into)?);
        if gone.kind != kept.kind {
            return invalid(format!(
                "entity {} is a {}, entity {} a {}: only one kind merges",
                from.0, gone.kind, into.0, kept.kind
            ));
        }
        let current = format!(
            "SELECT {FACT_COLS} FROM facts f WHERE f.entity = ?1 AND f.superseded_at IS NULL
             ORDER BY f.seq"
        );
        let moving = query_facts(&tx, &current, [&from.0])?;
        let staying = query_facts(&tx, &current, [&into.0])?;
        for f in &moving {
            let clash = staying
                .iter()
                .find(|k| f.attribute.is_some() && k.attribute == f.attribute && overlap(f, k));
            if let Some(k) = clash {
                return invalid(format!(
                    "entities {} and {}: current facts {} and {} set {} over overlapping \
                     validity; supersede one first",
                    from.0,
                    into.0,
                    f.id.0,
                    k.id.0,
                    k.attribute.as_deref().unwrap_or_default()
                ));
            }
        }
        let mut taken: Vec<String> = std::iter::once(&kept.name)
            .chain(&kept.aliases)
            .map(|n| norm(n))
            .collect();
        let mut aliases = kept.aliases.clone();
        for n in std::iter::once(&gone.name).chain(&gone.aliases) {
            if !taken.contains(&norm(n)) {
                taken.push(norm(n));
                aliases.push(n.clone());
            }
        }
        tx.execute("DELETE FROM entity_aliases WHERE entity = ?1", [&from.0])?;
        write_aliases(&tx, into, &aliases)?;
        tx.execute(
            "UPDATE entities SET merged_into = ?2 WHERE id = ?1 OR merged_into = ?1",
            params![from.0, into.0],
        )?;
        redirect_refs(&tx, from, into)?;
        let mut copies = Vec::with_capacity(moving.len());
        for f in &moving {
            let mut c = f.clone();
            c.id = copy_id(&tx, &f.id, at)?;
            c.entity = into.clone();
            c.recorded_at = at;
            close(&tx, f, at, Some(&c.id))?;
            copies.push(write_copy(&tx, &f.id, c)?);
        }
        reindex_entity(&tx, into)?;
        reindex_seeds(&tx, from, into)?;
        tx.commit()?;
        Ok(copies)
    }

    /// Records a new current version (`superseded_at` must be `None`). A missing `expires_at`
    /// is filled from the TTL table. With an `attribute`, an open-ended version that starts
    /// before a current version of the same entity and attribute is stored with `valid_to` at
    /// the earliest such start (a backfill never replaces a later version); then every current
    /// version whose validity overlaps it is superseded at `recorded_at` by it, and the parts
    /// of its validity outside the new one are written back as closed copies. Without an
    /// attribute the fact only adds.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] if [`Fact::validate`] fails, for a superseded version, a
    /// duplicate id, an unknown entity or domain, a never-cached kind, an expiry not after both
    /// the observation and the recording, or a version older than the one it would replace.
    pub fn insert_fact(&mut self, f: &Fact) -> Result<(), StoreError> {
        let f = self.prepare_fact(f)?;
        let tx = Unit::begin(&self.conn)?;
        insert_version(&tx, &f)?;
        tx.commit()?;
        Ok(())
    }

    /// [`Store::insert_fact`] for many facts in one transaction: all or none are stored.
    ///
    /// # Errors
    ///
    /// As [`Store::insert_fact`], for the first failing fact.
    pub fn insert_facts(&mut self, facts: &[Fact]) -> Result<(), StoreError> {
        let facts = facts
            .iter()
            .map(|f| self.prepare_fact(f))
            .collect::<Result<Vec<_>, _>>()?;
        let tx = Unit::begin(&self.conn)?;
        for f in &facts {
            insert_version(&tx, f)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn prepare_fact(&self, f: &Fact) -> Result<Fact, StoreError> {
        f.validate()?;
        if f.superseded_at.is_some() {
            return invalid(format!(
                "fact {}: a new version cannot be superseded",
                f.id.0
            ));
        }
        if self.ttl.rule(f.ttl).ttl == Ttl::NeverCache {
            return invalid(format!("fact {}: kind {} is never cached", f.id.0, f.ttl));
        }
        let mut f = f.clone();
        if f.expires_at.is_none() {
            f.expires_at = self.ttl.expiry(f.ttl, f.provenance.observed_at);
        }
        if let Some(x) = f.expires_at
            && (x <= f.recorded_at || x <= f.provenance.observed_at)
        {
            return invalid(format!(
                "fact {}: expired at {x}, before it was observed or recorded",
                f.id.0
            ));
        }
        Ok(f)
    }

    /// Retracts the current version `id` at transaction time `known_at`.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown id; [`StoreError::Invalid`] if it is already
    /// superseded or `known_at` precedes its `recorded_at`.
    pub fn supersede(&mut self, id: &FactId, known_at: UnixTime) -> Result<(), StoreError> {
        let tx = Unit::begin(&self.conn)?;
        let f =
            load_fact(&tx, id)?.ok_or_else(|| StoreError::NotFound(format!("fact {}", id.0)))?;
        close(&tx, &f, known_at, None)?;
        tx.commit()?;
        Ok(())
    }

    /// Retracts the current version `id` at `known_at` in favour of `by`, keeping copies of it
    /// (vectors included) over the valid-time slices `keep`; returns the copies' ids.
    ///
    /// # Errors
    ///
    /// As [`Store::supersede`], and [`StoreError::Invalid`] for an empty slice.
    pub fn supersede_by(
        &mut self,
        id: &FactId,
        known_at: UnixTime,
        by: &FactId,
        keep: &[(UnixTime, Option<UnixTime>)],
    ) -> Result<Vec<FactId>, StoreError> {
        let tx = Unit::begin(&self.conn)?;
        let f =
            load_fact(&tx, id)?.ok_or_else(|| StoreError::NotFound(format!("fact {}", id.0)))?;
        close(&tx, &f, known_at, Some(by))?;
        let mut copies = Vec::with_capacity(keep.len());
        for &(from, to) in keep {
            if to.is_some_and(|t| t <= from) {
                return invalid(format!("fact {}: empty slice [{from}, {to:?})", id.0));
            }
            let mut c = f.clone();
            c.id = copy_id(&tx, &f.id, known_at)?;
            c.valid_from = from;
            c.valid_to = to;
            c.recorded_at = known_at;
            copies.push(write_copy(&tx, &f.id, c)?);
        }
        tx.commit()?;
        Ok(copies)
    }

    /// The fact version with `id`, if any.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn fact(&self, id: &FactId) -> Result<Option<Fact>, StoreError> {
        load_fact(&self.conn, id)
    }

    /// Facts about `e` that hold at (`valid_at`, `known_at`), secrets and private ones
    /// included, by `valid_from`. The caller gates visibility.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn facts_of(
        &self,
        e: &EntityId,
        valid_at: UnixTime,
        known_at: UnixTime,
    ) -> Result<Vec<Fact>, StoreError> {
        let sql = format!(
            "SELECT {FACT_COLS} FROM facts f WHERE f.entity = ?3 AND {HOLDS}
             ORDER BY f.valid_from, f.seq"
        );
        query_facts(&self.conn, &sql, params![valid_at, known_at, e.0])
    }

    /// Every version of every fact about `e`, oldest recorded first.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn history(&self, e: &EntityId) -> Result<Vec<Fact>, StoreError> {
        let sql = format!(
            "SELECT {FACT_COLS} FROM facts f WHERE f.entity = ?1
             ORDER BY f.recorded_at, f.valid_from, f.seq"
        );
        query_facts(&self.conn, &sql, params![e.0])
    }

    /// FTS5 search over fact text and the entity's name and aliases, and over seedbed records,
    /// as written, as Russian stems and transliterated: the best `k` facts and records that hold
    /// at (`valid_at`, `known_at`), best first, score = -bm25 (higher is better). A record holds
    /// from its writing until its processing, within its valid interval and before its expiry,
    /// and is left out while it is forgotten. Private ones only with `include_private`; secrets
    /// never.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn lexical(
        &self,
        q: &str,
        k: usize,
        valid_at: UnixTime,
        known_at: UnixTime,
        include_private: bool,
    ) -> Result<Vec<(Indexed, f32)>, StoreError> {
        self.lexical_where(q, k, valid_at, known_at, include_private, None)
    }

    /// [`Store::lexical`] over the facts about `e` only, private ones included; no records.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn lexical_of(
        &self,
        e: &EntityId,
        q: &str,
        k: usize,
        valid_at: UnixTime,
        known_at: UnixTime,
    ) -> Result<Vec<(FactId, f32)>, StoreError> {
        let hits = self.lexical_where(q, k, valid_at, known_at, true, Some(e))?;
        hits.into_iter()
            .map(|(hit, score)| match hit {
                Indexed::Fact(id) => Ok((id, score)),
                Indexed::Seed(seq) => Err(StoreError::Corrupt(format!(
                    "record {seq} in a search of entity {}",
                    e.0
                ))),
            })
            .collect()
    }

    fn lexical_where(
        &self,
        q: &str,
        k: usize,
        valid_at: UnixTime,
        known_at: UnixTime,
        include_private: bool,
        entity: Option<&EntityId>,
    ) -> Result<Vec<(Indexed, f32)>, StoreError> {
        let Some(expr) = match_expression(q) else {
            return Ok(Vec::new());
        };
        let k = i64::try_from(k).unwrap_or(i64::MAX);
        let sql = format!(
            "SELECT f.id, s.seq, bm25(fact_fts, {COLUMN_WEIGHTS}) AS rank FROM fact_fts
             LEFT JOIN facts f ON f.seq = fact_fts.rowid
             LEFT JOIN seedbed s ON s.seq = -fact_fts.rowid
             WHERE fact_fts MATCH ?4
               AND ((f.seq IS NOT NULL AND {HOLDS} AND {VISIBLE} AND (?6 IS NULL OR f.entity = ?6))
                 OR (?6 IS NULL AND s.seq IS NOT NULL AND {SEED_HOLDS} AND {SEED_VISIBLE}
                     AND {SEED_KEPT}))
             ORDER BY rank, fact_fts.rowid LIMIT ?5"
        );
        let entity = entity.map(|e| e.0.as_str());
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(
            params![valid_at, known_at, include_private, expr, k, entity],
            |r| {
                #[allow(clippy::cast_possible_truncation)]
                let score = -r.get::<_, f64>(2)? as f32;
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    score,
                ))
            },
        )?;
        rows.map(|r| match r? {
            (Some(id), None, score) => Ok((Indexed::Fact(FactId(id)), score)),
            (None, Some(seq), score) => Ok((Indexed::Seed(seq), score)),
            _ => Err(StoreError::Corrupt(
                "an index row of both a fact and a record".into(),
            )),
        })
        .collect()
    }

    /// Writes `candidate` as a seedbed record at `written_at`, with its index row unless it is a
    /// secret pointer, and returns its `seq`. `entity` is the entity the write resolved its name
    /// to and `conflicts_with` the current fact it disputes. Its expiry is the TTL table's for its
    /// `ttl_kind`, else the edge's default for its source, from its `observed_at`, else
    /// `written_at`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a never-cached kind, an expiry at or before `written_at` or a
    /// valid interval that ends at or before it starts; SQLite errors, an unknown `entity` or
    /// `conflicts_with` among them. Nothing is written then.
    pub fn write_seed(
        &mut self,
        candidate: &c7::ProposeRequest,
        entity: Option<&EntityId>,
        conflicts_with: Option<&FactId>,
        written_at: UnixTime,
    ) -> Result<i64, StoreError> {
        let kind = ttl_kind(candidate);
        if self.ttl.rule(kind).ttl == Ttl::NeverCache {
            return invalid(format!("record: kind {kind} is never cached"));
        }
        let observed_at = candidate.observed_at.map(secs);
        let expires_at = self.ttl.expiry(kind, observed_at.unwrap_or(written_at));
        if let Some(x) = expires_at
            && x <= written_at
        {
            return invalid(format!("record: expired at {x}, before it was written"));
        }
        let valid_from = candidate
            .valid_from
            .map(secs)
            .or(observed_at)
            .unwrap_or(written_at);
        let valid_to = candidate.valid_to.map(secs);
        if valid_to.is_some_and(|t| t <= valid_from) {
            return invalid(format!(
                "record: valid from {valid_from} to {valid_to:?} is empty"
            ));
        }
        let sensitivity = match (&candidate.secret_ref, candidate.private) {
            (Some(_), _) => "secret",
            (None, Some(true)) => "private",
            (None, _) => "public",
        };
        let json = serde_json::to_string(candidate)
            .map_err(|e| StoreError::Invalid(format!("record: {e}")))?;
        let tx = Unit::begin(&self.conn)?;
        let seq: i64 = tx.query_row(
            "INSERT INTO seedbed (candidate, entity, conflicts_with, sensitivity, written_at,
                 valid_from, valid_to, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) RETURNING seq",
            params![
                json,
                entity.map(|e| e.0.as_str()),
                conflicts_with.map(|f| f.0.as_str()),
                sensitivity,
                written_at,
                valid_from,
                valid_to,
                expires_at
            ],
            |r| r.get(0),
        )?;
        if candidate.secret_ref.is_none() {
            index_seed(&tx, seq, candidate, entity)?;
        }
        tx.commit()?;
        Ok(seq)
    }

    /// The seedbed record `seq`, if any.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn seed(&self, seq: i64) -> Result<Option<SeedRecord>, StoreError> {
        let sql = format!("SELECT {SEED_COLS} FROM seedbed s WHERE s.seq = ?1");
        Ok(query_seeds(&self.conn, &sql, [seq])?.pop())
    }

    /// The records not expired or forgotten at `now` whose candidate may have the `source_ref`
    /// and `entity` of `candidate`, oldest first, processed or not; the caller compares the
    /// candidates.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn seeds_said(
        &self,
        candidate: &c7::ProposeRequest,
        now: UnixTime,
    ) -> Result<Vec<SeedRecord>, StoreError> {
        // SQLite is built without JSON functions: a stored candidate holds these fields as
        // serde writes them, so their text narrows the scan.
        let field = |name: &str, value: &str| {
            serde_json::to_string(value)
                .map(|v| format!("\"{name}\":{v}"))
                .map_err(|e| StoreError::Invalid(format!("record: {e}")))
        };
        let source = field("source_ref", &candidate.source_ref)?;
        let entity = candidate
            .entity
            .as_deref()
            .map(|e| field("entity", e))
            .transpose()?;
        let sql = format!(
            "SELECT {SEED_COLS} FROM seedbed s
             WHERE instr(s.candidate, ?1) AND (?2 IS NULL OR instr(s.candidate, ?2))
               AND (s.expires_at IS NULL OR ?3 < s.expires_at) AND {SEED_KEPT}
             ORDER BY s.seq"
        );
        query_seeds(&self.conn, &sql, params![source, entity, now])
    }

    /// The items of `ids` whose evidence is a history archive item: a fact whose `source_ref`
    /// starts with [`HISTORY_REF_PREFIX`], a record whose candidate's does. One query.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn history_evidence(&self, ids: &[Indexed]) -> Result<HashSet<Indexed>, StoreError> {
        let mut facts: Vec<&str> = Vec::new();
        let mut seeds: Vec<i64> = Vec::new();
        for id in ids {
            match id {
                Indexed::Fact(f) => facts.push(&f.0),
                Indexed::Seed(seq) => seeds.push(*seq),
            }
        }
        let mut out = HashSet::new();
        if facts.is_empty() && seeds.is_empty() {
            return Ok(out);
        }
        // SQLite is built without JSON functions; serde writes the candidate's field as
        // `"source_ref":"archive:...`, which a quote escaped inside another string never forms.
        let seed_marker = format!("\"source_ref\":\"{HISTORY_REF_PREFIX}");
        let list = |n: usize, from: usize| {
            (from..from + n)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        let sql = format!(
            "SELECT id, NULL FROM facts WHERE id IN ({}) AND substr(source_ref, 1, ?1) = ?2
             UNION ALL
             SELECT NULL, seq FROM seedbed WHERE seq IN ({}) AND instr(candidate, ?3)",
            list(facts.len(), 4),
            list(seeds.len(), 4 + facts.len()),
        );
        let mut values: Vec<rusqlite::types::Value> = vec![
            i64::try_from(HISTORY_REF_PREFIX.len())
                .unwrap_or(i64::MAX)
                .into(),
            HISTORY_REF_PREFIX.to_owned().into(),
            seed_marker.into(),
        ];
        values.extend(facts.iter().map(|f| (*f).to_owned().into()));
        values.extend(seeds.iter().map(|s| (*s).into()));
        let mut st = self.conn.prepare(&sql)?;
        let mut rows = st.query(rusqlite::params_from_iter(values))?;
        while let Some(row) = rows.next()? {
            match (
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<i64>>(1)?,
            ) {
                (Some(f), _) => out.insert(Indexed::Fact(FactId(f))),
                (None, Some(seq)) => out.insert(Indexed::Seed(seq)),
                (None, None) => false,
            };
        }
        Ok(out)
    }

    /// The first `limit` unprocessed records not forgotten written at or before `until`, oldest
    /// first.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn unprocessed_seeds(
        &self,
        until: UnixTime,
        limit: usize,
    ) -> Result<Vec<SeedRecord>, StoreError> {
        self.unprocessed_seeds_after(0, until, limit)
    }

    /// As [`Store::unprocessed_seeds`], among the records whose `seq` is greater than `after`.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn unprocessed_seeds_after(
        &self,
        after: i64,
        until: UnixTime,
        limit: usize,
    ) -> Result<Vec<SeedRecord>, StoreError> {
        let sql = format!(
            "SELECT {SEED_COLS} FROM seedbed s
             WHERE s.processed_at IS NULL AND s.written_at <= ?1 AND s.seq > ?3 AND {SEED_KEPT}
             ORDER BY s.written_at, s.seq LIMIT ?2"
        );
        query_seeds(
            &self.conn,
            &sql,
            params![until, i64::try_from(limit).unwrap_or(i64::MAX), after],
        )
    }

    /// Marks the record `seq` processed at `at` with the edge's answers `outcome`; once only.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown record; [`StoreError::Invalid`] for one already
    /// processed or an `at` before its writing.
    pub fn mark_seed_processed(
        &mut self,
        seq: i64,
        at: UnixTime,
        outcome: &[c7::ProposeResponse],
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(outcome)
            .map_err(|e| StoreError::Invalid(format!("record {seq}: {e}")))?;
        let tx = Unit::begin(&self.conn)?;
        let row: Option<(UnixTime, Option<UnixTime>)> = tx
            .query_row(
                "SELECT written_at, processed_at FROM seedbed WHERE seq = ?1",
                [seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            None => return Err(StoreError::NotFound(format!("record {seq}"))),
            Some((_, Some(done))) => {
                return invalid(format!("record {seq}: already processed at {done}"));
            }
            Some((written, None)) if at < written => {
                return invalid(format!(
                    "record {seq}: processed at {at}, before it was written at {written}"
                ));
            }
            Some(_) => {}
        }
        tx.execute(
            "UPDATE seedbed SET processed_at = ?2, outcome = ?3 WHERE seq = ?1",
            params![seq, at, json],
        )?;
        tx.commit()
    }

    /// Forgets `refs` at `at` for `reason` by the owner's command `provenance`, in one unit of
    /// work, and answers per ref the marks and queue rows written. A record without a live mark
    /// gets one. Every current fact a ref reaches ([`ForgetRef`]) that is not waiting in the
    /// queue under that ref is queued with the time and reason of the record's live mark, else
    /// `at` and `reason`.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] with the ref of an unknown record; SQLite errors. Nothing is
    /// written then.
    pub fn forget(
        &mut self,
        refs: &[ForgetRef],
        reason: &str,
        provenance: &str,
        at: UnixTime,
    ) -> Result<Vec<Changed>, StoreError> {
        let tx = Unit::begin(&self.conn)?;
        let mut out = Vec::with_capacity(refs.len());
        for r in refs {
            let (marks, marked_at, why) = match r {
                ForgetRef::Seed(seq) => {
                    known_seed(&tx, *seq)?;
                    let live: Option<(UnixTime, String)> = tx
                        .query_row(
                            "SELECT at, reason FROM seed_marks WHERE seed = ?1 AND lifted_at IS NULL",
                            [seq],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    if let Some((marked_at, why)) = live {
                        (0, marked_at, why)
                    } else {
                        tx.execute(
                            "INSERT INTO seed_marks (seed, at, reason, provenance)
                             VALUES (?1, ?2, ?3, ?4)",
                            params![seq, at, reason, provenance],
                        )?;
                        (1, at, reason.to_owned())
                    }
                }
                ForgetRef::Journal(_) => (0, at, reason.to_owned()),
            };
            let queue = tx.execute(
                "INSERT OR IGNORE INTO forget_queue (fact_id, ref, marked_at, reason, queued_at)
                 SELECT f.id, ?1, ?2, ?3, ?4 FROM facts f
                 WHERE f.superseded_at IS NULL
                   AND (f.source_ref = ?1
                     OR f.id IN (SELECT p.fact_id FROM edge_proposal p
                                 WHERE p.seed = ?5 AND p.status = 'accepted'))
                 ORDER BY f.seq",
                params![r.text(), marked_at, why, at, r.seed()],
            )?;
            out.push(Changed { marks, queue });
        }
        tx.commit()?;
        Ok(out)
    }

    /// Brings `refs` back at `at` by the owner's command `provenance`, in one unit of work, and
    /// answers per ref the marks lifted and queue rows removed: lifts the live mark of each
    /// record and removes the rows queued under each ref that the night has not taken.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] with the ref of an unknown record; SQLite errors. Nothing is
    /// changed then.
    pub fn unforget(
        &mut self,
        refs: &[ForgetRef],
        provenance: &str,
        at: UnixTime,
    ) -> Result<Vec<Changed>, StoreError> {
        let tx = Unit::begin(&self.conn)?;
        let mut out = Vec::with_capacity(refs.len());
        for r in refs {
            let marks = match r {
                ForgetRef::Seed(seq) => {
                    known_seed(&tx, *seq)?;
                    tx.execute(
                        "UPDATE seed_marks SET lifted_at = ?2, lifted_provenance = ?3
                         WHERE seed = ?1 AND lifted_at IS NULL",
                        params![seq, at, provenance],
                    )?
                }
                ForgetRef::Journal(_) => 0,
            };
            let queue = tx.execute(
                "DELETE FROM forget_queue WHERE ref = ?1 AND taken_at IS NULL",
                [r.text()],
            )?;
            out.push(Changed { marks, queue });
        }
        tx.commit()?;
        Ok(out)
    }

    /// Whether the record `seq` has a live mark.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn seed_forgotten(&self, seq: i64) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM seed_marks WHERE seed = ?1 AND lifted_at IS NULL",
                [seq],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Every mark of the record `seq`, lifted or not, oldest first.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn seed_marks(&self, seq: i64) -> Result<Vec<SeedMark>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT id, seed, at, reason, provenance, lifted_at, lifted_provenance
             FROM seed_marks WHERE seed = ?1 ORDER BY id",
        )?;
        let rows = st.query_map([seq], |r| {
            let lifted_at: Option<UnixTime> = r.get(5)?;
            let lifted_provenance: Option<String> = r.get(6)?;
            Ok(SeedMark {
                id: r.get(0)?,
                seed: r.get(1)?,
                at: r.get(2)?,
                reason: r.get(3)?,
                provenance: r.get(4)?,
                lifted: lifted_at.zip(lifted_provenance),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every row of the forget queue, taken or not, in the order queued.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn forget_queue(&self) -> Result<Vec<ForgetQueued>, StoreError> {
        query_queue(&self.conn, "ORDER BY id", [])
    }

    /// The first `limit` rows of the forget queue the night has not taken whose id is greater
    /// than `after`, by id.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn forget_waiting(
        &self,
        after: i64,
        limit: usize,
    ) -> Result<Vec<ForgetQueued>, StoreError> {
        query_queue(
            &self.conn,
            "WHERE taken_at IS NULL AND id > ?1 ORDER BY id LIMIT ?2",
            params![after, i64::try_from(limit).unwrap_or(i64::MAX)],
        )
    }

    /// Whether the forget queue holds the row `id`, taken or not.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn forget_row_exists(&self, id: i64) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM forget_queue WHERE id = ?1", [id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Clears the row `id` of the forget queue at `at` in one unit of work (ADR-0051 section
    /// 5): a row already taken changes nothing. Otherwise its fact version, when still
    /// current, is closed on the transaction axis at `at`, or at its recording when that is
    /// later, with no version replacing it; the row is marked taken at `at`, and a re-check
    /// of the version is dropped, so the forgotten fact is not looked up again. The version
    /// keeps its pin, vectors and index row for `as_of`; the worker drops it from the
    /// resident matrix and from `reconcile_queue` as any closed version.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown row; [`StoreError::Corrupt`] for a row whose
    /// fact is missing; SQLite errors. Nothing is changed then.
    pub fn clear_forgotten(&mut self, id: i64, at: UnixTime) -> Result<Cleared, StoreError> {
        let tx = Unit::begin(&self.conn)?;
        let row: Option<(String, Option<UnixTime>)> = tx
            .query_row(
                "SELECT fact_id, taken_at FROM forget_queue WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (fact, taken) =
            row.ok_or_else(|| StoreError::NotFound(format!("forget queue row {id}")))?;
        let fact = FactId(fact);
        if taken.is_some() {
            return Ok(Cleared {
                fact,
                closed: false,
            });
        }
        let f = load_fact(&tx, &fact)?.ok_or_else(|| {
            StoreError::Corrupt(format!("forget queue row {id}: missing fact {}", fact.0))
        })?;
        let closed = f.superseded_at.is_none();
        if closed {
            close(&tx, &f, at.max(f.recorded_at), None)?;
        }
        tx.execute("DELETE FROM rechecks WHERE fact = ?1", [&fact.0])?;
        tx.execute(
            "UPDATE forget_queue SET taken_at = ?2 WHERE id = ?1",
            params![id, at],
        )?;
        tx.commit()?;
        Ok(Cleared { fact, closed })
    }

    /// Stores the embedding of fact `id` under `model`, computed at `computed_at`, replacing an
    /// earlier one.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown fact; [`StoreError::Invalid`] for a secret fact,
    /// an empty or non-finite vector, or a dimension other than the model's earlier vectors.
    pub fn put_vector(
        &mut self,
        id: &FactId,
        model: &str,
        v: &[f32],
        computed_at: UnixTime,
    ) -> Result<(), StoreError> {
        if model.is_empty() {
            return invalid("empty model name");
        }
        if v.is_empty() || v.iter().any(|x| !x.is_finite()) {
            return invalid(format!("fact {}: empty or non-finite vector", id.0));
        }
        let tx = Unit::begin(&self.conn)?;
        let row: Option<(i64, String)> = tx
            .query_row(
                "SELECT seq, sensitivity FROM facts WHERE id = ?1",
                [&id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((seq, sens)) = row else {
            return Err(StoreError::NotFound(format!("fact {}", id.0)));
        };
        if sens == "secret" {
            return invalid(format!("fact {}: secret facts are not embedded", id.0));
        }
        let dim = i64::try_from(v.len()).unwrap_or(i64::MAX);
        let other: Option<i64> = tx
            .query_row(
                "SELECT dim FROM fact_vectors WHERE model = ?1 AND dim != ?2 LIMIT 1",
                params![model, dim],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(d) = other {
            return invalid(format!(
                "model {model}: dimension {dim}, stored vectors have {d}"
            ));
        }
        tx.execute(
            "INSERT INTO fact_vectors (model, fact, dim, v, computed_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(model, fact) DO UPDATE SET dim = ?3, v = ?4, computed_at = ?5",
            params![model, seq, dim, encode_vector(v), computed_at],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Vectors of `model` of the facts that hold at (`valid_at`, `known_at`), by insertion
    /// order. Private facts only with `include_private`; secrets never.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable blob.
    pub fn vectors(
        &self,
        model: &str,
        valid_at: UnixTime,
        known_at: UnixTime,
        include_private: bool,
    ) -> Result<Vec<(FactId, Vec<f32>)>, StoreError> {
        self.vectors_where(model, valid_at, known_at, include_private, None)
    }

    /// [`Store::vectors`] of the facts about `e` only, private ones included.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable blob.
    pub fn vectors_of(
        &self,
        e: &EntityId,
        model: &str,
        valid_at: UnixTime,
        known_at: UnixTime,
    ) -> Result<Vec<(FactId, Vec<f32>)>, StoreError> {
        self.vectors_where(model, valid_at, known_at, true, Some(e))
    }

    fn vectors_where(
        &self,
        model: &str,
        valid_at: UnixTime,
        known_at: UnixTime,
        include_private: bool,
        entity: Option<&EntityId>,
    ) -> Result<Vec<(FactId, Vec<f32>)>, StoreError> {
        let sql = format!(
            "SELECT f.id, v.v FROM fact_vectors v JOIN facts f ON f.seq = v.fact
             WHERE v.model = ?4 AND {HOLDS} AND {VISIBLE} AND (?5 IS NULL OR f.entity = ?5)
             ORDER BY f.seq"
        );
        let entity = entity.map(|e| e.0.as_str());
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(
            params![valid_at, known_at, include_private, model, entity],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )?;
        rows.map(|r| {
            let (id, blob) = r?;
            let v = decode_vector(&blob)
                .ok_or_else(|| StoreError::Corrupt(format!("vector of fact {id}")))?;
            Ok((FactId(id), v))
        })
        .collect()
    }

    /// Non-secret facts without a vector of `model` and the text to embed for each, `"<entity
    /// name>: <fact text>"`: current versions first, then closed versions that held at some
    /// transaction time, each by fewest failed embeddings under `model`, then in insertion
    /// order. A passage that failed [`PASSAGE_TRIES`] times is left out.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn missing_vectors(&self, model: &str) -> Result<Vec<(FactId, String)>, StoreError> {
        self.missing(model, None, i64::MAX)
    }

    /// How much of the store lacks a vector of `model`, for `health`: the non-secret versions a
    /// search can reach (current, and closed ones that held at some transaction time) without
    /// one, skipped passages included, and of them the passages skipped after
    /// [`PASSAGE_TRIES`] failures under `model`.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn vector_gaps(&self, model: &str) -> Result<(u64, u64), StoreError> {
        let (missing, skipped): (i64, i64) = self.conn.query_row(
            "SELECT count(*), coalesce(sum(coalesce(p.failures, 0) >= ?2), 0)
             FROM facts f JOIN entities e ON e.id = f.entity
             LEFT JOIN passage_failures p ON p.model = ?1 AND p.fact = f.seq
             WHERE f.sensitivity != 'secret'
               AND (f.superseded_at IS NULL OR f.recorded_at < f.superseded_at)
               AND NOT EXISTS (SELECT 1 FROM fact_vectors v WHERE v.model = ?1 AND v.fact = f.seq)",
            params![model, PASSAGE_TRIES],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let count = |n: i64| u64::try_from(n).unwrap_or(0);
        Ok((count(missing), count(skipped)))
    }

    /// The first current version of [`Store::missing_vectors`], or with `closed` the first
    /// closed one.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn missing_vector(
        &self,
        model: &str,
        closed: bool,
    ) -> Result<Option<(FactId, String)>, StoreError> {
        Ok(self.missing(model, Some(closed), 1)?.pop())
    }

    fn missing(
        &self,
        model: &str,
        closed: Option<bool>,
        limit: i64,
    ) -> Result<Vec<(FactId, String)>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT f.id, e.name || ': ' || f.text FROM facts f JOIN entities e ON e.id = f.entity
             LEFT JOIN passage_failures p ON p.model = ?1 AND p.fact = f.seq
             WHERE f.sensitivity != 'secret'
               AND (f.superseded_at IS NULL OR f.recorded_at < f.superseded_at)
               AND (?3 IS NULL OR (f.superseded_at IS NOT NULL) = ?3)
               AND NOT EXISTS (SELECT 1 FROM fact_vectors v WHERE v.model = ?1 AND v.fact = f.seq)
               AND coalesce(p.failures, 0) < ?4
             ORDER BY f.superseded_at IS NOT NULL, coalesce(p.failures, 0), f.seq LIMIT ?2",
        )?;
        let rows = st.query_map(params![model, limit, closed, PASSAGE_TRIES], |r| {
            Ok((FactId(r.get(0)?), r.get(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Counts a failed embedding of the passage of `id` under `model`; true when this failure
    /// is its [`PASSAGE_TRIES`]th and the passage is skipped from now on.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown fact; SQLite errors.
    pub fn passage_failed(&mut self, id: &FactId, model: &str) -> Result<bool, StoreError> {
        let tx = Unit::begin(&self.conn)?;
        let failures: Option<i64> = tx
            .query_row(
                "INSERT INTO passage_failures (model, fact, failures)
                 SELECT ?1, seq, 1 FROM facts WHERE id = ?2
                 ON CONFLICT(model, fact) DO UPDATE SET failures = failures + 1
                 RETURNING failures",
                params![model, id.0],
                |r| r.get(0),
            )
            .optional()?;
        let Some(failures) = failures else {
            return Err(StoreError::NotFound(format!("fact {}", id.0)));
        };
        tx.commit()?;
        Ok(failures == PASSAGE_TRIES)
    }

    /// Deletes the vectors and the failed embeddings of every model but `model`; returns how
    /// many vectors went.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn drop_other_vectors(&mut self, model: &str) -> Result<usize, StoreError> {
        let tx = Unit::begin(&self.conn)?;
        let n = tx.execute("DELETE FROM fact_vectors WHERE model != ?1", [model])?;
        tx.execute("DELETE FROM passage_failures WHERE model != ?1", [model])?;
        tx.commit()?;
        Ok(n)
    }

    /// The `seq` of every version with a vector of `model` that is current or closed after
    /// `now`, ascending: the rows of the resident matrix ([`crate::matrix`]).
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn resident_keys(&self, model: &str, now: UnixTime) -> Result<Vec<i64>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT v.fact FROM fact_vectors v JOIN facts f ON f.seq = v.fact
             WHERE v.model = ?1 AND f.sensitivity != 'secret'
               AND (f.superseded_at IS NULL OR f.superseded_at > ?2)
             ORDER BY v.fact",
        )?;
        let rows = st.query_map(params![model, now], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The vectors of `model` of [`Store::resident_keys`] at `now`, or with `seq` only that
    /// version's, with the times and the visibility a search filters them by.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable blob.
    pub fn resident_vectors(
        &self,
        model: &str,
        now: UnixTime,
        seq: Option<i64>,
    ) -> Result<Vec<ResidentVector>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT f.seq, f.id, f.valid_from, f.valid_to, f.recorded_at, f.superseded_at,
                    f.expires_at, f.sensitivity = 'private', v.v
             FROM fact_vectors v JOIN facts f ON f.seq = v.fact
             WHERE v.model = ?1 AND f.sensitivity != 'secret'
               AND (f.superseded_at IS NULL OR f.superseded_at > ?2)
               AND (?3 IS NULL OR v.fact = ?3)
             ORDER BY v.fact",
        )?;
        let rows = st.query_map(params![model, now, seq], |r| {
            Ok((
                ResidentVector {
                    seq: r.get(0)?,
                    fact: FactId(r.get(1)?),
                    times: Times {
                        valid_from: r.get(2)?,
                        valid_to: r.get(3)?,
                        recorded_at: r.get(4)?,
                        superseded_at: r.get(5)?,
                        expires_at: r.get(6)?,
                    },
                    private: r.get(7)?,
                    vector: Vec::new(),
                },
                r.get::<_, Vec<u8>>(8)?,
            ))
        })?;
        rows.map(|r| {
            let (mut row, blob): (ResidentVector, Vec<u8>) = r?;
            row.vector = decode_vector(&blob)
                .ok_or_else(|| StoreError::Corrupt(format!("vector of fact {}", row.fact.0)))?;
            Ok(row)
        })
        .collect()
    }

    /// A value that differs after every commit to the store since the last call, by this
    /// connection or another one.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn revision(&self) -> Result<(i64, u64), StoreError> {
        let others: i64 = self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?;
        Ok((others, self.conn.total_changes()))
    }

    /// Current-belief facts recorded and expired at or before `now`, oldest expiry first.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn expired(&self, now: UnixTime) -> Result<Vec<Fact>, StoreError> {
        let sql = format!(
            "SELECT {FACT_COLS} FROM facts f
             WHERE f.superseded_at IS NULL AND f.expires_at IS NOT NULL AND f.expires_at <= ?1
               AND f.recorded_at <= ?1
             ORDER BY f.expires_at, f.seq"
        );
        query_facts(&self.conn, &sql, params![now])
    }

    /// Closes every fact returned by [`Store::expired`]: the version is superseded at `now`, a
    /// history copy (without expiry) with `valid_to` at the old expiry keeps what was known for
    /// the past, and the old version is queued for re-check ([`Store::rechecks`]). Returns the
    /// ids of the expired versions.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn mark_expired(&mut self, now: UnixTime) -> Result<Vec<FactId>, StoreError> {
        let tx = Unit::begin(&self.conn)?;
        let expired = self.expired(now)?;
        for f in &expired {
            close(&tx, f, now, None)?;
            let end = f
                .expires_at
                .map_or(f.valid_to, |x| Some(f.valid_to.map_or(x, |t| t.min(x))));
            if end.is_none_or(|t| t > f.valid_from) {
                let mut copy = f.clone();
                copy.id = copy_id(&tx, &f.id, now)?;
                copy.valid_to = end;
                copy.recorded_at = now;
                write_copy(&tx, &f.id, copy)?;
            }
            tx.execute(
                "INSERT OR IGNORE INTO rechecks (fact, marked_at) VALUES (?1, ?2)",
                params![f.id.0, now],
            )?;
        }
        tx.commit()?;
        Ok(expired.into_iter().map(|f| f.id).collect())
    }

    /// The `limit` expired versions waiting longest for a re-check at their source, oldest mark
    /// first.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn rechecks(&self, limit: usize) -> Result<Vec<Fact>, StoreError> {
        let sql = format!(
            "SELECT {FACT_COLS} FROM rechecks r JOIN facts f ON f.id = r.fact
             ORDER BY r.marked_at, f.seq LIMIT ?1"
        );
        query_facts(&self.conn, &sql, [i64::try_from(limit).unwrap_or(i64::MAX)])
    }

    /// The earliest `expires_at` of a current-belief fact, if any fact expires.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn next_expiry(&self) -> Result<Option<UnixTime>, StoreError> {
        Ok(self.conn.query_row(
            "SELECT MIN(expires_at) FROM facts
             WHERE superseded_at IS NULL AND expires_at IS NOT NULL",
            [],
            |r| r.get(0),
        )?)
    }

    /// The maintenance pass at `now`: merges the FTS5 index into one segment, lets SQLite
    /// refresh its planner statistics and records `now` as the last pass. Only vectors of a
    /// replaced model are ever deleted, and new vectors reuse their pages, so `VACUUM` is not
    /// run.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn maintain(&mut self, now: UnixTime) -> Result<(), StoreError> {
        let tx = Unit::begin(&self.conn)?;
        tx.execute_batch("INSERT INTO fact_fts (fact_fts) VALUES ('optimize'); PRAGMA optimize;")?;
        tx.execute(
            "INSERT INTO maintenance (id, last_pass) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET last_pass = ?1",
            [now],
        )?;
        tx.commit()
    }

    /// The time of the last committed maintenance pass, if the store ever had one.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn last_maintenance(&self) -> Result<Option<UnixTime>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT last_pass FROM maintenance WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// The last content checks of the model file on record (ADR-0086 open question 15).
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn model_checks(&self) -> Result<ModelChecks, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT passed_at, digest, failed_at, failure FROM model_checks WHERE id = 1",
                [],
                |r| {
                    Ok(ModelChecks {
                        passed: match (r.get(0)?, r.get(1)?) {
                            (Some(at), Some(digest)) => Some((at, digest)),
                            _ => None,
                        },
                        failed: match (r.get(2)?, r.get(3)?) {
                            (Some(at), Some(failure)) => Some((at, failure)),
                            _ => None,
                        },
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    /// Records a check that began at `at` and found the model file sound with its content
    /// `digest`; it clears a failed check on record.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn model_check_passed(&mut self, digest: &str, at: UnixTime) -> Result<(), StoreError> {
        let tx = Unit::begin(&self.conn)?;
        tx.execute(
            "INSERT INTO model_checks (id, passed_at, digest, failed_at, failure)
             VALUES (1, ?1, ?2, NULL, NULL)
             ON CONFLICT(id) DO UPDATE SET passed_at = ?1, digest = ?2, failed_at = NULL,
                                           failure = NULL",
            params![at, digest],
        )?;
        tx.commit()
    }

    /// In one unit of work: records a check that began at `at` and found the model file
    /// corrupted for `failure`, and deletes every vector computed at or after the start of the
    /// last passed check, or every vector computed since they carry a time (`computed_at` above
    /// 0) when none passed. Their facts are without a vector again and are embedded again once
    /// a sound model is back; the vectors computed before stay. Returns how many went.
    ///
    /// The reconciliation decided from those vectors goes with them ([`withdraw_reconciled`]):
    /// the open `contradiction` items it wrote since that start whose fact or rival loses its
    /// vector are withdrawn, and their facts and the reconciled facts that lose their vector
    /// wait in `reconcile_queue` again (ADR-0086 section 8).
    ///
    /// # Errors
    ///
    /// SQLite errors; nothing is written then.
    pub fn model_check_failed(&mut self, failure: &str, at: UnixTime) -> Result<usize, StoreError> {
        let tx = Unit::begin(&self.conn)?;
        let since: Option<UnixTime> = tx
            .query_row("SELECT passed_at FROM model_checks WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
        let since = since.unwrap_or(1).max(1);
        withdraw_reconciled(&tx, since, at)?;
        let n = tx.execute("DELETE FROM fact_vectors WHERE computed_at >= ?1", [since])?;
        tx.execute(
            "INSERT INTO model_checks (id, failed_at, failure) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET failed_at = ?1, failure = ?2",
            params![at, failure],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// Pins the version `id` into the core profile, as first pinned at `pinned_at`.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown id; [`StoreError::Invalid`] for a secret pointer
    /// or a version already pinned.
    pub fn pin(&mut self, id: &FactId, pinned_at: UnixTime) -> Result<(), StoreError> {
        let tx = Unit::begin(&self.conn)?;
        let f =
            load_fact(&tx, id)?.ok_or_else(|| StoreError::NotFound(format!("fact {}", id.0)))?;
        if matches!(f.sensitivity, Sensitivity::Secret { .. }) {
            return invalid(format!("fact {}: a secret pointer is never pinned", id.0));
        }
        if exists(&tx, "SELECT 1 FROM pins WHERE fact = ?1", &id.0)? {
            return invalid(format!("fact {}: already pinned", id.0));
        }
        tx.execute(
            "INSERT INTO pins (fact, pinned_at) VALUES (?1, ?2)",
            params![id.0, pinned_at],
        )?;
        tx.commit()
    }

    /// When the chain of the version `id` was first pinned; `None` when it is not pinned.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn pinned_at(&self, id: &FactId) -> Result<Option<UnixTime>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT pinned_at FROM pins WHERE fact = ?1", [&id.0], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// The earliest first-pin time of the pinned versions of the entity and attribute of `id`
    /// (none for both) that `id` superseded: the pin a new version inherits.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn inherited_pin(&self, id: &FactId) -> Result<Option<UnixTime>, StoreError> {
        Ok(self.conn.query_row(
            "SELECT MIN(p.pinned_at) FROM facts n
             JOIN facts f ON f.entity = n.entity AND f.attribute IS n.attribute
                AND f.superseded_by = n.id
             JOIN pins p ON p.fact = f.id
             WHERE n.id = ?1",
            [&id.0],
            |r| r.get(0),
        )?)
    }

    /// The pinned versions that hold at `now` and are no secret pointers, as their entity's
    /// name and their text, ordered by entity name, case folded, then by first-pin time.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn pinned(&self, now: UnixTime) -> Result<Vec<(String, String)>, StoreError> {
        let sql = format!(
            "SELECT e.name, f.text FROM pins p
             JOIN facts f ON f.id = p.fact JOIN entities e ON e.id = f.entity
             WHERE f.sensitivity <> 'secret' AND {HOLDS}
             ORDER BY e.name_norm, e.name, p.pinned_at, f.seq"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![now, now], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The first time after `now` at which a current pinned version starts, ends or expires:
    /// the set [`Store::pinned`] answers changes then without a write.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn next_pinned_change(&self, now: UnixTime) -> Result<Option<UnixTime>, StoreError> {
        Ok(self.conn.query_row(
            "WITH c AS (
                SELECT f.valid_from, f.valid_to, f.expires_at FROM pins p
                JOIN facts f ON f.id = p.fact
                WHERE f.superseded_at IS NULL AND f.sensitivity <> 'secret'
             )
             SELECT MIN(t) FROM (
                SELECT valid_from AS t FROM c UNION ALL SELECT valid_to FROM c
                UNION ALL SELECT expires_at FROM c
             ) WHERE t > ?1",
            [now],
            |r| r.get(0),
        )?)
    }

    /// Removes `id` from the re-check queue (the source was checked).
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] if it is not queued.
    pub fn clear_recheck(&mut self, id: &FactId) -> Result<(), StoreError> {
        let tx = Unit::begin(&self.conn)?;
        let n = tx.execute("DELETE FROM rechecks WHERE fact = ?1", [&id.0])?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("re-check of fact {}", id.0)));
        }
        tx.commit()
    }
}

/// Where a unit of work sits: the outermost one is a transaction that takes the write lock at
/// `BEGIN IMMEDIATE`, an inner one a savepoint. A deferred transaction that reads first can fail
/// with `SQLITE_BUSY_SNAPSHOT` on its first write, without waiting, once another connection to
/// the store has committed.
#[derive(Clone, Copy)]
enum Depth {
    Outer,
    Inner,
}

impl Depth {
    fn begin(conn: &Connection) -> Result<Self, StoreError> {
        let depth = if conn.is_autocommit() {
            Self::Outer
        } else {
            Self::Inner
        };
        conn.execute_batch(match depth {
            Self::Outer => "BEGIN IMMEDIATE",
            Self::Inner => "SAVEPOINT unit",
        })?;
        Ok(depth)
    }

    fn end(self, conn: &Connection) -> Result<(), StoreError> {
        Ok(conn.execute_batch(match self {
            Self::Outer => "COMMIT",
            Self::Inner => "RELEASE unit",
        })?)
    }

    fn undo(self, conn: &Connection) -> Result<(), StoreError> {
        // SQLite ends the whole transaction itself on some failures; nothing is left to undo.
        if conn.is_autocommit() {
            return Ok(());
        }
        Ok(conn.execute_batch(match self {
            Self::Outer => "ROLLBACK",
            Self::Inner => "ROLLBACK TO unit; RELEASE unit",
        })?)
    }
}

/// A unit of work of one store method, rolled back when dropped before [`Unit::commit`].
struct Unit<'c> {
    conn: &'c Connection,
    depth: Depth,
    open: bool,
}

impl<'c> Unit<'c> {
    fn begin(conn: &'c Connection) -> Result<Self, StoreError> {
        Ok(Self {
            conn,
            depth: Depth::begin(conn)?,
            open: true,
        })
    }

    fn commit(mut self) -> Result<(), StoreError> {
        self.depth.end(self.conn)?;
        self.open = false;
        Ok(())
    }
}

impl Deref for Unit<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn
    }
}

impl Drop for Unit<'_> {
    fn drop(&mut self) {
        if self.open {
            let _ = self.depth.undo(self.conn);
        }
    }
}

/// The starting domains of the garden, each bound to a Wikidata class, parents first.
///
/// # Errors
///
/// [`StoreError::Corrupt`] if the built-in data does not parse.
pub fn starting_domains() -> Result<Vec<Domain>, StoreError> {
    parse_domains(include_str!("starting_domains.json"))
}

/// Parses domains from a JSON array of `{"slug", "label", "qid"?, "parent"?}` objects.
///
/// # Errors
///
/// [`StoreError::Corrupt`] for malformed JSON, a missing slug or label, or a bad QID.
pub fn parse_domains(json: &str) -> Result<Vec<Domain>, StoreError> {
    let bad = |what: String| StoreError::Corrupt(format!("domains: {what}"));
    let v: Value = serde_json::from_str(json).map_err(|e| bad(e.to_string()))?;
    let rows = v.as_array().ok_or_else(|| bad("not an array".into()))?;
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let field = |k: &str| r.get(k).and_then(Value::as_str).map(str::to_owned);
            let slug = field("slug").ok_or_else(|| bad(format!("row {i}: no slug")))?;
            let label = field("label").ok_or_else(|| bad(format!("row {i}: no label")))?;
            let wikidata = field("qid")
                .map(|q| WikidataQid::parse(&q).map_err(|e| bad(e.to_string())))
                .transpose()?;
            Ok(Domain {
                slug,
                label,
                wikidata,
                parent: field("parent"),
            })
        })
        .collect()
}

/// Most levels of the stored domain hierarchy walked before the chain is taken as corrupt.
const MAX_DOMAIN_DEPTH: usize = 1000;

/// Refuses a cycle among the parents of `ds`, a domain that is its own parent included.
fn check_hierarchy(ds: &[Domain]) -> Result<(), StoreError> {
    let parent = |slug: &str| {
        ds.iter()
            .find(|d| d.slug == slug)
            .and_then(|d| d.parent.as_deref())
    };
    for d in ds {
        let mut chain = vec![d.slug.as_str()];
        while let Some(p) = chain.last().and_then(|s| parent(s)) {
            let looped = chain.contains(&p);
            chain.push(p);
            if looped {
                return invalid(format!("domains: cycle {}", chain.join(" -> ")));
            }
        }
    }
    Ok(())
}

/// Moves the store to the last schema version in one transaction, so a failed migration leaves
/// it as it was.
fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let have: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let want = i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX);
    if have > want {
        return invalid(format!("schema version {have} is newer than {want}"));
    }
    if have == want {
        return Ok(());
    }
    let from = usize::try_from(have).unwrap_or(0);
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(from) {
        tx.execute_batch(sql)?;
        if i + 1 == RENORMALISED {
            renormalise(&tx)?;
        }
    }
    if from < REINDEXED {
        reindex_all(&tx)?;
    }
    tx.execute_batch(&format!("PRAGMA user_version = {want}"))?;
    tx.commit()?;
    Ok(())
}

/// A name as matched: lowercase, with runs of whitespace as one space and none at the ends.
fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Recomputes `name_norm` and `alias_norm` of every stored row with [`norm`].
fn renormalise(tx: &Transaction<'_>) -> Result<(), StoreError> {
    for (table, name, normed) in [
        ("entities", "name", "name_norm"),
        ("entity_aliases", "alias", "alias_norm"),
    ] {
        let rows: Vec<(i64, String)> = tx
            .prepare(&format!("SELECT rowid, {name} FROM {table}"))?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (k, n) in rows {
            tx.execute(
                &format!("UPDATE {table} SET {normed} = ?2 WHERE rowid = ?1"),
                params![k, norm(&n)],
            )?;
        }
    }
    Ok(())
}

/// Fails with [`StoreError::NotFound`] naming `seed:<seq>` unless the record exists.
fn known_seed(c: &Connection, seq: i64) -> Result<(), StoreError> {
    let found = c
        .query_row("SELECT 1 FROM seedbed WHERE seq = ?1", [seq], |_| Ok(()))
        .optional()?;
    found.ok_or_else(|| StoreError::NotFound(seed_ref(seq)))
}

fn exists(c: &Connection, sql: &str, key: &str) -> Result<bool, StoreError> {
    Ok(c.query_row(sql, [key], |_| Ok(())).optional()?.is_some())
}

/// Replaces the aliases of `e` with `aliases`, in order.
fn write_aliases(tx: &Connection, e: &EntityId, aliases: &[String]) -> Result<(), StoreError> {
    tx.execute("DELETE FROM entity_aliases WHERE entity = ?1", [&e.0])?;
    for (i, a) in aliases.iter().enumerate() {
        tx.execute(
            "INSERT INTO entity_aliases (entity, ord, alias, alias_norm) VALUES (?1, ?2, ?3, ?4)",
            params![e.0, i64::try_from(i).unwrap_or(i64::MAX), a, norm(a)],
        )?;
    }
    Ok(())
}

/// Whether an entity id is stored, and whether it was merged away.
#[derive(Debug, PartialEq, Eq)]
enum Standing {
    Unknown,
    Live,
    Merged(String),
}

fn standing(c: &Connection, id: &EntityId) -> Result<Standing, StoreError> {
    let row: Option<Option<String>> = c
        .query_row(
            "SELECT merged_into FROM entities WHERE id = ?1",
            [&id.0],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match row {
        None => Standing::Unknown,
        Some(None) => Standing::Live,
        Some(Some(into)) => Standing::Merged(into),
    })
}

/// The stored entity `id` that was not merged away.
fn live_entity(c: &Connection, id: &EntityId) -> Result<Entity, StoreError> {
    match standing(c, id)? {
        Standing::Unknown => return Err(StoreError::NotFound(format!("entity {}", id.0))),
        Standing::Merged(into) => return invalid(format!("entity {}: merged into {into}", id.0)),
        Standing::Live => {}
    }
    load_entity(c, id)?.ok_or_else(|| StoreError::NotFound(format!("entity {}", id.0)))
}

/// Ids of the other entities of the kind of `e`, not merged away, that one of its names names.
fn namesakes(c: &Connection, e: &Entity) -> Result<Vec<String>, StoreError> {
    let mut st = c.prepare(
        "SELECT id FROM entities WHERE kind = ?1 AND id != ?2 AND merged_into IS NULL
           AND (name_norm = ?3 OR id IN (SELECT entity FROM entity_aliases WHERE alias_norm = ?3))",
    )?;
    let mut ids: Vec<String> = Vec::new();
    for n in std::iter::once(&e.name).chain(&e.aliases) {
        let found = st
            .query_map(params![e.kind.as_str(), e.id.0, norm(n)], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        ids.extend(found);
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Refuses attributes of `e` that name a missing or merged entity or one of the wrong kind.
fn check_refs(c: &Connection, e: &Entity) -> Result<(), StoreError> {
    let refs: Vec<(&str, &EntityId, Option<EntityKind>)> = match &e.attrs {
        Attrs::None | Attrs::Place(_) => Vec::new(),
        Attrs::Preference(p) => vec![
            ("holder", &p.holder, Some(EntityKind::Person)),
            ("object", &p.object, None),
        ],
        Attrs::Visit(v) => std::iter::once(("place", &v.place, Some(EntityKind::Place)))
            .chain(v.with.iter().map(|w| ("with", w, Some(EntityKind::Person))))
            .collect(),
    };
    for (field, target, want) in refs {
        let row: Option<(String, Option<String>)> = c
            .query_row(
                "SELECT kind, merged_into FROM entities WHERE id = ?1",
                [&target.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let at = format!("entity {}: {field} {}", e.id.0, target.0);
        let Some((kind, merged)) = row else {
            return invalid(format!("{at} does not exist"));
        };
        if let Some(into) = merged {
            return invalid(format!("{at} was merged into {into}"));
        }
        let kind = EntityKind::parse(&kind)
            .ok_or_else(|| StoreError::Corrupt(format!("entity {}: kind", target.0)))?;
        if let Some(want) = want.filter(|w| *w != kind) {
            return invalid(format!("{at} is a {kind}, not a {want}"));
        }
    }
    Ok(())
}

/// Rewrites every attribute that names `from` to name `into`.
fn redirect_refs(tx: &Connection, from: &EntityId, into: &EntityId) -> Result<(), StoreError> {
    let rows: Vec<(String, String)> = tx
        .prepare("SELECT id, attrs FROM entities WHERE attrs IS NOT NULL")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let swap = |e: &mut EntityId| {
        let hit = e == from;
        if hit {
            e.clone_from(into);
        }
        hit
    };
    for (id, raw) in rows {
        let mut attrs =
            decode_attrs(&raw).ok_or_else(|| StoreError::Corrupt(format!("entity {id}: attrs")))?;
        let changed = match &mut attrs {
            Attrs::None | Attrs::Place(_) => false,
            Attrs::Preference(p) => swap(&mut p.holder) | swap(&mut p.object),
            Attrs::Visit(v) => {
                let mut hit = swap(&mut v.place);
                for w in &mut v.with {
                    hit |= swap(w);
                }
                let mut seen = Vec::new();
                v.with.retain(|w| {
                    let fresh = !seen.contains(w);
                    seen.push(w.clone());
                    fresh
                });
                hit
            }
        };
        if changed {
            tx.execute(
                "UPDATE entities SET attrs = ?2 WHERE id = ?1",
                params![id, encode_attrs(&attrs)],
            )?;
        }
    }
    Ok(())
}

/// Whether the valid times of `a` and `b` overlap.
fn overlap(a: &Fact, b: &Fact) -> bool {
    a.valid_to.is_none_or(|t| b.valid_from < t) && b.valid_to.is_none_or(|t| a.valid_from < t)
}

fn insert_version(tx: &Connection, f: &Fact) -> Result<(), StoreError> {
    let id = &f.id.0;
    if exists(tx, "SELECT 1 FROM facts WHERE id = ?1", id)? {
        return invalid(format!("fact {id}: id already exists"));
    }
    match standing(tx, &f.entity)? {
        Standing::Unknown => return invalid(format!("fact {id}: unknown entity {}", f.entity.0)),
        Standing::Merged(into) => {
            return invalid(format!(
                "fact {id}: entity {} was merged into {into}",
                f.entity.0
            ));
        }
        Standing::Live => {}
    }
    if let Some(d) = &f.domain
        && !exists(tx, "SELECT 1 FROM domains WHERE slug = ?1", d)?
    {
        return invalid(format!("fact {id}: unknown domain {d}"));
    }
    let mut clipped = None;
    if let Some(attr) = &f.attribute {
        // A backfilled open-ended version (it starts before a current one, learned later)
        // ends where the later version starts; it never replaces a later valid one.
        if f.valid_to.is_none() {
            let later: Option<UnixTime> = tx.query_row(
                "SELECT min(valid_from) FROM facts
                 WHERE entity = ?1 AND attribute = ?2 AND superseded_at IS NULL
                   AND valid_from > ?3",
                params![f.entity.0, attr, f.valid_from],
                |r| r.get(0),
            )?;
            if let Some(end) = later {
                let mut c = f.clone();
                c.valid_to = Some(end);
                clipped = Some(c);
            }
        }
        let f = clipped.as_ref().unwrap_or(f);
        let sql = format!(
            "SELECT {FACT_COLS} FROM facts f
             WHERE f.entity = ?1 AND f.attribute = ?2 AND f.superseded_at IS NULL
               AND (?4 IS NULL OR f.valid_from < ?4) AND (f.valid_to IS NULL OR f.valid_to > ?3)
             ORDER BY f.seq"
        );
        let old = query_facts(
            tx,
            &sql,
            params![f.entity.0, attr, f.valid_from, f.valid_to],
        )?;
        for o in &old {
            close(tx, o, f.recorded_at, Some(&f.id))?;
            let mut slices = Vec::new();
            if o.valid_from < f.valid_from {
                slices.push((o.valid_from, Some(f.valid_from)));
            }
            if let Some(end) = f.valid_to
                && o.valid_to.is_none_or(|t| t > end)
            {
                slices.push((end, o.valid_to));
            }
            for (from, to) in slices {
                let mut c = o.clone();
                c.id = copy_id(tx, &o.id, f.recorded_at)?;
                c.valid_from = from;
                c.valid_to = to;
                c.recorded_at = f.recorded_at;
                write_copy(tx, &o.id, c)?;
            }
        }
    }
    write_row(tx, clipped.as_ref().unwrap_or(f))
}

/// Sets `superseded_at` of the current version `f`, and `superseded_by` if a version replaces it.
fn close(tx: &Connection, f: &Fact, at: UnixTime, by: Option<&FactId>) -> Result<(), StoreError> {
    if f.superseded_at.is_some() {
        return invalid(format!("fact {}: already superseded", f.id.0));
    }
    if at < f.recorded_at {
        return invalid(format!(
            "fact {}: transaction time {at} precedes its recording at {}",
            f.id.0, f.recorded_at
        ));
    }
    tx.execute(
        "UPDATE facts SET superseded_at = ?2, superseded_by = ?3
         WHERE id = ?1 AND superseded_at IS NULL",
        params![f.id.0, at, by.map(|b| b.0.as_str())],
    )?;
    Ok(())
}

/// A fresh id for a closed copy of `id` written at `at`.
fn copy_id(tx: &Connection, id: &FactId, at: UnixTime) -> Result<FactId, StoreError> {
    let base = format!("{}~{at}", id.0);
    let mut cand = base.clone();
    let mut n = 1;
    while exists(tx, "SELECT 1 FROM facts WHERE id = ?1", &cand)? {
        n += 1;
        cand = format!("{base}.{n}");
    }
    Ok(FactId(cand))
}

/// Writes `c`, a closed copy of version `from`, with the vectors and the pin of `from`; returns
/// its id. A copy that is history when recorded (`valid_to` at or before `recorded_at`) has no
/// expiry.
fn write_copy(tx: &Connection, from: &FactId, mut c: Fact) -> Result<FactId, StoreError> {
    if c.valid_to.is_some_and(|t| t <= c.recorded_at) {
        c.expires_at = None;
    }
    write_row(tx, &c)?;
    copy_vectors(tx, from, &c.id)?;
    tx.execute(
        "INSERT INTO pins (fact, pinned_at) SELECT ?2, pinned_at FROM pins WHERE fact = ?1",
        params![from.0, c.id.0],
    )?;
    Ok(c.id)
}

fn copy_vectors(tx: &Connection, from: &FactId, to: &FactId) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO fact_vectors (model, fact, dim, v, computed_at)
         SELECT v.model, (SELECT seq FROM facts WHERE id = ?2), v.dim, v.v, v.computed_at
         FROM fact_vectors v JOIN facts f ON f.seq = v.fact WHERE f.id = ?1",
        params![from.0, to.0],
    )?;
    Ok(())
}

fn write_row(tx: &Connection, f: &Fact) -> Result<(), StoreError> {
    let (sens, vault) = match &f.sensitivity {
        Sensitivity::Public => ("public", None),
        Sensitivity::Private => ("private", None),
        Sensitivity::Secret { vault_ref } => ("secret", Some(vault_ref.as_str())),
    };
    tx.execute(
        "INSERT INTO facts (id, entity, text, domain, attribute, ttl_kind, valid_from, valid_to,
             recorded_at, superseded_at, source_kind, source_ref, observed_at, expires_at,
             sensitivity, vault_ref, superseded_by, quote, proof_count, confidence)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
             ?19, ?20)",
        params![
            f.id.0,
            f.entity.0,
            f.text,
            f.domain,
            f.attribute,
            f.ttl.as_str(),
            f.valid_from,
            f.valid_to,
            f.recorded_at,
            f.superseded_at,
            source_kind_str(f.provenance.kind),
            f.provenance.source_ref,
            f.provenance.observed_at,
            f.expires_at,
            sens,
            vault,
            f.superseded_by.as_ref().map(|b| b.0.as_str()),
            f.provenance.quote,
            f.provenance.proof_count,
            f64::from(f.confidence)
        ],
    )?;
    if vault.is_none() {
        index_row(
            tx,
            tx.last_insert_rowid(),
            &f.text,
            &entity_names(tx, &f.entity)?,
        )?;
    }
    Ok(())
}

/// Name and aliases of an entity as one FTS column value.
fn entity_names(c: &Connection, e: &EntityId) -> Result<String, StoreError> {
    Ok(c.query_row(
        "SELECT e.name || coalesce(' ' || (SELECT group_concat(alias, ' ')
             FROM (SELECT alias FROM entity_aliases WHERE entity = e.id ORDER BY ord)), '')
         FROM entities e WHERE e.id = ?1",
        [&e.0],
        |r| r.get(0),
    )?)
}

/// Rewrites the index rows of the facts about `e` and of the unprocessed records resolved to it
/// with its current name and aliases.
fn reindex_entity(tx: &Connection, e: &EntityId) -> Result<(), StoreError> {
    let names = entity_names(tx, e)?;
    let rows: Vec<(i64, String)> = tx
        .prepare("SELECT seq, text FROM facts WHERE entity = ?1 AND sensitivity != 'secret'")?
        .query_map([&e.0], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (seq, text) in rows {
        tx.execute("DELETE FROM fact_fts WHERE rowid = ?1", [seq])?;
        index_row(tx, seq, &text, &names)?;
    }
    reindex_seeds(tx, e, e)
}

/// Rewrites the index rows of the unprocessed non-secret records resolved to `of` with the
/// names of `named_by`. Processed records keep theirs: they answer only requests before their
/// processing.
fn reindex_seeds(tx: &Connection, of: &EntityId, named_by: &EntityId) -> Result<(), StoreError> {
    let sql = format!(
        "SELECT {SEED_COLS} FROM seedbed s
         WHERE s.processed_at IS NULL AND s.entity = ?1 AND s.sensitivity != 'secret'"
    );
    for r in query_seeds(tx, &sql, [&of.0])? {
        tx.execute("DELETE FROM fact_fts WHERE rowid = ?1", [-r.seq])?;
        index_seed(tx, r.seq, &r.candidate, Some(named_by))?;
    }
    Ok(())
}

/// Indexes every non-secret version and record into an empty `fact_fts`.
fn reindex_all(tx: &Connection) -> Result<(), StoreError> {
    let rows: Vec<(i64, String, String)> = tx
        .prepare("SELECT seq, text, entity FROM facts WHERE sensitivity != 'secret'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut names: HashMap<String, String> = HashMap::new();
    for (seq, text, entity) in rows {
        if !names.contains_key(&entity) {
            let n = entity_names(tx, &EntityId(entity.clone()))?;
            names.insert(entity.clone(), n);
        }
        index_row(tx, seq, &text, &names[&entity])?;
    }
    let sql = format!("SELECT {SEED_COLS} FROM seedbed s WHERE s.sensitivity != 'secret'");
    for r in query_seeds(tx, &sql, [])? {
        index_seed(tx, r.seq, &r.candidate, r.entity.as_ref())?;
    }
    Ok(())
}

/// Writes the `fact_fts` row `-seq` of a record: its text, and as names the entity name as
/// written and the resolved entity's name and aliases.
fn index_seed(
    tx: &Connection,
    seq: i64,
    c: &c7::ProposeRequest,
    entity: Option<&EntityId>,
) -> Result<(), StoreError> {
    let resolved = entity.map(|e| entity_names(tx, e)).transpose()?;
    let names: Vec<&str> = c
        .entity
        .iter()
        .chain(&resolved)
        .map(String::as_str)
        .collect();
    index_row(tx, -seq, &c.fact, &names.join(" "))
}

/// Writes the `fact_fts` row `rowid` from a text and its names: both as written, the stems of
/// their Russian tokens and the transliteration of all their tokens.
fn index_row(tx: &Connection, rowid: i64, text: &str, names: &str) -> Result<(), StoreError> {
    let stems: Vec<String> = translit::tokens(text)
        .iter()
        .chain(&translit::tokens(names))
        .filter(|t| morph::is_russian(t))
        .map(|t| morph::stem(t))
        .collect();
    let spelled = format!("{} {}", translit::key(text), translit::key(names));
    tx.execute(
        "INSERT INTO fact_fts (rowid, text, names, stems, translit) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            rowid,
            fold(text),
            fold(names),
            stems.join(" "),
            spelled.trim()
        ],
    )?;
    Ok(())
}

/// Text as indexed: `ё` becomes `е`, which `unicode61` does not fold.
fn fold(s: &str) -> String {
    s.replace('ё', "е").replace('Ё', "Е")
}

/// Most distinct terms of a lexical query; the rest is ignored. A question has a few dozen
/// words at most, and every term is one more posting list to merge.
const MAX_QUERY_TERMS: usize = 64;

/// FTS5 expression for a free-text query: the first [`MAX_QUERY_TERMS`] distinct tokens
/// ([`translit::tokens`]) against text and names, the stems of the Russian ones against stems
/// and their transliterations against translit, every term a quoted prefix term, all joined by
/// OR; `None` without tokens.
fn match_expression(q: &str) -> Option<String> {
    let mut raw: Vec<String> = Vec::new();
    for t in translit::tokens(q) {
        let t = fold(&t);
        if !raw.contains(&t) {
            raw.push(t);
            if raw.len() == MAX_QUERY_TERMS {
                break;
            }
        }
    }
    let stems = raw
        .iter()
        .filter(|t| morph::is_russian(t))
        .map(|t| morph::stem(t));
    let spelled = raw.iter().map(|t| translit::translit(t));
    let clauses: Vec<String> = [
        ("{text names}", any_of(raw.iter().cloned())),
        ("stems", any_of(stems)),
        ("translit", any_of(spelled)),
    ]
    .into_iter()
    .filter_map(|(cols, terms)| Some(format!("{cols} : ({})", terms?)))
    .collect();
    (!clauses.is_empty()).then(|| clauses.join(" OR "))
}

/// The distinct non-empty `terms` as quoted prefix terms joined by OR; `None` without one.
fn any_of(terms: impl Iterator<Item = String>) -> Option<String> {
    let mut seen: Vec<String> = Vec::new();
    for t in terms.filter(|t| !t.is_empty()) {
        if !seen.contains(&t) {
            seen.push(t);
        }
    }
    let quoted: Vec<String> = seen.iter().map(|t| format!("\"{t}\"*")).collect();
    (!quoted.is_empty()).then(|| quoted.join(" OR "))
}

fn encode_vector(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn decode_vector(b: &[u8]) -> Option<Vec<f32>> {
    if !b.len().is_multiple_of(4) {
        return None;
    }
    Some(
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

fn source_kind_str(k: SourceKind) -> &'static str {
    match k {
        SourceKind::Owner => "owner",
        SourceKind::Raw => "raw",
        SourceKind::Internet => "internet",
        SourceKind::Agent => "agent",
    }
}

fn parse_source_kind(s: &str) -> Option<SourceKind> {
    [
        SourceKind::Owner,
        SourceKind::Raw,
        SourceKind::Internet,
        SourceKind::Agent,
    ]
    .into_iter()
    .find(|k| source_kind_str(*k) == s)
}

fn query_facts(
    c: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
) -> Result<Vec<Fact>, StoreError> {
    let mut st = c.prepare(sql)?;
    let rows = st.query_map(p, raw_fact)?;
    rows.map(|r| decode_fact(r?)).collect()
}

/// The reconciliation decided from the vectors computed at or after `since`, which a failed
/// check of the model file is about to drop, undone at `at` (review E13r, finding 2):
///
/// - an open `contradiction` item that `Edge::reconcile_similar` wrote (it names the reconciled
///   fact in `fact_id`; the edge's own contradictions do not) at or after `since`, whose fact
///   or rival has such a vector, is withdrawn: status `withdrawn`, no longer in the digest, and
///   `resolve` answers it `not_open`; its fact, while current, waits in `reconcile_queue`
///   again with the item's candidate;
/// - every current fact that such a vector belongs to and that the edge queued for
///   reconciliation when it accepted it (accepted by the edge itself, without an attribute and
///   not secret) waits in `reconcile_queue` again, with a candidate made from the fact, since the
///   candidate it was queued with is gone.
///
/// A fact already waiting keeps its row.
fn withdraw_reconciled(c: &Connection, since: UnixTime, at: UnixTime) -> Result<(), StoreError> {
    let dropped: std::collections::HashSet<String> = c
        .prepare(
            "SELECT DISTINCT f.id FROM fact_vectors v JOIN facts f ON f.seq = v.fact
             WHERE v.computed_at >= ?1",
        )?
        .query_map([since], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if dropped.is_empty() {
        return Ok(());
    }
    let mut requeue = c.prepare(
        "INSERT OR IGNORE INTO reconcile_queue (fact, candidate)
         SELECT id, ?2 FROM facts WHERE id = ?1 AND superseded_at IS NULL",
    )?;
    let items: Vec<(i64, String, Option<String>, Option<String>)> = c
        .prepare(
            "SELECT id, fact_id, conflicts_with, candidate FROM edge_proposal
             WHERE status = 'queued' AND reason = 'contradiction' AND fact_id IS NOT NULL
               AND created_at >= ?1
             ORDER BY id",
        )?
        .query_map([since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<_, _>>()?;
    for (id, fact, rival, candidate) in items {
        if !dropped.contains(&fact) && rival.as_ref().is_none_or(|r| !dropped.contains(r)) {
            continue;
        }
        c.execute(
            "UPDATE edge_proposal SET status = 'withdrawn', candidate = NULL,
             entity_candidates = NULL, resolved_at = ?2
             WHERE id = ?1",
            params![id, at],
        )?;
        let candidate = match candidate {
            Some(candidate) => candidate,
            None => match load_fact(c, &FactId(fact.clone()))? {
                Some(f) => candidate_of(&f)?,
                None => continue,
            },
        };
        requeue.execute(params![fact, candidate])?;
    }
    let reconciled: Vec<String> = c
        .prepare(
            "SELECT DISTINCT f.id, f.seq FROM edge_proposal p JOIN facts f ON f.id = p.fact_id
             WHERE p.status = 'accepted' AND p.reason IS NULL AND f.superseded_at IS NULL
               AND f.attribute IS NULL AND f.sensitivity != 'secret'
             ORDER BY f.seq",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for id in reconciled {
        if !dropped.contains(&id) {
            continue;
        }
        if let Some(f) = load_fact(c, &FactId(id))? {
            requeue.execute(params![f.id.0, candidate_of(&f)?])?;
        }
    }
    Ok(())
}

/// A candidate, as C7 JSON, that the edge would write as the fact `f`: its text, its entity by
/// id, its source, times, lifetime type, confidence, domain and privacy.
fn candidate_of(f: &Fact) -> Result<String, StoreError> {
    #[allow(clippy::cast_precision_loss)]
    let time = |t: UnixTime| t as f64;
    let candidate = c7::ProposeRequest {
        fact: f.text.clone(),
        entity: Some(f.entity.0.clone()),
        source_ref: f.provenance.source_ref.clone(),
        valid_from: Some(time(f.valid_from)),
        kind: None,
        domain: f.domain.clone(),
        attribute: None,
        valid_to: f.valid_to.map(time),
        source_kind: serde_json::from_value(Value::from(source_kind_str(f.provenance.kind))).ok(),
        observed_at: Some(time(f.provenance.observed_at)),
        confidence: Some(f64::from(f.confidence)),
        ttl_kind: serde_json::from_value(Value::from(f.ttl.as_str())).ok(),
        secret_ref: None,
        private: matches!(f.sensitivity, Sensitivity::Private).then_some(true),
        pin: None,
    };
    serde_json::to_string(&candidate).map_err(|e| StoreError::Corrupt(format!("{}: {e}", f.id.0)))
}

fn load_fact(c: &Connection, id: &FactId) -> Result<Option<Fact>, StoreError> {
    let sql = format!("SELECT {FACT_COLS} FROM facts f WHERE f.id = ?1");
    Ok(query_facts(c, &sql, [&id.0])?.into_iter().next())
}

/// Columns of one seedbed row before decoding of its JSON and its sensitivity.
struct RawSeed {
    seq: i64,
    candidate: String,
    entity: Option<String>,
    conflicts_with: Option<String>,
    sens: String,
    written_at: UnixTime,
    valid_from: UnixTime,
    valid_to: Option<UnixTime>,
    expires_at: Option<UnixTime>,
    processed_at: Option<UnixTime>,
    outcome: Option<String>,
}

/// The rows of `forget_queue` that `tail`, the query's clauses after `FROM`, selects.
fn query_queue(
    c: &Connection,
    tail: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<ForgetQueued>, StoreError> {
    let mut st = c.prepare(&format!(
        "SELECT id, fact_id, ref, marked_at, reason, queued_at, taken_at
         FROM forget_queue {tail}"
    ))?;
    let rows = st.query_map(args, |r| {
        Ok(ForgetQueued {
            id: r.get(0)?,
            fact: FactId(r.get(1)?),
            r#ref: r.get(2)?,
            marked_at: r.get(3)?,
            reason: r.get(4)?,
            queued_at: r.get(5)?,
            taken_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records of `sql`, which selects [`SEED_COLS`].
fn query_seeds(
    c: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
) -> Result<Vec<SeedRecord>, StoreError> {
    let mut st = c.prepare(sql)?;
    let rows = st.query_map(p, |r| {
        Ok(RawSeed {
            seq: r.get(0)?,
            candidate: r.get(1)?,
            entity: r.get(2)?,
            conflicts_with: r.get(3)?,
            sens: r.get(4)?,
            written_at: r.get(5)?,
            valid_from: r.get(6)?,
            valid_to: r.get(7)?,
            expires_at: r.get(8)?,
            processed_at: r.get(9)?,
            outcome: r.get(10)?,
        })
    })?;
    rows.map(|r| decode_seed(r?)).collect()
}

fn decode_seed(raw: RawSeed) -> Result<SeedRecord, StoreError> {
    let bad = |what: &str| StoreError::Corrupt(format!("record {}: {what}", raw.seq));
    let candidate: c7::ProposeRequest =
        serde_json::from_str(&raw.candidate).map_err(|_| bad("candidate"))?;
    let sensitivity = match (raw.sens.as_str(), &candidate.secret_ref) {
        ("public", None) => Sensitivity::Public,
        ("private", None) => Sensitivity::Private,
        ("secret", Some(r)) => Sensitivity::Secret {
            vault_ref: r.clone(),
        },
        _ => return Err(bad("sensitivity")),
    };
    let processed = match (raw.processed_at, raw.outcome) {
        (None, None) => None,
        (Some(at), Some(o)) => Some((at, serde_json::from_str(&o).map_err(|_| bad("outcome"))?)),
        _ => return Err(bad("processing")),
    };
    Ok(SeedRecord {
        seq: raw.seq,
        candidate,
        entity: raw.entity.map(EntityId),
        conflicts_with: raw.conflicts_with.map(FactId),
        sensitivity,
        written_at: raw.written_at,
        valid_from: raw.valid_from,
        valid_to: raw.valid_to,
        expires_at: raw.expires_at,
        processed,
    })
}

/// Columns of one fact row before decoding of the text-coded enums.
struct RawFact {
    f: Fact,
    ttl: String,
    source: String,
    sens: String,
    vault: Option<String>,
}

fn raw_fact(r: &Row<'_>) -> rusqlite::Result<RawFact> {
    Ok(RawFact {
        f: Fact {
            id: FactId(r.get(0)?),
            entity: EntityId(r.get(1)?),
            text: r.get(2)?,
            domain: r.get(3)?,
            attribute: r.get(4)?,
            ttl: TtlKind::General,
            valid_from: r.get(6)?,
            valid_to: r.get(7)?,
            recorded_at: r.get(8)?,
            superseded_at: r.get(9)?,
            superseded_by: r.get::<_, Option<String>>(16)?.map(FactId),
            provenance: Provenance {
                kind: SourceKind::Owner,
                source_ref: r.get(11)?,
                observed_at: r.get(12)?,
                quote: r.get(17)?,
                proof_count: r.get(18)?,
            },
            expires_at: r.get(13)?,
            sensitivity: Sensitivity::Public,
            #[allow(clippy::cast_possible_truncation)]
            confidence: r.get::<_, f64>(19)? as f32,
        },
        ttl: r.get(5)?,
        source: r.get(10)?,
        sens: r.get(14)?,
        vault: r.get(15)?,
    })
}

fn decode_fact(raw: RawFact) -> Result<Fact, StoreError> {
    let RawFact {
        mut f,
        ttl,
        source,
        sens,
        vault,
    } = raw;
    let bad = |what: &str| StoreError::Corrupt(format!("fact {}: {what}", f.id.0));
    f.ttl = TtlKind::parse(&ttl).ok_or_else(|| bad("ttl kind"))?;
    f.provenance.kind = parse_source_kind(&source).ok_or_else(|| bad("source kind"))?;
    f.sensitivity = match (sens.as_str(), vault) {
        ("public", None) => Sensitivity::Public,
        ("private", None) => Sensitivity::Private,
        ("secret", Some(vault_ref)) => Sensitivity::Secret { vault_ref },
        _ => return Err(bad("sensitivity")),
    };
    Ok(f)
}

fn load_entity(c: &Connection, id: &EntityId) -> Result<Option<Entity>, StoreError> {
    let row: Option<(String, String, Option<String>, Option<String>)> = c
        .query_row(
            "SELECT kind, name, domain, attrs FROM entities WHERE id = ?1",
            [&id.0],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((kind, name, domain, attrs)) = row else {
        return Ok(None);
    };
    let bad = |what: &str| StoreError::Corrupt(format!("entity {}: {what}", id.0));
    let kind = EntityKind::parse(&kind).ok_or_else(|| bad("kind"))?;
    let attrs = match attrs {
        None => Attrs::None,
        Some(s) => decode_attrs(&s).ok_or_else(|| bad("attrs"))?,
    };
    let aliases = c
        .prepare("SELECT alias FROM entity_aliases WHERE entity = ?1 ORDER BY ord")?
        .query_map([&id.0], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(Some(Entity {
        id: id.clone(),
        kind,
        name,
        aliases,
        domain,
        attrs,
    }))
}

fn encode_attrs(a: &Attrs) -> Option<String> {
    let v = match a {
        Attrs::None => return None,
        Attrs::Place(p) => json!({
            "type": "place",
            "osm_id": p.osm_id,
            "lat_lon": p.lat_lon.map(|(a, b)| [a, b]),
        }),
        Attrs::Preference(p) => json!({
            "type": "preference",
            "holder": p.holder.0,
            "object": p.object.0,
            "polarity": match p.polarity { Polarity::Like => "like", Polarity::Dislike => "dislike" },
            "weight": p.weight,
            "hardness": match p.hardness { Hardness::Hard => "hard", Hardness::Soft => "soft" },
        }),
        Attrs::Visit(v) => json!({
            "type": "visit",
            "place": v.place.0,
            "at": v.at,
            "rating": v.rating,
            "with": v.with.iter().map(|e| e.0.as_str()).collect::<Vec<_>>(),
        }),
    };
    Some(v.to_string())
}

fn decode_attrs(s: &str) -> Option<Attrs> {
    let v: Value = serde_json::from_str(s).ok()?;
    let str_of = |k: &str| v.get(k)?.as_str().map(str::to_owned);
    match v.get("type")?.as_str()? {
        "place" => {
            let lat_lon = match v.get("lat_lon")? {
                Value::Null => None,
                Value::Array(a) if a.len() == 2 => Some((a[0].as_f64()?, a[1].as_f64()?)),
                _ => return None,
            };
            Some(Attrs::Place(PlaceAttrs {
                osm_id: str_of("osm_id"),
                lat_lon,
            }))
        }
        "preference" => {
            #[allow(clippy::cast_possible_truncation)]
            let weight = v.get("weight")?.as_f64()? as f32;
            Some(Attrs::Preference(PreferenceAttrs {
                holder: EntityId(str_of("holder")?),
                object: EntityId(str_of("object")?),
                polarity: match v.get("polarity")?.as_str()? {
                    "like" => Polarity::Like,
                    "dislike" => Polarity::Dislike,
                    _ => return None,
                },
                weight,
                hardness: match v.get("hardness")?.as_str()? {
                    "hard" => Hardness::Hard,
                    "soft" => Hardness::Soft,
                    _ => return None,
                },
            }))
        }
        "visit" => {
            let rating = match v.get("rating")? {
                Value::Null => None,
                r => Some(u8::try_from(r.as_u64()?).ok()?),
            };
            let with = v
                .get("with")?
                .as_array()?
                .iter()
                .map(|e| e.as_str().map(|s| EntityId(s.to_owned())))
                .collect::<Option<_>>()?;
            Some(Attrs::Visit(VisitAttrs {
                place: EntityId(str_of("place")?),
                at: v.get("at")?.as_i64()?,
                rating,
                with,
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_expression_quotes_prefix_terms_per_column() {
        assert_eq!(
            match_expression("Где живёт Anna? anna \"x\"").as_deref(),
            Some(
                "{text names} : (\"где\"* OR \"живет\"* OR \"anna\"* OR \"x\"*) \
                 OR stems : (\"где\"* OR \"живет\"*) \
                 OR translit : (\"gde\"* OR \"zivet\"* OR \"anna\"* OR \"x\"*)"
            )
        );
        assert_eq!(
            match_expression("стоматологи ъ").as_deref(),
            Some(
                "{text names} : (\"стоматологи\"* OR \"ъ\"*) OR stems : (\"стоматолог\"* OR \"ъ\"*) \
                 OR translit : (\"stomatologi\"*)"
            )
        );
        assert_eq!(match_expression(" ?! "), None);
        let long = (0..1000)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let e = match_expression(&long).unwrap();
        assert_eq!(e.matches("\"*").count(), 2 * MAX_QUERY_TERMS);
    }

    #[test]
    fn the_refill_of_the_index_restores_facts_and_records_but_no_secret_pointer() {
        let mut s = Store::open_in_memory().unwrap();
        let mira = EntityId("p-mira".into());
        s.upsert_entity(&Entity {
            id: mira.clone(),
            kind: EntityKind::Person,
            name: "Mira Velde".into(),
            aliases: vec!["Mirka".into()],
            domain: None,
            attrs: Attrs::None,
        })
        .unwrap();
        let fact = Fact {
            id: FactId("f1".into()),
            entity: mira.clone(),
            text: "Mira plays the cello.".into(),
            domain: None,
            attribute: None,
            ttl: TtlKind::Personal,
            valid_from: 0,
            valid_to: None,
            recorded_at: 10,
            superseded_at: None,
            superseded_by: None,
            provenance: Provenance {
                kind: SourceKind::Owner,
                source_ref: "msg:1".into(),
                observed_at: 10,
                quote: None,
                proof_count: 1,
            },
            expires_at: None,
            sensitivity: Sensitivity::Public,
            confidence: 1.0,
        };
        s.insert_fact(&fact).unwrap();
        let candidate = |text: &str, secret_ref: Option<&str>| c7::ProposeRequest {
            fact: text.into(),
            entity: Some("M. Velde".into()),
            source_ref: "msg:2".into(),
            valid_from: None,
            kind: None,
            domain: None,
            attribute: None,
            valid_to: None,
            source_kind: Some(c7::SourceKind::Owner),
            observed_at: None,
            confidence: None,
            ttl_kind: None,
            secret_ref: secret_ref.map(str::to_owned),
            private: None,
            pin: None,
        };
        let seq = s
            .write_seed(
                &candidate("Bought new cello strings.", None),
                Some(&mira),
                None,
                10,
            )
            .unwrap();
        s.write_seed(
            &candidate(
                "The cello case code is in the vault.",
                Some("vault:mira/case"),
            ),
            None,
            None,
            10,
        )
        .unwrap();
        let found = |s: &Store, q: &str| -> Vec<Indexed> {
            s.lexical(q, 10, 10, 10, true)
                .unwrap()
                .into_iter()
                .map(|(h, _)| h)
                .collect()
        };
        let before = found(&s, "cello");
        s.conn()
            .execute_batch("INSERT INTO fact_fts (fact_fts) VALUES ('delete-all')")
            .unwrap();
        assert_eq!(found(&s, "cello"), []);
        reindex_all(s.conn()).unwrap();
        assert_eq!(found(&s, "cello"), before);
        assert_eq!(before.len(), 2);
        assert!(before.contains(&Indexed::Seed(seq)));
        // Names as written and the resolved entity's alias.
        assert_eq!(
            found(&s, "strings Velde"),
            [Indexed::Seed(seq), Indexed::Fact(fact.id)]
        );
        assert_eq!(found(&s, "Mirka strings")[0], Indexed::Seed(seq));
        let rows: i64 = s
            .conn()
            .query_row("SELECT count(*) FROM fact_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 2);
    }

    #[test]
    fn vector_blob_round_trip() {
        let v = [1.0_f32, -0.5, 3.25];
        assert_eq!(decode_vector(&encode_vector(&v)).as_deref(), Some(&v[..]));
        assert_eq!(decode_vector(&[0, 1, 2]), None);
    }
}
