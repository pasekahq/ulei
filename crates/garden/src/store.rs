//! SQLite storage of the garden: domains, entities with aliases, fact versions on two time
//! axes, an FTS5 index and embedding vectors. ADR-0043.
//!
//! Rows of `facts` are append-only; the only in-place change is closing `superseded_at`. A new
//! version of a single-valued attribute supersedes the overlapping current versions and writes
//! closed copies of the parts of their validity it does not cover. Secret facts never enter the
//! FTS index or the vector table; they are reached only through their entity
//! ([`Store::find_entities`] then [`Store::facts_of`]).

use std::fmt;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde_json::{Value, json};

use crate::model::{
    Attrs, Domain, Entity, EntityId, EntityKind, Fact, FactId, Hardness, PlaceAttrs, Polarity,
    PreferenceAttrs, Provenance, Sensitivity, SourceKind, Ttl, TtlKind, TtlTable, UnixTime,
    VisitAttrs, WikidataQid,
};

/// Schema migrations; entry `i` moves `PRAGMA user_version` from `i` to `i + 1`.
const MIGRATIONS: &[&str] = &[r"
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
"];

const FACT_COLS: &str = "f.id, f.entity, f.text, f.domain, f.attribute, f.ttl_kind, \
    f.valid_from, f.valid_to, f.recorded_at, f.superseded_at, f.source_kind, f.source_ref, \
    f.observed_at, f.expires_at, f.sensitivity, f.vault_ref";

/// SQL form of [`Fact::holds`] over alias `f` with `?1` = `valid_at` and `?2` = `known_at`.
const HOLDS: &str = "f.valid_from <= ?1 AND (f.valid_to IS NULL OR ?1 < f.valid_to) \
    AND f.recorded_at <= ?2 AND (f.superseded_at IS NULL OR ?2 < f.superseded_at) \
    AND (f.expires_at IS NULL OR ?2 < f.expires_at)";

/// Visibility filter with `?3` = `include_private`; secrets never pass.
const VISIBLE: &str = "(f.sensitivity = 'public' OR (?3 AND f.sensitivity = 'private'))";

/// Longest identifier of a fact, an entity or a domain, in bytes.
const MAX_ID: usize = 200;

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

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            _ => None,
        }
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

impl Store {
    /// Opens or creates the database at `path` in WAL mode and migrates it.
    ///
    /// # Errors
    ///
    /// SQLite errors, or [`StoreError::Invalid`] for a schema newer than this code.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 5000;",
        )?;
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

    /// The TTL table used to fill missing expiries and reject never-cached facts.
    pub fn ttl_table(&self) -> &TtlTable {
        &self.ttl
    }

    /// Replaces the TTL table (a config override).
    pub fn set_ttl_table(&mut self, t: TtlTable) {
        self.ttl = t;
    }

    /// Inserts or replaces a domain; its parent must already exist.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a bad slug, an empty label or an unknown parent.
    pub fn upsert_domain(&mut self, d: &Domain) -> Result<(), StoreError> {
        check_slug(&d.slug)?;
        if d.label.trim().is_empty() {
            return invalid(format!("domain {}: empty label", d.slug));
        }
        let tx = self.conn.transaction()?;
        if let Some(p) = &d.parent
            && (p == &d.slug || !exists(&tx, "SELECT 1 FROM domains WHERE slug = ?1", p)?)
        {
            return invalid(format!("domain {}: unknown parent {p}", d.slug));
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
    /// stored ones are left as they are. Returns how many were inserted.
    ///
    /// # Errors
    ///
    /// As [`Store::upsert_domain`]; nothing is inserted then.
    pub fn seed_domains(&mut self, ds: &[Domain]) -> Result<usize, StoreError> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        for d in ds {
            check_slug(&d.slug)?;
            if d.label.trim().is_empty() {
                return invalid(format!("domain {}: empty label", d.slug));
            }
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
    /// names.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a bad id, an empty name, an unknown domain or attributes
    /// that do not fit the kind.
    pub fn upsert_entity(&mut self, e: &Entity) -> Result<(), StoreError> {
        check_id("entity", &e.id.0)?;
        if e.name.trim().is_empty() {
            return invalid(format!("entity {}: empty name", e.id.0));
        }
        let fits = matches!(
            (e.kind, &e.attrs),
            (_, Attrs::None)
                | (EntityKind::Place, Attrs::Place(_))
                | (EntityKind::Preference, Attrs::Preference(_))
                | (EntityKind::Visit, Attrs::Visit(_))
        );
        if !fits {
            return invalid(format!(
                "entity {}: attributes do not fit kind {}",
                e.id.0, e.kind
            ));
        }
        if let Attrs::Preference(p) = &e.attrs
            && !(0.0..=1.0).contains(&p.weight)
        {
            return invalid(format!("entity {}: weight outside [0, 1]", e.id.0));
        }
        if let Attrs::Visit(v) = &e.attrs
            && v.rating.is_some_and(|r| !(1..=5).contains(&r))
        {
            return invalid(format!("entity {}: rating outside 1..=5", e.id.0));
        }
        let tx = self.conn.transaction()?;
        if let Some(d) = &e.domain
            && !exists(&tx, "SELECT 1 FROM domains WHERE slug = ?1", d)?
        {
            return invalid(format!("entity {}: unknown domain {d}", e.id.0));
        }
        let attrs = encode_attrs(&e.attrs);
        tx.execute(
            "INSERT INTO entities (id, kind, name, name_norm, domain, attrs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET kind = ?2, name = ?3, name_norm = ?4, domain = ?5,
                 attrs = ?6",
            params![
                e.id.0,
                e.kind.as_str(),
                e.name,
                norm(&e.name),
                e.domain,
                attrs
            ],
        )?;
        tx.execute("DELETE FROM entity_aliases WHERE entity = ?1", [&e.id.0])?;
        for (i, a) in e.aliases.iter().enumerate() {
            tx.execute(
                "INSERT INTO entity_aliases (entity, ord, alias, alias_norm) VALUES (?1, ?2, ?3, ?4)",
                params![e.id.0, i64::try_from(i).unwrap_or(i64::MAX), a, norm(a)],
            )?;
        }
        reindex_entity(&tx, &e.id)?;
        tx.commit()?;
        Ok(())
    }

    /// The entity with `id`, if any.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn entity(&self, id: &EntityId) -> Result<Option<Entity>, StoreError> {
        load_entity(&self.conn, id)
    }

    /// Entities whose name or an alias equals `name_or_alias` ignoring case and surrounding
    /// spaces, by id.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn find_entities(&self, name_or_alias: &str) -> Result<Vec<Entity>, StoreError> {
        let ids: Vec<String> = self
            .conn
            .prepare(
                "SELECT id FROM entities WHERE name_norm = ?1
                 UNION SELECT entity FROM entity_aliases WHERE alias_norm = ?1
                 ORDER BY 1",
            )?
            .query_map([norm(name_or_alias)], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        ids.into_iter()
            .filter_map(|id| load_entity(&self.conn, &EntityId(id)).transpose())
            .collect()
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
    /// [`StoreError::Invalid`] for an empty id, text or provenance ref, a duplicate id, an
    /// unknown entity or domain, an inverted interval, a never-cached kind, a secret without a
    /// vault reference or with a text that may carry the value, or a version older than the
    /// one it would replace.
    pub fn insert_fact(&mut self, f: &Fact) -> Result<(), StoreError> {
        let f = self.prepare_fact(f)?;
        let tx = self.conn.transaction()?;
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
        let tx = self.conn.transaction()?;
        for f in &facts {
            insert_version(&tx, f)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn prepare_fact(&self, f: &Fact) -> Result<Fact, StoreError> {
        validate_fact(f)?;
        if self.ttl.rule(f.ttl).ttl == Ttl::NeverCache {
            return invalid(format!("fact {}: kind {} is never cached", f.id.0, f.ttl));
        }
        let mut f = f.clone();
        if f.expires_at.is_none() {
            f.expires_at = self.ttl.expiry(f.ttl, f.provenance.observed_at);
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
        let tx = self.conn.transaction()?;
        let f =
            load_fact(&tx, id)?.ok_or_else(|| StoreError::NotFound(format!("fact {}", id.0)))?;
        close(&tx, &f, known_at)?;
        tx.commit()?;
        Ok(())
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

    /// FTS5 search over fact text and the entity's name and aliases: the best `k` facts that
    /// hold at (`valid_at`, `known_at`), best first, score = -bm25 (higher is better). Private
    /// facts only with `include_private`; secrets never.
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
    ) -> Result<Vec<(FactId, f32)>, StoreError> {
        let Some(expr) = match_expression(q) else {
            return Ok(Vec::new());
        };
        let k = i64::try_from(k).unwrap_or(i64::MAX);
        let sql = format!(
            "SELECT f.id, bm25(fact_fts) AS rank FROM fact_fts JOIN facts f ON f.seq = fact_fts.rowid
             WHERE fact_fts MATCH ?4 AND {HOLDS} AND {VISIBLE}
             ORDER BY rank, f.seq LIMIT ?5"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![valid_at, known_at, include_private, expr, k], |r| {
            #[allow(clippy::cast_possible_truncation)]
            let score = -r.get::<_, f64>(1)? as f32;
            Ok((FactId(r.get(0)?), score))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Stores the embedding of fact `id` under `model`, replacing an earlier one.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for an unknown fact; [`StoreError::Invalid`] for a secret fact,
    /// an empty or non-finite vector, or a dimension other than the model's earlier vectors.
    pub fn put_vector(&mut self, id: &FactId, model: &str, v: &[f32]) -> Result<(), StoreError> {
        if model.is_empty() {
            return invalid("empty model name");
        }
        if v.is_empty() || v.iter().any(|x| !x.is_finite()) {
            return invalid(format!("fact {}: empty or non-finite vector", id.0));
        }
        let tx = self.conn.transaction()?;
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
            "INSERT INTO fact_vectors (model, fact, dim, v) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(model, fact) DO UPDATE SET dim = ?3, v = ?4",
            params![model, seq, dim, encode_vector(v)],
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
        let sql = format!(
            "SELECT f.id, v.v FROM fact_vectors v JOIN facts f ON f.seq = v.fact
             WHERE v.model = ?4 AND {HOLDS} AND {VISIBLE} ORDER BY f.seq"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![valid_at, known_at, include_private, model], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        rows.map(|r| {
            let (id, blob) = r?;
            let v = decode_vector(&blob)
                .ok_or_else(|| StoreError::Corrupt(format!("vector of fact {id}")))?;
            Ok((FactId(id), v))
        })
        .collect()
    }

    /// Current non-secret facts without a vector of `model` and the text to embed for each:
    /// `"<entity name>: <fact text>"`.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn missing_vectors(&self, model: &str) -> Result<Vec<(FactId, String)>, StoreError> {
        let mut st = self.conn.prepare(
            "SELECT f.id, e.name || ': ' || f.text FROM facts f JOIN entities e ON e.id = f.entity
             WHERE f.superseded_at IS NULL AND f.sensitivity != 'secret'
               AND NOT EXISTS (SELECT 1 FROM fact_vectors v WHERE v.model = ?1 AND v.fact = f.seq)
             ORDER BY f.seq",
        )?;
        let rows = st.query_map([model], |r| Ok((FactId(r.get(0)?), r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Current-belief facts whose expiry is at or before `now`, oldest expiry first.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn expired(&self, now: UnixTime) -> Result<Vec<Fact>, StoreError> {
        let sql = format!(
            "SELECT {FACT_COLS} FROM facts f
             WHERE f.superseded_at IS NULL AND f.expires_at IS NOT NULL AND f.expires_at <= ?1
             ORDER BY f.expires_at, f.seq"
        );
        query_facts(&self.conn, &sql, params![now])
    }

    /// Closes every fact returned by [`Store::expired`]: the version is superseded at `now`, a
    /// copy without expiry and with `valid_to` at the old expiry keeps what was known for the
    /// past, and the old version is queued for re-check ([`Store::rechecks`]). Returns the
    /// ids of the expired versions.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn mark_expired(&mut self, now: UnixTime) -> Result<Vec<FactId>, StoreError> {
        let expired = self.expired(now)?;
        let tx = self.conn.transaction()?;
        for f in &expired {
            close(&tx, f, now)?;
            let end = f
                .expires_at
                .map_or(f.valid_to, |x| Some(f.valid_to.map_or(x, |t| t.min(x))));
            if end.is_none_or(|t| t > f.valid_from) {
                let mut copy = f.clone();
                copy.id = copy_id(&tx, &f.id, now)?;
                copy.valid_to = end;
                copy.recorded_at = now;
                copy.expires_at = None;
                write_row(&tx, &copy)?;
                copy_vectors(&tx, &f.id, &copy.id)?;
            }
            tx.execute(
                "INSERT OR IGNORE INTO rechecks (fact, marked_at) VALUES (?1, ?2)",
                params![f.id.0, now],
            )?;
        }
        tx.commit()?;
        Ok(expired.into_iter().map(|f| f.id).collect())
    }

    /// Expired versions waiting for a re-check at their source, oldest mark first.
    ///
    /// # Errors
    ///
    /// SQLite errors or an undecodable row.
    pub fn rechecks(&self) -> Result<Vec<Fact>, StoreError> {
        let sql = format!(
            "SELECT {FACT_COLS} FROM rechecks r JOIN facts f ON f.id = r.fact
             ORDER BY r.marked_at, f.seq"
        );
        query_facts(&self.conn, &sql, [])
    }

    /// Removes `id` from the re-check queue (the source was checked).
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] if it is not queued.
    pub fn clear_recheck(&mut self, id: &FactId) -> Result<(), StoreError> {
        let n = self
            .conn
            .execute("DELETE FROM rechecks WHERE fact = ?1", [&id.0])?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("re-check of fact {}", id.0)));
        }
        Ok(())
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

fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let have: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let want = i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX);
    if have > want {
        return invalid(format!("schema version {have} is newer than {want}"));
    }
    for (i, sql) in MIGRATIONS
        .iter()
        .enumerate()
        .skip(usize::try_from(have).unwrap_or(0))
    {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        tx.commit()?;
    }
    Ok(())
}

fn check_id(what: &str, id: &str) -> Result<(), StoreError> {
    if id.trim().is_empty() || id.len() > MAX_ID || id.chars().any(char::is_control) {
        return invalid(format!("bad {what} id {id:?}"));
    }
    Ok(())
}

fn check_slug(s: &str) -> Result<(), StoreError> {
    let ok = !s.is_empty()
        && s.len() <= MAX_ID
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        invalid(format!("bad domain slug {s:?}"))
    }
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

fn exists(c: &Connection, sql: &str, key: &str) -> Result<bool, StoreError> {
    Ok(c.query_row(sql, [key], |_| Ok(())).optional()?.is_some())
}

fn validate_fact(f: &Fact) -> Result<(), StoreError> {
    check_id("fact", &f.id.0)?;
    let id = &f.id.0;
    if f.text.trim().is_empty() {
        return invalid(format!("fact {id}: empty text"));
    }
    if f.provenance.source_ref.trim().is_empty() {
        return invalid(format!("fact {id}: empty provenance ref"));
    }
    if f.valid_to.is_some_and(|t| t <= f.valid_from) {
        return invalid(format!("fact {id}: valid_to is not after valid_from"));
    }
    if f.superseded_at.is_some() {
        return invalid(format!("fact {id}: a new version cannot be superseded"));
    }
    if f.attribute.as_ref().is_some_and(|a| a.trim().is_empty()) {
        return invalid(format!("fact {id}: empty attribute"));
    }
    if let Sensitivity::Secret { vault_ref } = &f.sensitivity {
        if vault_ref.trim().is_empty() {
            return invalid(format!("fact {id}: secret without a vault reference"));
        }
        if may_carry_secret(&f.text) {
            return invalid(format!(
                "fact {id}: the text of a secret fact may carry its value; say only that it exists"
            ));
        }
    }
    Ok(())
}

/// Whether `text` looks like it carries a secret value: a run of four or more digits (numbers,
/// PINs, card and document numbers) or a word of 16 or more characters mixing letters and
/// digits (keys, tokens).
fn may_carry_secret(text: &str) -> bool {
    let mut digits = 0;
    for c in text.chars() {
        digits = if c.is_ascii_digit() { digits + 1 } else { 0 };
        if digits >= 4 {
            return true;
        }
    }
    text.split_whitespace().any(|w| {
        w.chars().count() >= 16
            && w.chars().any(|c| c.is_ascii_digit())
            && w.chars().any(char::is_alphabetic)
    })
}

fn insert_version(tx: &Transaction<'_>, f: &Fact) -> Result<(), StoreError> {
    let id = &f.id.0;
    if exists(tx, "SELECT 1 FROM facts WHERE id = ?1", id)? {
        return invalid(format!("fact {id}: id already exists"));
    }
    if !exists(tx, "SELECT 1 FROM entities WHERE id = ?1", &f.entity.0)? {
        return invalid(format!("fact {id}: unknown entity {}", f.entity.0));
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
            close(tx, o, f.recorded_at)?;
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
                write_row(tx, &c)?;
                copy_vectors(tx, &o.id, &c.id)?;
            }
        }
    }
    write_row(tx, clipped.as_ref().unwrap_or(f))
}

/// Sets `superseded_at` of the current version `f`.
fn close(tx: &Transaction<'_>, f: &Fact, at: UnixTime) -> Result<(), StoreError> {
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
        "UPDATE facts SET superseded_at = ?2 WHERE id = ?1 AND superseded_at IS NULL",
        params![f.id.0, at],
    )?;
    Ok(())
}

/// A fresh id for a closed copy of `id` written at `at`.
fn copy_id(tx: &Transaction<'_>, id: &FactId, at: UnixTime) -> Result<FactId, StoreError> {
    let base = format!("{}~{at}", id.0);
    let mut cand = base.clone();
    let mut n = 1;
    while exists(tx, "SELECT 1 FROM facts WHERE id = ?1", &cand)? {
        n += 1;
        cand = format!("{base}.{n}");
    }
    Ok(FactId(cand))
}

fn copy_vectors(tx: &Transaction<'_>, from: &FactId, to: &FactId) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO fact_vectors (model, fact, dim, v)
         SELECT v.model, (SELECT seq FROM facts WHERE id = ?2), v.dim, v.v
         FROM fact_vectors v JOIN facts f ON f.seq = v.fact WHERE f.id = ?1",
        params![from.0, to.0],
    )?;
    Ok(())
}

fn write_row(tx: &Transaction<'_>, f: &Fact) -> Result<(), StoreError> {
    let (sens, vault) = match &f.sensitivity {
        Sensitivity::Public => ("public", None),
        Sensitivity::Private => ("private", None),
        Sensitivity::Secret { vault_ref } => ("secret", Some(vault_ref.as_str())),
    };
    tx.execute(
        "INSERT INTO facts (id, entity, text, domain, attribute, ttl_kind, valid_from, valid_to,
             recorded_at, superseded_at, source_kind, source_ref, observed_at, expires_at,
             sensitivity, vault_ref)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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
            vault
        ],
    )?;
    if vault.is_none() {
        let seq = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO fact_fts (rowid, text, names) VALUES (?1, ?2, ?3)",
            params![seq, fold(&f.text), fold(&entity_names(tx, &f.entity)?)],
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

fn reindex_entity(tx: &Transaction<'_>, e: &EntityId) -> Result<(), StoreError> {
    let names = fold(&entity_names(tx, e)?);
    let rows: Vec<(i64, String)> = tx
        .prepare("SELECT seq, text FROM facts WHERE entity = ?1 AND sensitivity != 'secret'")?
        .query_map([&e.0], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (seq, text) in rows {
        tx.execute("DELETE FROM fact_fts WHERE rowid = ?1", [seq])?;
        tx.execute(
            "INSERT INTO fact_fts (rowid, text, names) VALUES (?1, ?2, ?3)",
            params![seq, fold(&text), names],
        )?;
    }
    Ok(())
}

/// Text as indexed: `ё` becomes `е`, which `unicode61` does not fold.
fn fold(s: &str) -> String {
    s.replace('ё', "е").replace('Ё', "Е")
}

/// FTS5 expression for a free-text query: each distinct lowercased letter/digit run as a
/// quoted prefix term, joined by OR; `None` without terms.
fn match_expression(q: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for t in q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let t = fold(&t.to_lowercase());
        if !terms.contains(&t) {
            terms.push(t);
        }
    }
    if terms.is_empty() {
        return None;
    }
    let quoted: Vec<String> = terms.iter().map(|t| format!("\"{t}\"*")).collect();
    Some(quoted.join(" OR "))
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

fn load_fact(c: &Connection, id: &FactId) -> Result<Option<Fact>, StoreError> {
    let sql = format!("SELECT {FACT_COLS} FROM facts f WHERE f.id = ?1");
    Ok(query_facts(c, &sql, [&id.0])?.into_iter().next())
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
            provenance: Provenance {
                kind: SourceKind::Owner,
                source_ref: r.get(11)?,
                observed_at: r.get(12)?,
            },
            expires_at: r.get(13)?,
            sensitivity: Sensitivity::Public,
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
    fn match_expression_quotes_prefix_terms() {
        assert_eq!(
            match_expression("Где живёт Anna? anna \"x\"").as_deref(),
            Some("\"где\"* OR \"живет\"* OR \"anna\"* OR \"x\"*")
        );
        assert_eq!(match_expression(" ?! "), None);
    }

    #[test]
    fn secret_heuristic() {
        assert!(may_carry_secret("passport number 4512 334455"));
        assert!(may_carry_secret("api key sk9fj2k3l4m5n6b7v8"));
        assert!(!may_carry_secret("The passport is in the vault."));
    }

    #[test]
    fn vector_blob_round_trip() {
        let v = [1.0_f32, -0.5, 3.25];
        assert_eq!(decode_vector(&encode_vector(&v)).as_deref(), Some(&v[..]));
        assert_eq!(decode_vector(&[0, 1, 2]), None);
    }
}
