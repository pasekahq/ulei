//! The archive store of C10 (ADR-0051 section 2): one SQLite file holding every item's original
//! bytes, deflated when that is shorter, its entries, its FTS5 text and the audit of deletes.
//! `put` only adds rows; `delete` is the only operation that removes any, and it writes a
//! `deletions` row that is never removed. Triggers refuse every update of a stored row.

mod query;

use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use miniz_oxide::deflate::compress_to_vec;
use miniz_oxide::inflate::decompress_to_vec_with_limit;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};
use sha2::{Digest, Sha256};
use ulei_contracts::generated::c10::{
    Deletion, Entry, ErrorCode, Hit, PutResponse, PutStatus, SearchRequest,
};

/// `PRAGMA user_version` of the schema this code writes.
pub const SCHEMA_VERSION: i64 = 1;

/// Longest `external_id` in UTF-8 bytes.
pub const MAX_EXTERNAL_ID_BYTES: usize = 512;

/// Hits of a search without `limit`.
pub const DEFAULT_LIMIT: u64 = 10;

/// Most hits a search may ask for.
pub const MAX_LIMIT: u64 = 50;

const DEFLATE_LEVEL: u8 = 6;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const PRAGMAS: &str = "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; \
    PRAGMA cache_size = -256; PRAGMA mmap_size = 0; PRAGMA temp_store = FILE; \
    PRAGMA foreign_keys = ON;";

// `blobs.row` aliases the rowid so `VACUUM` cannot renumber it under the `item_fts` rowids.
const SCHEMA: &str = "
CREATE TABLE blobs (
  row INTEGER PRIMARY KEY,
  id TEXT NOT NULL UNIQUE,
  len INTEGER NOT NULL,
  codec TEXT NOT NULL CHECK (codec IN ('deflate', 'none')),
  body BLOB NOT NULL);
CREATE TABLE entries (
  source TEXT NOT NULL,
  external_id TEXT NOT NULL,
  id TEXT NOT NULL REFERENCES blobs (id),
  who TEXT,
  at REAL,
  kind TEXT NOT NULL,
  put_at REAL NOT NULL,
  UNIQUE (source, external_id));
CREATE INDEX entries_id ON entries (id);
CREATE VIRTUAL TABLE item_fts USING fts5(text, tokenize = 'unicode61 remove_diacritics 2');
CREATE TABLE cursors (
  source TEXT PRIMARY KEY,
  cursor INTEGER NOT NULL);
CREATE TABLE deletions (
  row INTEGER PRIMARY KEY,
  deleted_at REAL NOT NULL,
  id TEXT NOT NULL,
  len INTEGER NOT NULL,
  entries TEXT NOT NULL,
  reason TEXT NOT NULL,
  pid INTEGER NOT NULL,
  uid INTEGER NOT NULL);
CREATE TRIGGER blobs_immutable BEFORE UPDATE ON blobs
  BEGIN SELECT RAISE(ABORT, 'archived rows are immutable'); END;
CREATE TRIGGER entries_immutable BEFORE UPDATE ON entries
  BEGIN SELECT RAISE(ABORT, 'archived rows are immutable'); END;
CREATE TRIGGER deletions_immutable BEFORE UPDATE ON deletions
  BEGIN SELECT RAISE(ABORT, 'the audit is never changed'); END;
CREATE TRIGGER deletions_kept BEFORE DELETE ON deletions
  BEGIN SELECT RAISE(ABORT, 'the audit is never removed'); END;
";

const ENTRY_COLUMNS: &str = "source, external_id, who, at, kind, put_at";

/// Condition on an entry `e` of the item `id_expr` passing the optional filters `?1` source,
/// `?2` who, `?3` kind, `?4` since and `?5` until; a NULL `at` fails every time filter.
fn entry_filter(id_expr: &str) -> String {
    format!(
        "e.id = {id_expr} AND (?1 IS NULL OR e.source = ?1) \
         AND (?2 IS NULL OR e.who = ?2) AND (?3 IS NULL OR e.kind = ?3) \
         AND (?4 IS NULL OR e.at >= ?4) AND (?5 IS NULL OR e.at <= ?5)"
    )
}

/// One record to archive: the original bytes and the metadata of its entry.
#[derive(Debug, Clone, Copy)]
pub struct Item<'a> {
    /// The putter, `^[a-z][a-z0-9_-]{0,63}$`.
    pub source: &'a str,
    /// The source's id of the record, 1 to [`MAX_EXTERNAL_ID_BYTES`] bytes.
    pub external_id: &'a str,
    /// Author as the source names it.
    pub who: Option<&'a str>,
    /// Unix time of the original record.
    pub at: Option<f64>,
    /// Media type of `bytes`.
    pub kind: &'a str,
    /// The text the archive indexes.
    pub text: &'a str,
    /// The original.
    pub bytes: &'a [u8],
}

/// An item's original as `read` answers it.
#[derive(Debug, Clone, PartialEq)]
pub struct Original {
    /// The original bytes, checked against the item's id.
    pub bytes: Vec<u8>,
    /// Media type of the first entry.
    pub kind: String,
    /// Length of the original in bytes.
    pub len: u64,
    /// Every entry of the item, oldest first.
    pub entries: Vec<Entry>,
}

/// Who asked for a delete, as the socket's peer credentials name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    /// The caller's pid.
    pub pid: u64,
    /// The caller's uid.
    pub uid: u64,
}

/// Why an archive operation failed.
#[derive(Debug)]
pub enum Error {
    /// The input breaks the C10 method input; the text names the field.
    Invalid(String),
    /// No item has this id.
    NotFound(String),
    /// The `(source, external_id)` is stored with the item `stored`.
    Conflict {
        /// Id of the item the pair already names.
        stored: String,
    },
    /// A stored body does not give back the bytes of its id.
    Corrupt(String),
    /// The file has a schema version this code does not know.
    SchemaVersion(i64),
    /// The file or its directory lets another user in.
    OpenMode {
        /// The file or the directory.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The file or its directory could not be created, or the clock is before the epoch.
    Io(io::Error),
    /// The audit row of a delete could not be encoded or decoded.
    Json(serde_json::Error),
    /// SQLite failed.
    Sqlite(rusqlite::Error),
}

impl Error {
    /// The C10 error code the daemon answers with.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Invalid(_) => ErrorCode::InvalidRequest,
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::Conflict { .. } => ErrorCode::Conflict,
            Self::Corrupt(_)
            | Self::SchemaVersion(_)
            | Self::OpenMode { .. }
            | Self::Io(_)
            | Self::Json(_)
            | Self::Sqlite(_) => ErrorCode::StoreFailed,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(why) => write!(f, "invalid request: {why}"),
            Self::NotFound(id) => write!(f, "no item {id}"),
            Self::Conflict { stored } => {
                write!(
                    f,
                    "the source's record is stored with other bytes ({stored})"
                )
            }
            Self::Corrupt(why) => write!(f, "corrupt item: {why}"),
            Self::SchemaVersion(v) => write!(f, "archive schema version {v} is not supported"),
            Self::OpenMode { path, mode } => write!(
                f,
                "{} has mode {mode:04o}, other users must have no access",
                path.display()
            ),
            Self::Io(e) => write!(f, "archive file: {e}"),
            Self::Json(e) => write!(f, "audit row: {e}"),
            Self::Sqlite(e) => write!(f, "archive store: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// An open archive file.
#[derive(Debug)]
pub struct Archive {
    conn: Connection,
}

impl Archive {
    /// Opens the archive at `path`, creating a missing directory 0700, a missing file 0600 and
    /// its schema; the file runs in WAL mode. A directory or file other users can reach is
    /// refused.
    ///
    /// # Errors
    /// [`Error::Io`], [`Error::OpenMode`], [`Error::SchemaVersion`] for a file of an unknown
    /// version, or [`Error::Sqlite`].
    pub fn open(path: &Path) -> Result<Self, Error> {
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(Error::Io)?;
        owner_only(dir)?;
        // `create_new` never follows a symlink; an existing file is left to the NOFOLLOW open.
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(Error::Io(e)),
            _ => {}
        }
        owner_only(path)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut conn = Connection::open_with_flags(path, flags)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch(PRAGMAS)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match tx.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? {
            0 => {
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            found => return Err(Error::SchemaVersion(found)),
        }
        tx.commit()?;
        Ok(Self { conn })
    }

    /// Archives `item`: its bytes as a new item unless their id is stored, and a new entry
    /// unless its `(source, external_id)` is stored, all in one transaction. A stored pair
    /// with the same id is answered `duplicate` and changes nothing.
    ///
    /// # Errors
    /// [`Error::Invalid`], [`Error::Conflict`] when the pair is stored with another id,
    /// [`Error::Io`] for the clock, or [`Error::Sqlite`].
    pub fn put(&mut self, item: &Item<'_>) -> Result<PutResponse, Error> {
        check_source(item.source)?;
        check_external_id(item.external_id)?;
        check_time("at", item.at)?;
        check_non_empty("kind", item.kind)?;
        let id = hex(&Sha256::digest(item.bytes));
        let put_at = now()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored: Option<String> = tx
            .query_row(
                "SELECT id FROM entries WHERE source = ?1 AND external_id = ?2",
                params![item.source, item.external_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(stored) = stored {
            return if stored == id {
                Ok(PutResponse {
                    id,
                    status: PutStatus::Duplicate,
                })
            } else {
                Err(Error::Conflict { stored })
            };
        }
        let known = tx
            .query_row("SELECT 1 FROM blobs WHERE id = ?1", [&id], |_| Ok(()))
            .optional()?
            .is_some();
        if !known {
            insert_blob(&tx, &id, item)?;
        }
        tx.execute(
            &format!(
                "INSERT INTO entries (id, {ENTRY_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
            ),
            params![
                &id,
                item.source,
                item.external_id,
                item.who,
                item.at,
                item.kind,
                put_at
            ],
        )?;
        tx.commit()?;
        Ok(PutResponse {
            id,
            status: PutStatus::Stored,
        })
    }

    /// Items whose text matches `request.q` and which have an entry passing every filter, best
    /// first, each with those entries and a snippet. The query is sanitized as the reference
    /// does it; one FTS5 still rejects finds nothing.
    ///
    /// # Errors
    /// [`Error::Invalid`] or [`Error::Sqlite`].
    pub fn search(&self, request: &SearchRequest) -> Result<Vec<Hit>, Error> {
        check_non_empty("q", &request.q)?;
        if let Some(source) = &request.source {
            check_source(source)?;
        }
        if let Some(who) = &request.who {
            check_non_empty("who", who)?;
        }
        if let Some(kind) = &request.kind {
            check_non_empty("kind", kind)?;
        }
        check_time("since", request.since)?;
        check_time("until", request.until)?;
        let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(Error::Invalid(format!("limit must be 1 to {MAX_LIMIT}")));
        }
        let expr = query::fts5_query(&request.q);
        if expr.is_empty() {
            return Ok(Vec::new());
        }
        let filters = (
            request.source.as_deref(),
            request.who.as_deref(),
            request.kind.as_deref(),
            request.since,
            request.until,
        );
        let mut st = self.conn.prepare(&format!(
            "SELECT b.id, snippet(item_fts, 0, '>>>', '<<<', '...', 40) \
             FROM item_fts JOIN blobs b ON b.row = item_fts.rowid \
             WHERE item_fts MATCH ?6 \
               AND EXISTS (SELECT 1 FROM entries e WHERE {}) \
             ORDER BY item_fts.rank, b.row LIMIT ?7",
            entry_filter("b.id")
        ))?;
        let rows = st.query_map(
            params![
                filters.0,
                filters.1,
                filters.2,
                filters.3,
                filters.4,
                expr,
                int(limit)?
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        let found = match rows.collect::<Result<Vec<_>, _>>() {
            Ok(found) => found,
            // FTS5 parses the expression while stepping and reports a rejected one as a plain
            // SQLITE_ERROR; the reference answers it with no hits.
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::Unknown => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };
        let mut st = self.conn.prepare(&format!(
            "SELECT {ENTRY_COLUMNS} FROM entries e WHERE {} ORDER BY e.put_at, e.rowid",
            entry_filter("?6")
        ))?;
        found
            .into_iter()
            .map(|(id, snippet)| {
                let entries = st
                    .query_map(
                        params![filters.0, filters.1, filters.2, filters.3, filters.4, &id],
                        entry,
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Hit {
                    id,
                    entries,
                    snippet,
                })
            })
            .collect()
    }

    /// The original of the item `id`, inflated and checked against `id`, with its entries.
    ///
    /// # Errors
    /// [`Error::Invalid`] for a malformed id, [`Error::NotFound`], [`Error::Corrupt`] when the
    /// stored body does not give back bytes of that id, or [`Error::Sqlite`].
    pub fn read(&self, id: &str) -> Result<Original, Error> {
        check_id(id)?;
        let (len, codec, body): (u64, String, Vec<u8>) = self
            .conn
            .query_row(
                "SELECT len, codec, body FROM blobs WHERE id = ?1",
                [id],
                |r| Ok((get_u64(r, 0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(id.to_owned()))?;
        let bytes = original(id, len, &codec, body)?;
        let entries = entries_of(&self.conn, id)?;
        let kind = entries
            .first()
            .map(|e| e.kind.clone())
            .ok_or_else(|| Error::Corrupt(format!("item {id} has no entry")))?;
        Ok(Original {
            bytes,
            kind,
            len,
            entries,
        })
    }

    /// Removes the item `id` with its FTS5 row and entries, and in the same transaction writes
    /// the `deletions` row it answers.
    ///
    /// # Errors
    /// [`Error::Invalid`] for a malformed id or an empty reason, [`Error::NotFound`],
    /// [`Error::Io`] for the clock, [`Error::Json`] or [`Error::Sqlite`].
    pub fn delete(&mut self, id: &str, reason: &str, caller: Caller) -> Result<Deletion, Error> {
        check_id(id)?;
        check_non_empty("reason", reason)?;
        let deleted_at = now()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (row, len): (i64, u64) = tx
            .query_row("SELECT row, len FROM blobs WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, get_u64(r, 1)?))
            })
            .optional()?
            .ok_or_else(|| Error::NotFound(id.to_owned()))?;
        let entries = entries_of(&tx, id)?;
        tx.execute("DELETE FROM item_fts WHERE rowid = ?1", [row])?;
        tx.execute("DELETE FROM entries WHERE id = ?1", [id])?;
        tx.execute("DELETE FROM blobs WHERE row = ?1", [row])?;
        tx.execute(
            "INSERT INTO deletions (deleted_at, id, len, entries, reason, pid, uid) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                deleted_at,
                id,
                int(len)?,
                serde_json::to_string(&entries)?,
                reason,
                int(caller.pid)?,
                int(caller.uid)?
            ],
        )?;
        tx.commit()?;
        Ok(Deletion {
            deleted_at,
            id: id.to_owned(),
            len,
            entries,
            reason: reason.to_owned(),
            pid: caller.pid,
            uid: caller.uid,
        })
    }

    /// Every audit row, oldest first.
    ///
    /// # Errors
    /// [`Error::Json`] or [`Error::Sqlite`].
    pub fn deletions(&self) -> Result<Vec<Deletion>, Error> {
        let mut st = self.conn.prepare(
            "SELECT deleted_at, id, len, entries, reason, pid, uid FROM deletions ORDER BY row",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, f64>(0)?,
                r.get::<_, String>(1)?,
                get_u64(r, 2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                get_u64(r, 5)?,
                get_u64(r, 6)?,
            ))
        })?;
        rows.map(|row| {
            let (deleted_at, id, len, entries, reason, pid, uid) = row?;
            Ok(Deletion {
                deleted_at,
                id,
                len,
                entries: serde_json::from_str(&entries)?,
                reason,
                pid,
                uid,
            })
        })
        .collect()
    }

    /// Items stored.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn items(&self) -> Result<u64, Error> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM blobs", [], |r| get_u64(r, 0))?)
    }
}

/// Refuses `path` when its group or other permission bits are set.
fn owner_only(path: &Path) -> Result<(), Error> {
    let mode = fs::metadata(path).map_err(Error::Io)?.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(Error::OpenMode {
            path: path.to_owned(),
            mode,
        });
    }
    Ok(())
}

/// Writes the blob of `item` under `id`, deflated when that is shorter, and its FTS5 row.
fn insert_blob(tx: &Transaction<'_>, id: &str, item: &Item<'_>) -> Result<(), Error> {
    let deflated = compress_to_vec(item.bytes, DEFLATE_LEVEL);
    let (codec, body) = if deflated.len() < item.bytes.len() {
        ("deflate", deflated.as_slice())
    } else {
        ("none", item.bytes)
    };
    tx.execute(
        "INSERT INTO blobs (id, len, codec, body) VALUES (?1, ?2, ?3, ?4)",
        params![id, int(item.bytes.len() as u64)?, codec, body],
    )?;
    tx.execute(
        "INSERT INTO item_fts (rowid, text) VALUES (?1, ?2)",
        params![tx.last_insert_rowid(), item.text],
    )?;
    Ok(())
}

/// The bytes a stored body gives back, refused unless they are `len` bytes hashing to `id`.
fn original(id: &str, len: u64, codec: &str, body: Vec<u8>) -> Result<Vec<u8>, Error> {
    let corrupt = |why: String| Error::Corrupt(format!("item {id}: {why}"));
    let bytes = match codec {
        "none" => body,
        "deflate" => {
            let limit = usize::try_from(len).map_err(|e| corrupt(e.to_string()))?;
            decompress_to_vec_with_limit(&body, limit)
                .map_err(|e| corrupt(format!("inflate: {e}")))?
        }
        other => return Err(corrupt(format!("unknown codec {other}"))),
    };
    if bytes.len() as u64 != len {
        return Err(corrupt(format!(
            "{} bytes, stored length {len}",
            bytes.len()
        )));
    }
    if hex(&Sha256::digest(&bytes)) != id {
        return Err(corrupt("SHA-256 does not match".to_owned()));
    }
    Ok(bytes)
}

/// Every entry of the item `id`, oldest first.
fn entries_of(conn: &Connection, id: &str) -> Result<Vec<Entry>, Error> {
    let mut st = conn.prepare(&format!(
        "SELECT {ENTRY_COLUMNS} FROM entries WHERE id = ?1 ORDER BY put_at, rowid"
    ))?;
    let rows = st.query_map([id], entry)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// `n` as an SQLite integer.
fn int(n: u64) -> Result<i64, Error> {
    i64::try_from(n).map_err(|_| Error::Invalid(format!("{n} is above the SQLite integer range")))
}

/// Column `i` of `r` as a non-negative integer.
fn get_u64(r: &Row<'_>, i: usize) -> rusqlite::Result<u64> {
    let v: i64 = r.get(i)?;
    u64::try_from(v).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(i, v))
}

fn entry(r: &Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        source: r.get(0)?,
        external_id: r.get(1)?,
        who: r.get(2)?,
        at: r.get(3)?,
        kind: r.get(4)?,
        put_at: r.get(5)?,
    })
}

fn check_source(source: &str) -> Result<(), Error> {
    let mut chars = source.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && source.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(
            "source must match ^[a-z][a-z0-9_-]{0,63}$".to_owned(),
        ))
    }
}

fn check_external_id(external_id: &str) -> Result<(), Error> {
    if external_id.is_empty() || external_id.len() > MAX_EXTERNAL_ID_BYTES {
        return Err(Error::Invalid(format!(
            "external_id must be 1 to {MAX_EXTERNAL_ID_BYTES} bytes of UTF-8"
        )));
    }
    Ok(())
}

fn check_id(id: &str) -> Result<(), Error> {
    if id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(())
    } else {
        Err(Error::Invalid(
            "id must be 64 lowercase hex digits".to_owned(),
        ))
    }
}

fn check_time(field: &str, time: Option<f64>) -> Result<(), Error> {
    match time {
        Some(t) if !(t.is_finite() && t >= 0.0) => Err(Error::Invalid(format!(
            "{field} must be a Unix time at or after 0"
        ))),
        _ => Ok(()),
    }
}

fn check_non_empty(field: &str, value: &str) -> Result<(), Error> {
    if value.is_empty() {
        Err(Error::Invalid(format!("{field} must not be empty")))
    } else {
        Ok(())
    }
}

fn now() -> Result<f64, Error> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .map_err(|e| Error::Io(io::Error::other(e)))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}
