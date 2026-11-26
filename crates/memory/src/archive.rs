//! The archive store of C10 (ADR-0051 section 2): one SQLite file holding every item's original
//! bytes, deflated when that is shorter, its entries, its FTS5 text and the audit of deletes.
//! `put` and `pull` only add rows; `delete` is the only operation that removes any, and it writes
//! a `deletions` row that is never removed. Triggers refuse every update of a stored row.
//! An original over one request body goes in parts through an upload (ADR-0076): its parts are
//! stored as they come, checked whole at the upload's end, and read back by range.
//! Each entry has a `seq`, its place in the order the archive stored its source's entries;
//! `list` walks a source by it (ADR-0078).

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
    Deletion, Entry, ErrorCode, Hit, ListRequest, ListResponse, Listed, PutResponse, PutStatus,
    SearchRequest, Skipped,
};

/// `PRAGMA user_version` of the schema this code writes.
pub const SCHEMA_VERSION: i64 = 4;

/// Longest `external_id` in UTF-8 bytes.
pub const MAX_EXTERNAL_ID_BYTES: usize = 512;

/// Hits of a search without `limit`.
pub const DEFAULT_LIMIT: u64 = 10;

/// Most hits a search may ask for.
pub const MAX_LIMIT: u64 = 50;

/// Most characters of an item's text a hit carries: the seedbed lift's bound, which keeps a page
/// of [`MAX_LIMIT`] hits far under the 1 MiB body cap.
pub const HIT_TEXT_CHARS: u64 = 1_200;

/// Entries of a `list` page without `limit`.
pub const DEFAULT_LIST_LIMIT: u64 = 100;

/// Most entries a `list` page may ask for.
pub const MAX_LIST_LIMIT: u64 = 500;

/// Characters of each entry's text a `list` page carries without `text_chars`.
pub const DEFAULT_LIST_TEXT_CHARS: u64 = 4_000;

/// Most characters of each entry's text a `list` page may ask for.
pub const MAX_LIST_TEXT_CHARS: u64 = 16_384;

/// Most bytes of a serialized `list` page; a page ends before an entry that would pass it.
pub const MAX_LIST_BODY: usize = 786_432;

/// Most bytes of the original in one part of an upload.
pub const MAX_PART: usize = 512 * 1024;

/// Most bytes of a range `read` answers.
pub const MAX_RANGE: u64 = 512 * 1024;

/// Longest original an upload takes, in bytes.
pub const MAX_ITEM_LEN: u64 = 16 * 1024 * 1024 * 1024;

/// Most bytes of UTF-8 of an upload's text that are kept and indexed.
pub const MAX_UPLOAD_TEXT: usize = 256 * 1024;

/// An upload expires this long after its last `begin` or `part`.
pub const UPLOAD_TTL: Duration = Duration::from_secs(3_600);

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

/// What version 2 adds to version 1.
const SKIPPED: &str = "
CREATE TABLE skipped (
  row INTEGER PRIMARY KEY,
  source TEXT NOT NULL,
  external_id TEXT NOT NULL,
  len INTEGER NOT NULL,
  at REAL,
  skipped_at REAL NOT NULL,
  UNIQUE (source, external_id));
CREATE TRIGGER skipped_immutable BEFORE UPDATE ON skipped
  BEGIN SELECT RAISE(ABORT, 'skipped references are immutable'); END;
CREATE TRIGGER skipped_kept BEFORE DELETE ON skipped
  BEGIN SELECT RAISE(ABORT, 'skipped references are never removed'); END;
";

/// What version 3 adds to version 2: uploads, their parts, and the stored items made of parts.
/// `parts.owner` is the `uploads.row` the part came in, kept by `chunked` once the item is
/// stored; `AUTOINCREMENT` never gives a row number twice, so it never names two uploads.
const CHUNKS: &str = "
CREATE TABLE uploads (
  row INTEGER PRIMARY KEY AUTOINCREMENT,
  id TEXT NOT NULL,
  len INTEGER NOT NULL,
  source TEXT NOT NULL,
  external_id TEXT NOT NULL,
  who TEXT,
  at REAL,
  kind TEXT NOT NULL,
  text TEXT NOT NULL,
  parts INTEGER NOT NULL,
  received INTEGER NOT NULL,
  touched_at REAL NOT NULL);
CREATE TABLE parts (
  owner INTEGER NOT NULL,
  seq INTEGER NOT NULL,
  start INTEGER NOT NULL,
  len INTEGER NOT NULL,
  sha256 TEXT NOT NULL,
  codec TEXT NOT NULL CHECK (codec IN ('deflate', 'none')),
  body BLOB NOT NULL,
  PRIMARY KEY (owner, seq));
CREATE INDEX parts_start ON parts (owner, start);
CREATE TABLE chunked (
  id TEXT PRIMARY KEY REFERENCES blobs (id),
  owner INTEGER NOT NULL UNIQUE);
CREATE TRIGGER parts_immutable BEFORE UPDATE ON parts
  BEGIN SELECT RAISE(ABORT, 'archived rows are immutable'); END;
CREATE TRIGGER chunked_immutable BEFORE UPDATE ON chunked
  BEGIN SELECT RAISE(ABORT, 'archived rows are immutable'); END;
";

/// What version 4 adds to version 3: the `seq` of every entry, given by `AUTOINCREMENT` and so
/// never twice. The entries stored before are numbered in the order `read` lists them; a trigger
/// numbers each new entry in the transaction that inserts it, and another drops the number of a
/// deleted entry.
const SEQ: &str = "
CREATE TABLE entry_seq (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  source TEXT NOT NULL,
  external_id TEXT NOT NULL,
  UNIQUE (source, external_id));
CREATE INDEX entry_seq_source ON entry_seq (source, seq);
CREATE TRIGGER entry_seq_immutable BEFORE UPDATE ON entry_seq
  BEGIN SELECT RAISE(ABORT, 'archived rows are immutable'); END;
INSERT INTO entry_seq (source, external_id)
  SELECT source, external_id FROM entries ORDER BY put_at, rowid;
CREATE TRIGGER entries_seq AFTER INSERT ON entries
  BEGIN INSERT INTO entry_seq (source, external_id) VALUES (new.source, new.external_id); END;
CREATE TRIGGER entries_seq_gone AFTER DELETE ON entries
  BEGIN DELETE FROM entry_seq WHERE source = old.source AND external_id = old.external_id; END;
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

/// A record a source names without its bytes; the archive keeps the reference forever.
#[derive(Debug, Clone, Copy)]
pub struct Skip<'a> {
    /// The id its item would have had.
    pub external_id: &'a str,
    /// Length of its original in bytes.
    pub len: u64,
    /// Unix time of the original record.
    pub at: Option<f64>,
}

/// One page a source handed over: its items and skipped records, all of `source`, and the
/// position after the page, if the source gave one.
#[derive(Debug, Clone)]
pub struct Page<'a> {
    /// The source every item and skip of the page belongs to.
    pub source: &'a str,
    /// The records with their bytes.
    pub items: Vec<Item<'a>>,
    /// The records without their bytes.
    pub skipped: Vec<Skip<'a>>,
    /// The source's position after the page.
    pub cursor: Option<u64>,
}

/// A record whose original goes in parts: its entry's metadata, the indexed text and the length
/// and id of the original the parts must make.
#[derive(Debug, Clone, Copy)]
pub struct Upload<'a> {
    /// The putter, `^[a-z][a-z0-9_-]{0,63}$`.
    pub source: &'a str,
    /// The source's id of the record, 1 to [`MAX_EXTERNAL_ID_BYTES`] bytes.
    pub external_id: &'a str,
    /// Author as the source names it.
    pub who: Option<&'a str>,
    /// Unix time of the original record.
    pub at: Option<f64>,
    /// Media type of the original.
    pub kind: &'a str,
    /// The text the archive indexes; its first [`MAX_UPLOAD_TEXT`] bytes are kept.
    pub text: &'a str,
    /// Length of the original, 1 to [`MAX_ITEM_LEN`].
    pub len: u64,
    /// Lowercase hex SHA-256 of the original.
    pub id: &'a str,
}

/// What `begin` settled.
#[derive(Debug, Clone, PartialEq)]
pub enum Begun {
    /// The parts are to be sent to this upload.
    Upload(u64),
    /// The entry is settled without any part, as `put` would answer.
    Done(PutResponse),
}

/// An upload's state after a part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Received {
    /// Parts the upload holds.
    pub parts: u64,
    /// Bytes of the original the upload holds.
    pub bytes: u64,
}

/// An item's original as `read` answers it.
#[derive(Debug, Clone, PartialEq)]
pub struct Original {
    /// The original bytes, checked against the item's id, or the range's bytes.
    pub bytes: Vec<u8>,
    /// First byte of the range, when a range was read.
    pub offset: Option<u64>,
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
    /// No item has this id, or no upload this number.
    NotFound(String),
    /// The item was put in parts and is read by range only.
    TooLarge(String),
    /// The `(source, external_id)` is stored with the item `stored`.
    Conflict {
        /// Id of the item the pair already names.
        stored: String,
    },
    /// A stored body does not give back the bytes of its id.
    Corrupt(String),
    /// The file has a schema version this code does not know.
    SchemaVersion(i64),
    /// The file or its directory belongs to another user.
    ForeignOwner {
        /// The file or the directory.
        path: PathBuf,
        /// Its owner.
        uid: u32,
    },
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
            Self::TooLarge(_) => ErrorCode::TooLarge,
            Self::Conflict { .. } => ErrorCode::Conflict,
            Self::Corrupt(_)
            | Self::SchemaVersion(_)
            | Self::ForeignOwner { .. }
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
            Self::NotFound(what) => write!(f, "no {what}"),
            Self::TooLarge(why) => f.write_str(why),
            Self::Conflict { stored } => {
                write!(
                    f,
                    "the source's record is stored with other bytes ({stored})"
                )
            }
            Self::Corrupt(why) => write!(f, "corrupt item: {why}"),
            Self::SchemaVersion(v) => write!(f, "archive schema version {v} is not supported"),
            Self::ForeignOwner { path, uid } => write!(
                f,
                "{} belongs to uid {uid}, it must belong to the daemon's uid",
                path.display()
            ),
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
    upload_ttl: Duration,
}

impl Archive {
    /// Opens the archive at `path` for `uid`, creating a missing directory 0700, a missing file
    /// 0600 and its schema, and upgrading a file of version 1 to 3; the file runs in WAL mode. A
    /// directory or file that another uid owns or other users can reach is refused.
    ///
    /// # Errors
    /// [`Error::Io`], [`Error::ForeignOwner`], [`Error::OpenMode`], [`Error::SchemaVersion`] for
    /// a file of an unknown version, or [`Error::Sqlite`].
    pub fn open(path: &Path, uid: u32) -> Result<Self, Error> {
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(Error::Io)?;
        owner_only(dir, uid)?;
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
        owner_only(path, uid)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut conn = Connection::open_with_flags(path, flags)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch(PRAGMAS)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match tx.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? {
            // Each step adds tables and never rewrites a row.
            found @ 0..=3 => {
                if found == 0 {
                    tx.execute_batch(SCHEMA)?;
                }
                if found <= 1 {
                    tx.execute_batch(SKIPPED)?;
                }
                if found <= 2 {
                    tx.execute_batch(CHUNKS)?;
                }
                tx.execute_batch(SEQ)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            found => return Err(Error::SchemaVersion(found)),
        }
        tx.commit()?;
        Ok(Self {
            conn,
            upload_ttl: UPLOAD_TTL,
        })
    }

    /// Sets how long an upload lives after its last `begin` or `part`; [`UPLOAD_TTL`] by default.
    pub const fn set_upload_ttl(&mut self, ttl: Duration) {
        self.upload_ttl = ttl;
    }

    /// Archives `item`: its bytes as a new item unless their id is stored, and a new entry
    /// unless its `(source, external_id)` is stored, all in one transaction. A stored pair
    /// with the same id is answered `duplicate` and changes nothing.
    ///
    /// # Errors
    /// [`Error::Invalid`], [`Error::Conflict`] when the pair is stored with another id,
    /// [`Error::Io`] for the clock, or [`Error::Sqlite`].
    pub fn put(&mut self, item: &Item<'_>) -> Result<PutResponse, Error> {
        check_item(item)?;
        let put_at = now()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let put = insert_item(&tx, item, put_at)?;
        tx.commit()?;
        Ok(put)
    }

    /// Stores `page` in one transaction: each item as [`Archive::put`] does, each skip as a
    /// reference unless its `(source, external_id)` is kept, and the page's cursor as the
    /// position of its source. Answers how many items were `stored`.
    ///
    /// # Errors
    /// [`Error::Invalid`], [`Error::Conflict`], [`Error::Io`] for the clock, or
    /// [`Error::Sqlite`]; on any of them nothing of the page is stored.
    pub fn pull(&mut self, page: &Page<'_>) -> Result<u64, Error> {
        check_source(page.source)?;
        for item in &page.items {
            if item.source != page.source {
                return Err(Error::Invalid(format!(
                    "an item of source {} in a page of {}",
                    item.source, page.source
                )));
            }
            check_item(item)?;
        }
        for skip in &page.skipped {
            check_external_id(skip.external_id)?;
            check_time("at", skip.at)?;
        }
        let at = now()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut stored = 0;
        for item in &page.items {
            if insert_item(&tx, item, at)?.status == PutStatus::Stored {
                stored += 1;
            }
        }
        for skip in &page.skipped {
            tx.execute(
                "INSERT INTO skipped (source, external_id, len, at, skipped_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (source, external_id) DO NOTHING",
                params![page.source, skip.external_id, int(skip.len)?, skip.at, at],
            )?;
        }
        if let Some(cursor) = page.cursor {
            tx.execute(
                "INSERT INTO cursors (source, cursor) VALUES (?1, ?2) \
                 ON CONFLICT (source) DO UPDATE SET cursor = excluded.cursor",
                params![page.source, int(cursor)?],
            )?;
        }
        tx.commit()?;
        Ok(stored)
    }

    /// The position stored for `source` by the last [`Archive::pull`] that gave one.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn cursor(&self, source: &str) -> Result<Option<u64>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT cursor FROM cursors WHERE source = ?1",
                [source],
                |r| get_u64(r, 0),
            )
            .optional()?)
    }

    /// Every skipped reference, oldest first.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn skipped(&self) -> Result<Vec<Skipped>, Error> {
        let mut st = self
            .conn
            .prepare("SELECT source, external_id, len, at, skipped_at FROM skipped ORDER BY row")?;
        let rows = st.query_map([], |r| {
            Ok(Skipped {
                source: r.get(0)?,
                external_id: r.get(1)?,
                len: get_u64(r, 2)?,
                at: r.get(3)?,
                skipped_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Items whose text matches `request.q` and which have an entry passing every filter, best
    /// first, each with those entries, a snippet and the head of its text up to
    /// [`HIT_TEXT_CHARS`] characters. The query is sanitized as the reference
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
            "SELECT b.id, snippet(item_fts, 0, '>>>', '<<<', '...', 40), \
                    substr(item_fts.text, 1, ?8), length(item_fts.text) > ?8 \
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
                int(limit)?,
                int(HIT_TEXT_CHARS)?
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, bool>(3)?,
                ))
            },
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
            .map(|(id, snippet, text, text_truncated)| {
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
                    text: Some(text),
                    text_truncated: Some(text_truncated),
                })
            })
            .collect()
    }

    /// The entries of `request.source` after the `seq` `request.after`, by `seq`, at most
    /// `request.limit`, each with the head of its item's text up to `request.text_chars`
    /// characters. The page ends before an entry that would take its serialized body past
    /// [`MAX_LIST_BODY`] bytes, but holds at least one entry when one follows; `more` says
    /// whether the source holds entries after the page's last. Reads only.
    ///
    /// # Errors
    /// [`Error::Invalid`], [`Error::Json`] or [`Error::Sqlite`].
    pub fn list(&self, request: &ListRequest) -> Result<ListResponse, Error> {
        check_source(&request.source)?;
        let after = int(request.after.unwrap_or(0))?;
        let limit = request.limit.unwrap_or(DEFAULT_LIST_LIMIT);
        if !(1..=MAX_LIST_LIMIT).contains(&limit) {
            return Err(Error::Invalid(format!(
                "limit must be 1 to {MAX_LIST_LIMIT}"
            )));
        }
        let text_chars = request.text_chars.unwrap_or(DEFAULT_LIST_TEXT_CHARS);
        if !(1..=MAX_LIST_TEXT_CHARS).contains(&text_chars) {
            return Err(Error::Invalid(format!(
                "text_chars must be 1 to {MAX_LIST_TEXT_CHARS}"
            )));
        }
        let mut st = self.conn.prepare(
            "SELECT s.seq, e.id, e.external_id, e.who, e.at, e.kind, e.put_at, \
                    coalesce(substr(f.text, 1, ?3), ''), coalesce(length(f.text) > ?3, 0) \
             FROM entry_seq s \
             JOIN entries e ON e.source = s.source AND e.external_id = s.external_id \
             JOIN blobs b ON b.id = e.id \
             LEFT JOIN item_fts f ON f.rowid = b.row \
             WHERE s.source = ?1 AND s.seq > ?2 ORDER BY s.seq LIMIT ?4",
        )?;
        // One row past the limit tells whether more follow.
        let mut rows = st.query(params![
            &request.source,
            after,
            int(text_chars)?,
            int(limit)? + 1
        ])?;
        let mut page = ListResponse {
            entries: Vec::new(),
            more: false,
        };
        // `false` is the longer value of `more`, so the bound holds for either.
        let mut body = serde_json::to_vec(&page)?.len();
        while let Some(r) = rows.next()? {
            if page.entries.len() as u64 == limit {
                page.more = true;
                break;
            }
            let truncated: bool = r.get(8)?;
            let listed = Listed {
                seq: get_u64(r, 0)?,
                id: r.get(1)?,
                external_id: r.get(2)?,
                who: r.get(3)?,
                at: r.get(4)?,
                kind: r.get(5)?,
                put_at: r.get(6)?,
                text: r.get(7)?,
                text_truncated: truncated.then_some(true),
            };
            // A comma before every entry but the first.
            let grown =
                body + serde_json::to_vec(&listed)?.len() + usize::from(!page.entries.is_empty());
            if grown > MAX_LIST_BODY && !page.entries.is_empty() {
                page.more = true;
                break;
            }
            body = grown;
            page.entries.push(listed);
        }
        Ok(page)
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
            .ok_or_else(|| Error::NotFound(format!("item {id}")))?;
        if self.owner_of(id)?.is_some() {
            return Err(Error::TooLarge(format!(
                "item {id} of {len} bytes was put in parts: read it by range"
            )));
        }
        let bytes = original(id, len, &codec, body)?;
        self.answer(id, bytes, None, len)
    }

    /// At most `length` bytes of the original of the item `id` from `offset`, cut at its end,
    /// with its entries. The parts the range touches are inflated and each is checked against the
    /// SHA-256 it was stored with; an item stored whole is checked whole.
    ///
    /// # Errors
    /// [`Error::Invalid`] for a malformed id, an `offset` past the end or a `length` of 0 or over
    /// [`MAX_RANGE`], [`Error::NotFound`], [`Error::Corrupt`], or [`Error::Sqlite`].
    pub fn read_range(&self, id: &str, offset: u64, length: u64) -> Result<Original, Error> {
        check_id(id)?;
        if !(1..=MAX_RANGE).contains(&length) {
            return Err(Error::Invalid(format!("length must be 1 to {MAX_RANGE}")));
        }
        let (len, codec, body): (u64, String, Vec<u8>) = self
            .conn
            .query_row(
                "SELECT len, codec, body FROM blobs WHERE id = ?1",
                [id],
                |r| Ok((get_u64(r, 0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("item {id}")))?;
        if offset > len {
            return Err(Error::Invalid(format!(
                "offset {offset} is past the end of the {len} bytes"
            )));
        }
        let end = offset.saturating_add(length).min(len);
        let bytes = match self.owner_of(id)? {
            None => {
                let whole = original(id, len, &codec, body)?;
                whole[to_usize(offset)?..to_usize(end)?].to_vec()
            }
            Some(owner) => self.range_of_parts(id, owner, offset, end)?,
        };
        self.answer(id, bytes, Some(offset), len)
    }

    /// The bytes `offset..end` of the parts of `owner`, each part checked against its SHA-256.
    fn range_of_parts(
        &self,
        id: &str,
        owner: i64,
        offset: u64,
        end: u64,
    ) -> Result<Vec<u8>, Error> {
        let mut out = Vec::with_capacity(to_usize(end - offset)?);
        if offset == end {
            return Ok(out);
        }
        let mut st = self.conn.prepare(
            "SELECT start, len, sha256, codec, body FROM parts \
             WHERE owner = ?1 AND start < ?3 AND start + len > ?2 ORDER BY start",
        )?;
        let mut rows = st.query(params![owner, int(offset)?, int(end)?])?;
        let mut next = offset;
        while let Some(r) = rows.next()? {
            let (start, plen, sha, codec, body): (u64, u64, String, String, Vec<u8>) = (
                get_u64(r, 0)?,
                get_u64(r, 1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
            );
            let part = original(&sha, plen, &codec, body)
                .map_err(|e| Error::Corrupt(format!("item {id}: part at {start}: {e}")))?;
            if start > next {
                break;
            }
            let from = to_usize(next - start)?;
            let to = to_usize(end.min(start + plen) - start)?;
            out.extend_from_slice(&part[from..to]);
            next = start + to as u64;
        }
        if next != end {
            return Err(Error::Corrupt(format!(
                "item {id}: its parts miss bytes from {next}"
            )));
        }
        Ok(out)
    }

    /// The `parts` owner of the item `id` when it was put in parts.
    fn owner_of(&self, id: &str) -> Result<Option<i64>, Error> {
        Ok(self
            .conn
            .query_row("SELECT owner FROM chunked WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    fn answer(
        &self,
        id: &str,
        bytes: Vec<u8>,
        offset: Option<u64>,
        len: u64,
    ) -> Result<Original, Error> {
        let entries = entries_of(&self.conn, id)?;
        let kind = entries
            .first()
            .map(|e| e.kind.clone())
            .ok_or_else(|| Error::Corrupt(format!("item {id} has no entry")))?;
        Ok(Original {
            bytes,
            offset,
            kind,
            len,
            entries,
        })
    }

    /// Opens an upload of `upload` after removing every expired one. A stored
    /// `(source, external_id)` answers `duplicate` with the same id and is refused as a conflict
    /// with another; a stored id adds the entry at once; else the answer is a new upload's
    /// number.
    ///
    /// # Errors
    /// [`Error::Invalid`], [`Error::Conflict`], [`Error::Io`] for the clock, or
    /// [`Error::Sqlite`].
    pub fn begin(&mut self, upload: &Upload<'_>) -> Result<Begun, Error> {
        check_source(upload.source)?;
        check_external_id(upload.external_id)?;
        check_time("at", upload.at)?;
        check_non_empty("kind", upload.kind)?;
        check_id(upload.id)?;
        if !(1..=MAX_ITEM_LEN).contains(&upload.len) {
            return Err(Error::Invalid(format!("len must be 1 to {MAX_ITEM_LEN}")));
        }
        let at = now()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire(&tx, at - self.upload_ttl.as_secs_f64())?;
        let item = Item {
            source: upload.source,
            external_id: upload.external_id,
            who: upload.who,
            at: upload.at,
            kind: upload.kind,
            text: "",
            bytes: &[],
        };
        let begun = if let Some(put) = settle(&tx, &item, upload.id, at)? {
            Begun::Done(put)
        } else {
            tx.execute(
                "INSERT INTO uploads (id, len, source, external_id, who, at, kind, text, parts, \
                 received, touched_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 0, ?9)",
                params![
                    upload.id,
                    int(upload.len)?,
                    upload.source,
                    upload.external_id,
                    upload.who,
                    upload.at,
                    upload.kind,
                    head(upload.text, MAX_UPLOAD_TEXT),
                    at
                ],
            )?;
            Begun::Upload(to_u64(tx.last_insert_rowid())?)
        };
        tx.commit()?;
        Ok(begun)
    }

    /// Stores `bytes` as the part `index` of the open upload `upload`: the next one in order, 1 to
    /// [`MAX_PART`] bytes, not past the upload's length.
    ///
    /// # Errors
    /// [`Error::NotFound`] for an upload that is not open or expired, [`Error::Invalid`],
    /// [`Error::Io`] for the clock, or [`Error::Sqlite`].
    pub fn part(&mut self, upload: u64, index: u64, bytes: &[u8]) -> Result<Received, Error> {
        if bytes.is_empty() || bytes.len() > MAX_PART {
            return Err(Error::Invalid(format!(
                "a part must be 1 to {MAX_PART} bytes"
            )));
        }
        let at = now()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (len, parts, received) = open_upload(&tx, upload, at - self.upload_ttl.as_secs_f64())?;
        if index != parts {
            return Err(Error::Invalid(format!(
                "upload {upload} expects part {parts}, not {index}"
            )));
        }
        let bytes_len = bytes.len() as u64;
        if received + bytes_len > len {
            return Err(Error::Invalid(format!(
                "upload {upload} of {len} bytes holds {received}; a part of {bytes_len} passes its end"
            )));
        }
        let (codec, body) = deflated(bytes);
        tx.execute(
            "INSERT INTO parts (owner, seq, start, len, sha256, codec, body) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                int(upload)?,
                int(index)?,
                int(received)?,
                int(bytes_len)?,
                hex(&Sha256::digest(bytes)),
                codec,
                body.as_ref()
            ],
        )?;
        tx.execute(
            "UPDATE uploads SET parts = parts + 1, received = received + ?2, touched_at = ?3 \
             WHERE row = ?1",
            params![int(upload)?, int(bytes_len)?, at],
        )?;
        tx.commit()?;
        Ok(Received {
            parts: parts + 1,
            bytes: received + bytes_len,
        })
    }

    /// Closes the upload `upload`: checks that its parts make its length and id, then stores
    /// the item over those parts, its index row and its entry as `put` does, in one transaction.
    /// Parts that miss bytes or hash to another id remove the upload and store nothing.
    ///
    /// # Errors
    /// [`Error::NotFound`] for an upload that is not open or expired, [`Error::Invalid`] for
    /// missing bytes or another SHA-256, [`Error::Conflict`], [`Error::Corrupt`] for a part
    /// that does not inflate, [`Error::Io`] for the clock, or [`Error::Sqlite`].
    pub fn end(&mut self, upload: u64) -> Result<PutResponse, Error> {
        let at = now()?;
        let horizon = at - self.upload_ttl.as_secs_f64();
        let (len, _, received) = open_upload(&self.conn, upload, horizon)?;
        let refusal = self.refusal(upload, len, received)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(why) = refusal {
            drop_upload(&tx, int(upload)?)?;
            tx.commit()?;
            return Err(Error::Invalid(why));
        }
        let owner = int(upload)?;
        let (id, source, external_id, who, at_record, kind, text): (
            String,
            String,
            String,
            Option<String>,
            Option<f64>,
            String,
            String,
        ) = tx.query_row(
            "SELECT id, source, external_id, who, at, kind, text FROM uploads WHERE row = ?1",
            [owner],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )?;
        let item = Item {
            source: &source,
            external_id: &external_id,
            who: who.as_deref(),
            at: at_record,
            kind: &kind,
            text: &text,
            bytes: &[],
        };
        let put = match settle(&tx, &item, &id, at) {
            Ok(Some(put)) => {
                // Settled by another upload or put meanwhile: these parts are not needed.
                drop_upload(&tx, owner)?;
                put
            }
            Ok(None) => {
                tx.execute(
                    "INSERT INTO blobs (id, len, codec, body) VALUES (?1, ?2, 'none', X'')",
                    params![&id, int(len)?],
                )?;
                tx.execute(
                    "INSERT INTO item_fts (rowid, text) VALUES (?1, ?2)",
                    params![tx.last_insert_rowid(), &text],
                )?;
                tx.execute(
                    "INSERT INTO chunked (id, owner) VALUES (?1, ?2)",
                    params![&id, owner],
                )?;
                tx.execute("DELETE FROM uploads WHERE row = ?1", [owner])?;
                insert_entry(&tx, &item, &id, at)?;
                PutResponse {
                    id,
                    status: PutStatus::Stored,
                }
            }
            Err(e @ Error::Conflict { .. }) => {
                // Another put stored the pair with other bytes meanwhile: the parts are not needed.
                drop_upload(&tx, owner)?;
                tx.commit()?;
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        tx.commit()?;
        Ok(put)
    }

    /// Why the upload `upload` of `len` bytes, `received` of them held, cannot be stored: missing
    /// bytes or another SHA-256; `None` when its parts make its original.
    fn refusal(&self, upload: u64, len: u64, received: u64) -> Result<Option<String>, Error> {
        Ok(if received == len {
            let (bytes, sha) = self.hash_parts(upload)?;
            let id: String = self.conn.query_row(
                "SELECT id FROM uploads WHERE row = ?1",
                [int(upload)?],
                |r| r.get(0),
            )?;
            if bytes != len {
                Some(format!(
                    "upload {upload}: its parts hold {bytes} of {len} bytes"
                ))
            } else if sha != id {
                Some(format!(
                    "upload {upload}: its bytes hash to {sha}, not to the id {id}"
                ))
            } else {
                None
            }
        } else {
            Some(format!(
                "upload {upload} holds {received} of its {len} bytes"
            ))
        })
    }

    /// The bytes the parts of `owner` hold and their SHA-256, inflating one part at a time.
    fn hash_parts(&self, owner: u64) -> Result<(u64, String), Error> {
        let mut st = self.conn.prepare(
            "SELECT start, len, sha256, codec, body FROM parts WHERE owner = ?1 ORDER BY seq",
        )?;
        let mut rows = st.query([int(owner)?])?;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        while let Some(r) = rows.next()? {
            let (start, plen, sha, codec, body): (u64, u64, String, String, Vec<u8>) = (
                get_u64(r, 0)?,
                get_u64(r, 1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
            );
            if start != total {
                return Err(Error::Corrupt(format!(
                    "upload {owner}: a part starts at {start}, not {total}"
                )));
            }
            let part = original(&sha, plen, &codec, body)?;
            hasher.update(&part);
            total += plen;
        }
        Ok((total, hex(&hasher.finalize())))
    }

    /// Uploads open now, expired ones included until the next `begin` removes them.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn uploads(&self) -> Result<u64, Error> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM uploads", [], |r| get_u64(r, 0))?)
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
            .ok_or_else(|| Error::NotFound(format!("item {id}")))?;
        let entries = entries_of(&tx, id)?;
        let owner: Option<i64> = tx
            .query_row("SELECT owner FROM chunked WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        tx.execute("DELETE FROM item_fts WHERE rowid = ?1", [row])?;
        tx.execute("DELETE FROM entries WHERE id = ?1", [id])?;
        if let Some(owner) = owner {
            tx.execute("DELETE FROM chunked WHERE id = ?1", [id])?;
            tx.execute("DELETE FROM parts WHERE owner = ?1", [owner])?;
        }
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

/// Refuses `path` when another uid than `uid` owns it or its group or other permission bits are
/// set.
fn owner_only(path: &Path, uid: u32) -> Result<(), Error> {
    let meta = fs::metadata(path).map_err(Error::Io)?;
    if meta.uid() != uid {
        return Err(Error::ForeignOwner {
            path: path.to_owned(),
            uid: meta.uid(),
        });
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(Error::OpenMode {
            path: path.to_owned(),
            mode,
        });
    }
    Ok(())
}

fn check_item(item: &Item<'_>) -> Result<(), Error> {
    check_source(item.source)?;
    check_external_id(item.external_id)?;
    check_time("at", item.at)?;
    check_non_empty("kind", item.kind)
}

/// Archives the checked `item` in `tx` as [`Archive::put`] describes, its entry put at `put_at`.
fn insert_item(tx: &Transaction<'_>, item: &Item<'_>, put_at: f64) -> Result<PutResponse, Error> {
    let id = hex(&Sha256::digest(item.bytes));
    if let Some(put) = settle(tx, item, &id, put_at)? {
        return Ok(put);
    }
    insert_blob(tx, &id, item)?;
    insert_entry(tx, item, &id, put_at)?;
    Ok(PutResponse {
        id,
        status: PutStatus::Stored,
    })
}

/// Settles the entry of `item` with the original `id` when that needs no new blob: `duplicate`
/// for its stored pair with the same id, a conflict for one with another, a new entry `stored`
/// when the id is stored; `None` when the original must be stored first.
fn settle(
    tx: &Transaction<'_>,
    item: &Item<'_>,
    id: &str,
    put_at: f64,
) -> Result<Option<PutResponse>, Error> {
    let stored: Option<String> = tx
        .query_row(
            "SELECT id FROM entries WHERE source = ?1 AND external_id = ?2",
            params![item.source, item.external_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(stored) = stored {
        return if stored == id {
            Ok(Some(PutResponse {
                id: id.to_owned(),
                status: PutStatus::Duplicate,
            }))
        } else {
            Err(Error::Conflict { stored })
        };
    }
    let known = tx
        .query_row("SELECT 1 FROM blobs WHERE id = ?1", [id], |_| Ok(()))
        .optional()?
        .is_some();
    if !known {
        return Ok(None);
    }
    insert_entry(tx, item, id, put_at)?;
    Ok(Some(PutResponse {
        id: id.to_owned(),
        status: PutStatus::Stored,
    }))
}

/// The entry of `item` for the stored original `id`.
fn insert_entry(tx: &Transaction<'_>, item: &Item<'_>, id: &str, put_at: f64) -> Result<(), Error> {
    tx.execute(
        &format!("INSERT INTO entries (id, {ENTRY_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
        params![
            id,
            item.source,
            item.external_id,
            item.who,
            item.at,
            item.kind,
            put_at
        ],
    )?;
    Ok(())
}

/// Removes every upload last touched before `horizon` with its parts.
fn expire(tx: &Transaction<'_>, horizon: f64) -> Result<(), Error> {
    tx.execute(
        "DELETE FROM parts WHERE owner IN (SELECT row FROM uploads WHERE touched_at < ?1)",
        [horizon],
    )?;
    tx.execute("DELETE FROM uploads WHERE touched_at < ?1", [horizon])?;
    Ok(())
}

/// Removes the upload `owner` with its parts.
fn drop_upload(tx: &Transaction<'_>, owner: i64) -> Result<(), Error> {
    tx.execute("DELETE FROM parts WHERE owner = ?1", [owner])?;
    tx.execute("DELETE FROM uploads WHERE row = ?1", [owner])?;
    Ok(())
}

/// The length, parts and received bytes of the upload `upload` when it is open and was touched
/// at or after `horizon`.
fn open_upload(conn: &Connection, upload: u64, horizon: f64) -> Result<(u64, u64, u64), Error> {
    let gone = || Error::NotFound(format!("open upload {upload}"));
    let row = i64::try_from(upload).map_err(|_| gone())?;
    conn.query_row(
        "SELECT len, parts, received FROM uploads WHERE row = ?1 AND touched_at >= ?2",
        params![row, horizon],
        |r| Ok((get_u64(r, 0)?, get_u64(r, 1)?, get_u64(r, 2)?)),
    )
    .optional()?
    .ok_or_else(gone)
}

/// The deflated `bytes` with codec `deflate` when that is shorter, else `bytes` with `none`.
fn deflated(bytes: &[u8]) -> (&'static str, std::borrow::Cow<'_, [u8]>) {
    let deflated = compress_to_vec(bytes, DEFLATE_LEVEL);
    if deflated.len() < bytes.len() {
        ("deflate", std::borrow::Cow::Owned(deflated))
    } else {
        ("none", std::borrow::Cow::Borrowed(bytes))
    }
}

/// The longest head of `text` of at most `max` bytes that ends at a character boundary.
fn head(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Writes the blob of `item` under `id`, deflated when that is shorter, and its FTS5 row.
fn insert_blob(tx: &Transaction<'_>, id: &str, item: &Item<'_>) -> Result<(), Error> {
    let (codec, body) = deflated(item.bytes);
    tx.execute(
        "INSERT INTO blobs (id, len, codec, body) VALUES (?1, ?2, ?3, ?4)",
        params![id, int(item.bytes.len() as u64)?, codec, body.as_ref()],
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

/// `n` as a length in memory.
fn to_usize(n: u64) -> Result<usize, Error> {
    usize::try_from(n).map_err(|_| Error::Invalid(format!("{n} is above the address range")))
}

/// A row number as an upload's number.
fn to_u64(n: i64) -> Result<u64, Error> {
    u64::try_from(n).map_err(|_| Error::Corrupt(format!("row number {n}")))
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
