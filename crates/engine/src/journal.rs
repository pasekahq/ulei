// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The session journal (C8, ADR-0014): sessions with their system prompts, their live and
//! archived messages and the steps of their turns, the history index of their turn messages and
//! the forget marks on messages (ADR-0051) and the record of each inbox event the C6 server
//! accepted, in one `SQLite` file. A turn sends the live set and numbers its request ids from
//! journal state, so a turn after a restart sends the bytes an engine that never stopped would
//! send, but for the random tag of its request ids.

pub mod history;
pub mod marks;
pub mod raw;
pub mod search;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{DirBuilder, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Statement, TransactionBehavior, params};
use serde_json::{Map, Value};
use ulei_contracts::generated::c4g::{Message, Usage};
use ulei_contracts::generated::c8::{
    self, InboxDelivery, InboxDeliveryDelivery, MessageOrigin, Session, Step,
};

use crate::agent::Outcome;
use crate::compress;
use crate::tools::random_hex;

/// Schema version of the file, kept in `PRAGMA user_version`; files of versions 1 to 8 are
/// migrated.
pub const SCHEMA_VERSION: i64 = 9;
/// Longest session key, as the C4G `session_key`.
pub const MAX_KEY_CHARS: usize = 128;

const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const PRAGMAS: &str = "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-512; PRAGMA foreign_keys=ON;";
const SCHEMA: &str = "CREATE TABLE sessions (
  id INTEGER PRIMARY KEY, key TEXT NOT NULL, system_prompt TEXT, route TEXT);
CREATE INDEX sessions_key ON sessions (key, id);
CREATE TABLE steps (
  session INTEGER NOT NULL REFERENCES sessions (id), turn INTEGER NOT NULL,
  step INTEGER NOT NULL, request_id TEXT NOT NULL UNIQUE, route TEXT, usage TEXT,
  PRIMARY KEY (session, turn, step)) WITHOUT ROWID;
CREATE TABLE deliveries (
  event_id TEXT PRIMARY KEY, attempt INTEGER NOT NULL, turn TEXT NOT NULL,
  delivery TEXT) WITHOUT ROWID;";
const MESSAGES: &str = "CREATE TABLE messages (
  session INTEGER NOT NULL REFERENCES sessions (id), seq INTEGER NOT NULL,
  turn INTEGER NOT NULL, message TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1,
  compacted INTEGER NOT NULL DEFAULT 0, n INTEGER NOT NULL UNIQUE, at INTEGER,
  origin TEXT NOT NULL CHECK (origin IN ('turn', 'compaction')),
  PRIMARY KEY (session, seq)) WITHOUT ROWID;
CREATE INDEX messages_live ON messages (session, active, seq);";
/// The history index: its own copy of the text of each `turn` message, rowid the message's `n`.
const FTS: &str =
    "CREATE VIRTUAL TABLE messages_fts USING fts5(content, tool_name, tool_calls, role UNINDEXED);";
/// Forget marks; a mark is live while `lifted_at` is NULL and is never deleted.
const MARKS: &str = "CREATE TABLE marks (
  id INTEGER PRIMARY KEY, n INTEGER NOT NULL REFERENCES messages (n), at INTEGER NOT NULL,
  reason TEXT NOT NULL, provenance TEXT NOT NULL, lifted_at INTEGER, lifted_provenance TEXT);
CREATE INDEX marks_live ON marks (n) WHERE lifted_at IS NULL;";
/// Engine settings the journal keeps across restarts, one row per name; `route` is the default
/// route alias a global C6 `set_route` stored (C8 `DefaultRoute`, ADR-0094) and
/// `prune_rearm.<session>` the wait of a session's next proactive prune (C8 `PruneRearm`,
/// ADR-0098).
const SETTINGS: &str =
    "CREATE TABLE settings (name TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;";
/// The name of the default route alias in `settings`.
const DEFAULT_ROUTE: &str = "route";
/// The prefix of the name of a session's proactive prune wait in `settings`, before its id.
const PRUNE_REARM: &str = "prune_rearm.";
const INDEX: &str = "INSERT INTO messages_fts (rowid, content, tool_name, tool_calls, role) VALUES (?1, ?2, ?3, ?4, ?5)";
/// Version 1 to 2: sessions gain the stored system prompt, absent for the existing ones.
const MIGRATE_1: &str =
    "ALTER TABLE sessions ADD COLUMN system_prompt TEXT; PRAGMA user_version=2;";
/// Version 2 to 3: messages gain the compaction flags, the existing ones live.
const MIGRATE_2: &str = "ALTER TABLE messages ADD COLUMN active INTEGER NOT NULL DEFAULT 1;
ALTER TABLE messages ADD COLUMN compacted INTEGER NOT NULL DEFAULT 0;
CREATE INDEX messages_live ON messages (session, active, seq);
PRAGMA user_version=3;";
/// Version 3 to 4: sessions gain the stored route alias, absent for the existing ones.
const MIGRATE_3: &str = "ALTER TABLE sessions ADD COLUMN route TEXT; PRAGMA user_version=4;";
/// Version 4 to 5: the inbox delivery records, none for the existing file.
const MIGRATE_4: &str = "CREATE TABLE deliveries (
  event_id TEXT PRIMARY KEY, attempt INTEGER NOT NULL, turn TEXT NOT NULL,
  delivery TEXT) WITHOUT ROWID;
PRAGMA user_version=5;";
/// Version 5 to 6, first half: the version 5 messages set aside for [`migrate_5`].
const MIGRATE_5: &str = "ALTER TABLE messages RENAME TO messages5; DROP INDEX messages_live;";

/// Failure of a journal operation.
#[derive(Debug)]
pub enum Error {
    /// The file carries another schema version than [`SCHEMA_VERSION`].
    SchemaVersion(i64),
    /// The session key is empty or longer than [`MAX_KEY_CHARS`].
    InvalidKey,
    /// No session has this id.
    UnknownSession(u64),
    /// No message has this `n`.
    UnknownMessage(u64),
    /// The session has no stored turn, so it has nothing to compact.
    NoTurn(u64),
    /// The compacted set is empty; a live set without messages would lose the session.
    EmptyCompaction(u64),
    /// The turn is not the next one of its session: another append came first.
    StaleTurn {
        /// The turn the append named.
        turn: u64,
        /// The session's next turn.
        next: u64,
    },
    /// Creating the journal's directory or file failed.
    Io(io::Error),
    /// A record does not serialize, or a stored record is not its C4G or C8 shape.
    Json(serde_json::Error),
    /// `SQLite` failure.
    Sqlite(rusqlite::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SchemaVersion(found) => write!(
                f,
                "journal schema version {found}, this engine reads {SCHEMA_VERSION}"
            ),
            Self::InvalidKey => write!(f, "session key must have 1 to {MAX_KEY_CHARS} chars"),
            Self::UnknownSession(id) => write!(f, "no session {id} in the journal"),
            Self::UnknownMessage(n) => write!(f, "no message {n} in the journal"),
            Self::NoTurn(id) => write!(f, "session {id} has no turn to compact"),
            Self::EmptyCompaction(id) => write!(f, "session {id} cannot be compacted to nothing"),
            Self::StaleTurn { turn, next } => {
                write!(f, "turn {turn} is stale, the next turn is {next}")
            }
            Self::Io(e) => write!(f, "journal file: {e}"),
            Self::Json(e) => write!(f, "journal record: {e}"),
            Self::Sqlite(e) => write!(f, "journal SQLite: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

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

/// The next turn of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Turn {
    /// `id` of the session.
    pub session: u64,
    /// Turn number, from 1.
    pub number: u64,
}

impl Turn {
    /// The request ids of one run of this turn, under a tag drawn from the kernel's random
    /// source.
    ///
    /// # Errors
    /// The failure of the random source.
    pub fn request_ids(&self) -> io::Result<RequestIds> {
        Ok(RequestIds {
            turn: *self,
            tag: random_hex(TAG_BYTES)?,
        })
    }
}

/// Random bytes of the tag that sets the request ids of one run of a turn apart from those of
/// every other run, in this process or another, with this journal or another.
const TAG_BYTES: usize = 6;

/// The C4G request ids of one run of a turn.
#[derive(Debug, Clone)]
pub struct RequestIds {
    turn: Turn,
    tag: String,
}

impl RequestIds {
    /// The `request_id` of step `step`: `s<session>-t<turn>-<step>-<12 hex tag>`. Session and
    /// turn are `SQLite` integers of at most 19 digits, so the id stays within 64 bytes for any
    /// step below 10^9.
    pub fn step(&self, step: usize) -> String {
        format!(
            "s{}-t{}-{step}-{}",
            self.turn.session, self.turn.number, self.tag
        )
    }
}

/// An open journal file.
#[derive(Debug)]
pub struct Journal {
    conn: Connection,
}

impl Journal {
    /// Opens the journal at `path`, creating a missing directory 0700, a missing file 0600 and
    /// its schema, and migrating a file of an older version; the conversation history is
    /// readable by the owner only.
    ///
    /// # Errors
    /// [`Error::Io`], [`Error::SchemaVersion`] when the file has a version above
    /// [`SCHEMA_VERSION`], [`Error::Json`] for a message of a migrated file that is not a C4G
    /// `Message`, or [`Error::Sqlite`].
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(Error::Io)?;
        }
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
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut conn = Connection::open_with_flags(path, flags)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch(PRAGMAS)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match tx.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? {
            0 => {
                tx.execute_batch(SCHEMA)?;
                tx.execute_batch(MESSAGES)?;
                tx.execute_batch(FTS)?;
                tx.execute_batch(MARKS)?;
                tx.execute_batch(SETTINGS)?;
            }
            SCHEMA_VERSION => {}
            found @ 1..SCHEMA_VERSION => {
                for (from, step) in (1..).zip([MIGRATE_1, MIGRATE_2, MIGRATE_3, MIGRATE_4]) {
                    if from >= found {
                        tx.execute_batch(step)?;
                    }
                }
                if found < 6 {
                    migrate_5(&tx)?;
                }
                if found < 7 {
                    migrate_6(&tx)?;
                }
                if found < 8 {
                    tx.execute_batch(MARKS)?;
                }
                tx.execute_batch(SETTINGS)?;
            }
            found => return Err(Error::SchemaVersion(found)),
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(Self { conn })
    }

    /// The current session of `key` with its stored system prompt and route, created empty
    /// when the key has none.
    ///
    /// # Errors
    /// [`Error::InvalidKey`] or [`Error::Sqlite`].
    pub fn session(&mut self, key: &str) -> Result<Session, Error> {
        check_key(key)?;
        match self.current(key)? {
            Some(session) => Ok(session),
            None => self.new_session(key),
        }
    }

    /// The route alias stored with the current session of `key`; `None` when it stores none or
    /// the key has no session. Waits at most `within` for a lock another connection holds.
    ///
    /// # Errors
    /// [`Error::InvalidKey`] or [`Error::Sqlite`], `SQLITE_BUSY` when the lock outlasts
    /// `within`.
    pub fn route(&self, key: &str, within: Duration) -> Result<Option<String>, Error> {
        Ok(self
            .current_within(key, within)?
            .and_then(|session| session.route))
    }

    /// Whether `key` has a session. Waits at most `within` for a lock another connection holds.
    ///
    /// # Errors
    /// [`Error::InvalidKey`] or [`Error::Sqlite`], `SQLITE_BUSY` when the lock outlasts
    /// `within`.
    pub fn has_session(&self, key: &str, within: Duration) -> Result<bool, Error> {
        Ok(self.current_within(key, within)?.is_some())
    }

    /// The current session of `key`, waiting at most `within` for a lock.
    fn current_within(&self, key: &str, within: Duration) -> Result<Option<Session>, Error> {
        check_key(key)?;
        self.conn.busy_timeout(within)?;
        let current = self.current(key);
        self.conn.busy_timeout(BUSY_TIMEOUT)?;
        current
    }

    /// Stores `route` with the current session of `key`, created empty when the key has none.
    /// Waits at most `within` for a lock another connection holds.
    ///
    /// # Errors
    /// [`Error::InvalidKey`] or [`Error::Sqlite`], `SQLITE_BUSY` when the lock outlasts
    /// `within`.
    pub fn store_route(&mut self, key: &str, route: &str, within: Duration) -> Result<(), Error> {
        self.conn.busy_timeout(within)?;
        let stored = self.session(key).and_then(|session| {
            self.conn.execute(
                "UPDATE sessions SET route=?2 WHERE id=?1",
                params![session.id.cast_signed(), route],
            )?;
            Ok(())
        });
        self.conn.busy_timeout(BUSY_TIMEOUT)?;
        stored
    }

    /// The default route alias a global C6 `set_route` stored (C8 `DefaultRoute`); `None` when
    /// none is stored and the configured one holds. Waits at most `within` for a lock another
    /// connection holds.
    ///
    /// # Errors
    /// [`Error::Sqlite`], `SQLITE_BUSY` when the lock outlasts `within`.
    pub fn default_route(&self, within: Duration) -> Result<Option<String>, Error> {
        self.conn.busy_timeout(within)?;
        let route = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE name=?1",
                [DEFAULT_ROUTE],
                |r| r.get(0),
            )
            .optional();
        self.conn.busy_timeout(BUSY_TIMEOUT)?;
        Ok(route?)
    }

    /// Stores `route` as the default route alias, or with `None` removes the stored one so the
    /// configured default holds again. Waits at most `within` for a lock another connection
    /// holds.
    ///
    /// # Errors
    /// [`Error::Sqlite`], `SQLITE_BUSY` when the lock outlasts `within`.
    pub fn store_default_route(
        &mut self,
        route: Option<&str>,
        within: Duration,
    ) -> Result<(), Error> {
        self.conn.busy_timeout(within)?;
        let stored = match route {
            Some(route) => self.conn.execute(
                "INSERT INTO settings (name, value) VALUES (?1, ?2)
                 ON CONFLICT (name) DO UPDATE SET value=excluded.value",
                params![DEFAULT_ROUTE, route],
            ),
            None => self
                .conn
                .execute("DELETE FROM settings WHERE name=?1", [DEFAULT_ROUTE]),
        };
        self.conn.busy_timeout(BUSY_TIMEOUT)?;
        stored.map(drop).map_err(Error::from)
    }

    /// The session of `key` with the highest id, if any.
    fn current(&self, key: &str) -> Result<Option<Session>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, system_prompt, route FROM sessions WHERE key=?1 ORDER BY id DESC LIMIT 1",
                [key],
                |r| {
                    Ok(Session {
                        id: r.get::<_, i64>(0)?.cast_unsigned(),
                        key: key.to_owned(),
                        system_prompt: r.get(1)?,
                        route: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Starts an empty session that becomes the current one of `key`; older sessions of the
    /// key stay readable by their id.
    ///
    /// # Errors
    /// [`Error::InvalidKey`] or [`Error::Sqlite`].
    pub fn new_session(&mut self, key: &str) -> Result<Session, Error> {
        check_key(key)?;
        self.conn
            .execute("INSERT INTO sessions (key) VALUES (?1)", [key])?;
        Ok(Session {
            id: self.conn.last_insert_rowid().cast_unsigned(),
            key: key.to_owned(),
            system_prompt: None,
            route: None,
        })
    }

    /// Stores `prompt` as the system prompt of session `session`.
    ///
    /// # Errors
    /// [`Error::UnknownSession`] or [`Error::Sqlite`].
    pub fn store_system_prompt(&mut self, session: u64, prompt: &str) -> Result<(), Error> {
        let updated = self.conn.execute(
            "UPDATE sessions SET system_prompt=?2 WHERE id=?1",
            params![session.cast_signed(), prompt],
        )?;
        if updated == 0 {
            return Err(Error::UnknownSession(session));
        }
        Ok(())
    }

    /// The live set of session `session`, the messages a turn sends, in order.
    ///
    /// # Errors
    /// [`Error::UnknownSession`], [`Error::Json`] for a stored message that is not a C4G
    /// `Message`, or [`Error::Sqlite`].
    pub fn messages(&self, session: u64) -> Result<Vec<Message>, Error> {
        self.known(session)?;
        let mut stmt = self
            .conn
            .prepare("SELECT message FROM messages WHERE session=?1 AND active=1 ORDER BY seq")?;
        let rows = stmt.query_map([session.cast_signed()], |r| r.get::<_, String>(0))?;
        rows.map(|text| Ok(serde_json::from_str(&text?)?)).collect()
    }

    /// Every message record of session `session`, live and archived, in `seq` order.
    ///
    /// # Errors
    /// [`Error::UnknownSession`], [`Error::Json`] for a stored message that is not an object
    /// or a stored origin that is not a C8 one, or [`Error::Sqlite`].
    pub fn records(&self, session: u64) -> Result<Vec<c8::Message>, Error> {
        self.known(session)?;
        let mut stmt = self.conn.prepare(
            "SELECT seq, turn, message, active, compacted, n, at, origin FROM messages WHERE session=?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([session.cast_signed()], |r| {
            Ok((
                (
                    r.get::<_, i64>(0)?.cast_unsigned(),
                    r.get::<_, i64>(1)?.cast_unsigned(),
                    r.get::<_, String>(2)?,
                ),
                (r.get::<_, bool>(3)?, r.get::<_, bool>(4)?),
                (
                    r.get::<_, i64>(5)?.cast_unsigned(),
                    r.get::<_, Option<i64>>(6)?.map(i64::cast_unsigned),
                    r.get::<_, String>(7)?,
                ),
            ))
        })?;
        rows.map(|row| {
            let ((seq, turn, message), (active, compacted), (n, at, origin)) = row?;
            Ok(c8::Message {
                session,
                seq,
                turn,
                message: object(&message)?,
                active: Some(active),
                compacted: Some(compacted),
                n: Some(n),
                at,
                origin: Some(serde_json::from_value(Value::String(origin))?),
            })
        })
        .collect()
    }

    /// The steps of session `session`, in turn and step order.
    ///
    /// # Errors
    /// [`Error::UnknownSession`], [`Error::Json`] for a stored route or usage that is not an
    /// object, or [`Error::Sqlite`].
    pub fn steps(&self, session: u64) -> Result<Vec<Step>, Error> {
        self.known(session)?;
        let mut stmt = self.conn.prepare(
            "SELECT turn, step, request_id, route, usage FROM steps WHERE session=?1 ORDER BY turn, step",
        )?;
        let rows = stmt.query_map([session.cast_signed()], |r| {
            Ok((
                r.get::<_, i64>(0)?.cast_unsigned(),
                r.get::<_, i64>(1)?.cast_unsigned(),
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (turn, step, request_id, route, usage) = row?;
            Ok(Step {
                session,
                turn,
                step,
                request_id,
                route: route.as_deref().map(object).transpose()?,
                usage: usage.as_deref().map(object).transpose()?,
            })
        })
        .collect()
    }

    /// The usage the last step of session `session` reported; `None` without one.
    ///
    /// # Errors
    /// [`Error::UnknownSession`], [`Error::Json`] for a stored usage that is not a C4G `Usage`,
    /// or [`Error::Sqlite`].
    pub fn last_usage(&self, session: u64) -> Result<Option<Usage>, Error> {
        self.known(session)?;
        let usage = self
            .conn
            .query_row(
                "SELECT usage FROM steps WHERE session=?1 ORDER BY turn DESC, step DESC LIMIT 1",
                [session.cast_signed()],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(usage.as_deref().map(serde_json::from_str).transpose()?)
    }

    /// The rough tokens the live set of session `session` must reach before its next proactive
    /// prune (C8 `PruneRearm`); 0 when none is stored or the stored value is not a count.
    ///
    /// # Errors
    /// [`Error::UnknownSession`] or [`Error::Sqlite`].
    pub fn prune_rearm(&self, session: u64) -> Result<u64, Error> {
        self.known(session)?;
        let value = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE name=?1",
                [format!("{PRUNE_REARM}{session}")],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(value.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    /// The next turn of session `session`.
    ///
    /// # Errors
    /// [`Error::UnknownSession`] or [`Error::Sqlite`].
    pub fn next_turn(&self, session: u64) -> Result<Turn, Error> {
        self.known(session)?;
        Ok(Turn {
            session,
            number: next_turn(&self.conn, session)?,
        })
    }

    /// The last stored turn of `key`: the highest turn of its newest session that has one;
    /// `None` when no session of the key has a stored turn. Waits at most `within` for a lock
    /// another connection holds.
    ///
    /// # Errors
    /// [`Error::Sqlite`], `SQLITE_BUSY` when the lock outlasts `within`.
    pub fn last_turn(&self, key: &str, within: Duration) -> Result<Option<Turn>, Error> {
        self.conn.busy_timeout(within)?;
        let last = self
            .conn
            .query_row(
                "SELECT s.id, max(t.turn) FROM sessions s JOIN (SELECT session, turn FROM messages UNION ALL SELECT session, turn FROM steps) t ON t.session=s.id WHERE s.key=?1 GROUP BY s.id ORDER BY s.id DESC LIMIT 1",
                [key],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional();
        self.conn.busy_timeout(BUSY_TIMEOUT)?;
        Ok(last?.map(|(session, number)| Turn {
            session: session.cast_unsigned(),
            number: number.cast_unsigned(),
        }))
    }

    /// Appends one turn in one transaction at Unix time `at`: `input`, the messages the turn
    /// started with, then the messages and steps of `outcome`. After a compaction inside the
    /// turn the live set, `input` and the messages before the compaction are archived, and the
    /// compacted set and the later messages follow them as the new live set, and the session's
    /// proactive prune wait becomes the compacted set's
    /// [`Compacted::prune_rearm`](crate::agent::Compacted::prune_rearm). Every row
    /// carries the turn's number; the compacted set has origin `compaction`, every other message
    /// `turn` and its history index row. The marks and lifts the turn made gain the `n` of the
    /// first message of `input`, its prompt, in their provenance. On any failure the journal is
    /// left as it was.
    ///
    /// # Errors
    /// [`Error::UnknownSession`], [`Error::StaleTurn`] when `turn` is not the session's next
    /// turn, [`Error::EmptyCompaction`] for an empty compacted set, [`Error::Json`] or
    /// [`Error::Sqlite`].
    pub fn append(
        &mut self,
        turn: Turn,
        input: &[Message],
        outcome: &Outcome,
        at: i64,
    ) -> Result<(), Error> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !exists(&tx, turn.session)? {
            return Err(Error::UnknownSession(turn.session));
        }
        let next = next_turn(&tx, turn.session)?;
        if turn.number != next {
            return Err(Error::StaleTurn {
                turn: turn.number,
                next,
            });
        }
        let names = compress::tool_names(input.iter().chain(&outcome.messages));
        let prompt = next_n(&tx)?;
        match &outcome.compacted {
            None => insert_messages(
                &tx,
                turn,
                at,
                true,
                MessageOrigin::Turn,
                input.iter().chain(&outcome.messages),
                &names,
            )?,
            Some(compacted) => {
                if compacted.live.is_empty() {
                    return Err(Error::EmptyCompaction(turn.session));
                }
                let (before, after) = outcome.messages.split_at(compacted.after);
                archive_live(&tx, turn.session)?;
                insert_messages(
                    &tx,
                    turn,
                    at,
                    false,
                    MessageOrigin::Turn,
                    input.iter().chain(before),
                    &names,
                )?;
                insert_messages(
                    &tx,
                    turn,
                    at,
                    true,
                    MessageOrigin::Compaction,
                    &compacted.live,
                    &names,
                )?;
                insert_messages(&tx, turn, at, true, MessageOrigin::Turn, after, &names)?;
                store_prune_rearm(&tx, turn.session, compacted.prune_rearm)?;
            }
        }
        {
            let mut insert = tx.prepare(
                "INSERT INTO steps (session, turn, step, request_id, route, usage) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (n, step) in (0i64..).zip(&outcome.steps) {
                insert.execute(params![
                    turn.session.cast_signed(),
                    turn.number.cast_signed(),
                    n,
                    step.request_id,
                    step.route.as_ref().map(serde_json::to_string).transpose()?,
                    step.usage.as_ref().map(serde_json::to_string).transpose()?,
                ])?;
            }
        }
        if !input.is_empty() {
            marks::sign(&tx, turn, prompt)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Replaces the live set of session `session` with `compacted` in one transaction at Unix
    /// time `at`: the live messages stay stored as archived (`active` false, `compacted` true)
    /// and `compacted` follows them as the new live set with origin `compaction`, carrying the
    /// session's last turn; the session's proactive prune wait is removed. On any failure the
    /// journal is left as it was.
    ///
    /// # Errors
    /// [`Error::EmptyCompaction`] for an empty `compacted`, [`Error::UnknownSession`],
    /// [`Error::NoTurn`] for a session without a stored turn, [`Error::Json`] or
    /// [`Error::Sqlite`].
    pub fn compact(&mut self, session: u64, compacted: &[Message], at: i64) -> Result<(), Error> {
        if compacted.is_empty() {
            return Err(Error::EmptyCompaction(session));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !exists(&tx, session)? {
            return Err(Error::UnknownSession(session));
        }
        let last = next_turn(&tx, session)? - 1;
        if last == 0 {
            return Err(Error::NoTurn(session));
        }
        archive_live(&tx, session)?;
        let turn = Turn {
            session,
            number: last,
        };
        insert_messages(
            &tx,
            turn,
            at,
            true,
            MessageOrigin::Compaction,
            compacted,
            &HashMap::new(),
        )?;
        store_prune_rearm(&tx, session, 0)?;
        tx.commit()?;
        Ok(())
    }

    /// The record of inbox event `event_id`, if any. Waits at most `within` for a lock another
    /// connection holds.
    ///
    /// # Errors
    /// [`Error::Json`] for a stored delivery that is not a C8 one, or [`Error::Sqlite`],
    /// `SQLITE_BUSY` when the lock outlasts `within`.
    pub fn delivery(
        &self,
        event_id: &str,
        within: Duration,
    ) -> Result<Option<InboxDelivery>, Error> {
        let row = waiting(&self.conn, within, |conn| {
            Ok(conn
                .query_row(
                    "SELECT attempt, turn, delivery FROM deliveries WHERE event_id=?1",
                    [event_id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?.cast_unsigned(),
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()?)
        })?;
        row.map(|(attempt, turn, delivery)| {
            Ok(InboxDelivery {
                event_id: event_id.to_owned(),
                attempt,
                turn,
                delivery: delivery
                    .map(|text| serde_json::from_value(Value::String(text)))
                    .transpose()?,
            })
        })
        .transpose()
    }

    /// Commits `record` as the one of its event, replacing the event's earlier record. Waits at
    /// most `within` for a lock another connection holds.
    ///
    /// # Errors
    /// [`Error::Sqlite`], `SQLITE_BUSY` when the lock outlasts `within`.
    pub fn record_delivery(
        &mut self,
        record: &InboxDelivery,
        within: Duration,
    ) -> Result<(), Error> {
        waiting(&self.conn, within, |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO deliveries (event_id, attempt, turn, delivery) VALUES (?1, ?2, ?3, ?4)",
                params![
                    record.event_id,
                    record.attempt.cast_signed(),
                    record.turn,
                    record.delivery.map(InboxDeliveryDelivery::as_str),
                ],
            )?;
            Ok(())
        })
    }

    /// Deletes the record of `event_id` when it names `turn`. Waits at most `within` for a lock
    /// another connection holds.
    ///
    /// # Errors
    /// [`Error::Sqlite`], `SQLITE_BUSY` when the lock outlasts `within`.
    pub fn forget_delivery(
        &mut self,
        event_id: &str,
        turn: &str,
        within: Duration,
    ) -> Result<(), Error> {
        waiting(&self.conn, within, |conn| {
            conn.execute(
                "DELETE FROM deliveries WHERE event_id=?1 AND turn=?2",
                [event_id, turn],
            )?;
            Ok(())
        })
    }

    /// Commits `delivery` into the record of `event_id` when it names `turn` and has no
    /// delivery yet; false when there is no such record. Waits at most `within` for a lock
    /// another connection holds.
    ///
    /// # Errors
    /// [`Error::Sqlite`], `SQLITE_BUSY` when the lock outlasts `within`.
    pub fn settle_delivery(
        &mut self,
        event_id: &str,
        turn: &str,
        delivery: InboxDeliveryDelivery,
        within: Duration,
    ) -> Result<bool, Error> {
        waiting(&self.conn, within, |conn| {
            let updated = conn.execute(
                "UPDATE deliveries SET delivery=?3 WHERE event_id=?1 AND turn=?2 AND delivery IS NULL",
                [event_id, turn, delivery.as_str()],
            )?;
            Ok(updated == 1)
        })
    }

    fn known(&self, session: u64) -> Result<(), Error> {
        if exists(&self.conn, session)? {
            Ok(())
        } else {
            Err(Error::UnknownSession(session))
        }
    }
}

/// Seconds since the unix epoch, the time the journal writes; a clock set before it reads 0.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed())
}

/// Checks that `key` is a session key: 1 to [`MAX_KEY_CHARS`] chars.
///
/// # Errors
/// [`Error::InvalidKey`].
pub fn check_key(key: &str) -> Result<(), Error> {
    if key.is_empty() || key.chars().count() > MAX_KEY_CHARS {
        return Err(Error::InvalidKey);
    }
    Ok(())
}

/// Runs `op` on `conn` waiting at most `within` for a lock another connection holds.
fn waiting<T>(
    conn: &Connection,
    within: Duration,
    op: impl FnOnce(&Connection) -> Result<T, Error>,
) -> Result<T, Error> {
    conn.busy_timeout(within)?;
    let out = op(conn);
    conn.busy_timeout(BUSY_TIMEOUT)?;
    out
}

fn exists(conn: &Connection, session: u64) -> Result<bool, Error> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sessions WHERE id=?1",
            [session.cast_signed()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Marks the live messages of `session` archived by a compaction.
fn archive_live(conn: &Connection, session: u64) -> Result<(), Error> {
    conn.execute(
        "UPDATE messages SET active=0, compacted=1 WHERE session=?1 AND active=1",
        [session.cast_signed()],
    )?;
    Ok(())
}

/// Stores `tokens` as the wait of the next proactive prune of session `session` (C8
/// `PruneRearm`), or with 0 removes it.
fn store_prune_rearm(conn: &Connection, session: u64, tokens: u64) -> Result<(), Error> {
    let name = format!("{PRUNE_REARM}{session}");
    if tokens == 0 {
        conn.execute("DELETE FROM settings WHERE name=?1", [name])?;
    } else {
        conn.execute(
            "INSERT INTO settings (name, value) VALUES (?1, ?2)
             ON CONFLICT (name) DO UPDATE SET value=excluded.value",
            params![name, tokens.to_string()],
        )?;
    }
    Ok(())
}

/// Inserts `messages` of `turn` written at Unix time `at` with `origin` after the highest `seq`
/// of its session and the highest `n` of the journal, live or archived by a compaction; a `turn`
/// message gets its history index row, its tool results named by the calls in `names`.
fn insert_messages<'a>(
    conn: &Connection,
    turn: Turn,
    at: i64,
    live: bool,
    origin: MessageOrigin,
    messages: impl IntoIterator<Item = &'a Message>,
    names: &HashMap<&str, &str>,
) -> Result<(), Error> {
    let first_seq: i64 = conn.query_row(
        "SELECT count(*) FROM messages WHERE session=?1",
        [turn.session.cast_signed()],
        |r| r.get(0),
    )?;
    let first_n = next_n(conn)?;
    let mut insert = conn.prepare(
        "INSERT INTO messages (session, seq, turn, message, active, compacted, n, at, origin) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut indexed = (origin == MessageOrigin::Turn)
        .then(|| conn.prepare(INDEX))
        .transpose()?;
    for ((seq, n), message) in (first_seq..).zip(first_n..).zip(messages) {
        insert.execute(params![
            turn.session.cast_signed(),
            seq,
            turn.number.cast_signed(),
            serde_json::to_string(message)?,
            live,
            !live,
            n,
            at,
            origin.as_str(),
        ])?;
        if let Some(indexed) = &mut indexed {
            index(indexed, n, message, names)?;
        }
    }
    Ok(())
}

/// Writes the history index row of message `n` with `insert`, a prepared [`INDEX`].
fn index(
    insert: &mut Statement<'_>,
    n: i64,
    message: &Message,
    names: &HashMap<&str, &str>,
) -> Result<(), Error> {
    let text = compress::index_text(message, names)?;
    insert.execute(params![
        n,
        text.content,
        text.tool_name,
        text.tool_calls,
        text.role.as_str()
    ])?;
    Ok(())
}

/// Version 5 to 6, second half: copies the set-aside messages into the version 6 table, numbered
/// in `(session, seq)` order without `at`. A message is `compaction` when it is a handoff summary
/// or its bytes equal those of an archived message of its session with a lower `seq`, as a kept
/// head or tail does; `turn` otherwise.
fn migrate_5(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(MIGRATE_5)?;
    conn.execute_batch(MESSAGES)?;
    {
        let mut select = conn.prepare(
            "SELECT session, seq, turn, message, active, compacted FROM messages5 ORDER BY session, seq",
        )?;
        let mut insert = conn.prepare(
            "INSERT INTO messages (session, seq, turn, message, active, compacted, n, origin) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut rows = select.query([])?;
        let mut session = None;
        let mut archived: HashSet<String> = HashSet::new();
        let mut n: i64 = 0;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let message: String = row.get(3)?;
            let active: bool = row.get(4)?;
            if session != Some(id) {
                session = Some(id);
                archived.clear();
            }
            let origin = if archived.contains(&message)
                || compress::is_summary(&serde_json::from_str(&message)?)
            {
                MessageOrigin::Compaction
            } else {
                MessageOrigin::Turn
            };
            n += 1;
            insert.execute(params![
                id,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                message,
                active,
                row.get::<_, bool>(5)?,
                n,
                origin.as_str(),
            ])?;
            if !active {
                archived.insert(message);
            }
        }
    }
    conn.execute_batch("DROP TABLE messages5;")?;
    Ok(())
}

/// Version 6 to 7: the history index, filled from the `turn` messages of each session with the
/// tool calls of the whole session naming their results.
fn migrate_6(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(FTS)?;
    let mut insert = conn.prepare(INDEX)?;
    let mut select = conn.prepare(
        "SELECT session, n, message, origin = 'turn' FROM messages ORDER BY session, seq",
    )?;
    let mut rows = select.query([])?;
    let mut session: Vec<(i64, Message, bool)> = Vec::new();
    let mut current = None;
    loop {
        let row = rows.next()?;
        let id = row.map(|r| r.get::<_, i64>(0)).transpose()?;
        if id != current && !session.is_empty() {
            let names = compress::tool_names(session.iter().map(|(_, m, _)| m));
            for (n, message, _) in session.iter().filter(|(_, _, turn)| *turn) {
                index(&mut insert, *n, message, &names)?;
            }
            session.clear();
        }
        let Some(row) = row else { break };
        current = id;
        session.push((
            row.get(1)?,
            serde_json::from_str(&row.get::<_, String>(2)?)?,
            row.get(3)?,
        ));
    }
    Ok(())
}

/// The `n` the next inserted message gets: one past the highest of the journal.
fn next_n(conn: &Connection) -> Result<i64, Error> {
    Ok(
        conn.query_row("SELECT coalesce(max(n), 0) + 1 FROM messages", [], |r| {
            r.get(0)
        })?,
    )
}

/// One past the highest turn with a stored message or step; 1 for an empty session.
fn next_turn(conn: &Connection, session: u64) -> Result<u64, Error> {
    let last: Option<i64> = conn.query_row(
        "SELECT max(turn) FROM (SELECT turn FROM messages WHERE session=?1 UNION ALL SELECT turn FROM steps WHERE session=?1)",
        [session.cast_signed()],
        |r| r.get(0),
    )?;
    Ok(last.map_or(1, |t| t.cast_unsigned() + 1))
}

fn object(text: &str) -> Result<Map<String, Value>, Error> {
    Ok(serde_json::from_str(text)?)
}
