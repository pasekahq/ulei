//! The session journal (C8, ADR-0014): sessions, their messages and the steps of their turns
//! in one `SQLite` file. A turn's request ids come from journal state, so a turn after a
//! restart sends the bytes an engine that never stopped would send.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value};
use ulei_contracts::generated::c4g::Message;
use ulei_contracts::generated::c8::{Session, Step};

use crate::agent::Outcome;

/// Schema version of the file, kept in `PRAGMA user_version`.
pub const SCHEMA_VERSION: i64 = 1;
/// Longest session key, as the C4G `session_key`.
pub const MAX_KEY_CHARS: usize = 128;

const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const PRAGMAS: &str = "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-512; PRAGMA foreign_keys=ON;";
const SCHEMA: &str = "CREATE TABLE sessions (id INTEGER PRIMARY KEY, key TEXT NOT NULL);
CREATE INDEX sessions_key ON sessions (key, id);
CREATE TABLE messages (
  session INTEGER NOT NULL REFERENCES sessions (id), seq INTEGER NOT NULL,
  turn INTEGER NOT NULL, message TEXT NOT NULL, PRIMARY KEY (session, seq)) WITHOUT ROWID;
CREATE TABLE steps (
  session INTEGER NOT NULL REFERENCES sessions (id), turn INTEGER NOT NULL,
  step INTEGER NOT NULL, request_id TEXT NOT NULL UNIQUE, route TEXT, usage TEXT,
  PRIMARY KEY (session, turn, step)) WITHOUT ROWID;
PRAGMA user_version=1;";

/// Failure of a journal operation.
#[derive(Debug)]
pub enum Error {
    /// The file carries another schema version than [`SCHEMA_VERSION`].
    SchemaVersion(i64),
    /// The session key is empty or longer than [`MAX_KEY_CHARS`].
    InvalidKey,
    /// No session has this id.
    UnknownSession(u64),
    /// The turn is not the next one of its session: another append came first.
    StaleTurn {
        /// The turn the append named.
        turn: u64,
        /// The session's next turn.
        next: u64,
    },
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
            Self::StaleTurn { turn, next } => {
                write!(f, "turn {turn} is stale, the next turn is {next}")
            }
            Self::Json(e) => write!(f, "journal record: {e}"),
            Self::Sqlite(e) => write!(f, "journal SQLite: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
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
    /// The C4G `request_id` of step `step`: `s<session>-t<turn>-<step>`, at most 64 bytes.
    pub fn request_id(&self, step: usize) -> String {
        format!("s{}-t{}-{step}", self.session, self.number)
    }
}

/// An open journal file.
#[derive(Debug)]
pub struct Journal {
    conn: Connection,
}

impl Journal {
    /// Opens the journal at `path`, creating the file and its schema when missing.
    ///
    /// # Errors
    /// [`Error::SchemaVersion`] when the file has another schema version, or [`Error::Sqlite`].
    pub fn open(path: &Path) -> Result<Self, Error> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut conn = Connection::open_with_flags(path, flags)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch(PRAGMAS)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match tx.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? {
            0 => tx.execute_batch(SCHEMA)?,
            SCHEMA_VERSION => {}
            found => return Err(Error::SchemaVersion(found)),
        }
        tx.commit()?;
        Ok(Self { conn })
    }

    /// The current session of `key`, created empty when the key has none.
    ///
    /// # Errors
    /// [`Error::InvalidKey`] or [`Error::Sqlite`].
    pub fn session(&mut self, key: &str) -> Result<Session, Error> {
        check_key(key)?;
        let current =
            self.conn
                .query_row("SELECT max(id) FROM sessions WHERE key=?1", [key], |r| {
                    r.get::<_, Option<i64>>(0)
                })?;
        match current {
            Some(id) => Ok(Session {
                id: id.cast_unsigned(),
                key: key.to_owned(),
            }),
            None => self.new_session(key),
        }
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
        })
    }

    /// The messages of session `session`, in order.
    ///
    /// # Errors
    /// [`Error::UnknownSession`], [`Error::Json`] for a stored message that is not a C4G
    /// `Message`, or [`Error::Sqlite`].
    pub fn messages(&self, session: u64) -> Result<Vec<Message>, Error> {
        self.known(session)?;
        let mut stmt = self
            .conn
            .prepare("SELECT message FROM messages WHERE session=?1 ORDER BY seq")?;
        let rows = stmt.query_map([session.cast_signed()], |r| r.get::<_, String>(0))?;
        rows.map(|text| Ok(serde_json::from_str(&text?)?)).collect()
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

    /// Appends one turn in one transaction: `input`, the messages the turn started with, then
    /// the messages and steps of `outcome`. On any failure the journal is left as it was.
    ///
    /// # Errors
    /// [`Error::UnknownSession`], [`Error::StaleTurn`] when `turn` is not the session's next
    /// turn, [`Error::Json`] or [`Error::Sqlite`].
    pub fn append(
        &mut self,
        turn: Turn,
        input: &[Message],
        outcome: &Outcome,
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
        let first: i64 = tx.query_row(
            "SELECT count(*) FROM messages WHERE session=?1",
            [turn.session.cast_signed()],
            |r| r.get(0),
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO messages (session, seq, turn, message) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (seq, message) in (first..).zip(input.iter().chain(&outcome.messages)) {
                insert.execute(params![
                    turn.session.cast_signed(),
                    seq,
                    turn.number.cast_signed(),
                    serde_json::to_string(message)?
                ])?;
            }
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
        tx.commit()?;
        Ok(())
    }

    fn known(&self, session: u64) -> Result<(), Error> {
        if exists(&self.conn, session)? {
            Ok(())
        } else {
            Err(Error::UnknownSession(session))
        }
    }
}

fn check_key(key: &str) -> Result<(), Error> {
    if key.is_empty() || key.chars().count() > MAX_KEY_CHARS {
        return Err(Error::InvalidKey);
    }
    Ok(())
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
