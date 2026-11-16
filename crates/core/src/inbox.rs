//! Owner inbox store (C1): the owner-only root, `producer.key` and `events.sqlite3` of the
//! Python inbox, with its packet checks byte for byte (ADR-0007).

mod packet;

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use hmac::KeyInit;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;

use packet::PacketMac;
pub use packet::{Invalid, canonical};

const KEY_FILE: &str = "producer.key";
const KEY_LEN: usize = 32;
const DB_FILE: &str = "events.sqlite3";
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const PRAGMAS: &str =
    "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-512;";
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY, packet BLOB NOT NULL, digest TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
    lease TEXT, lease_until REAL, available REAL NOT NULL DEFAULT 0,
    session_id TEXT, outcome TEXT, created REAL NOT NULL)";
const ROW_COLUMNS: &str = "id, packet, digest, status, attempts, lease, lease_until, available, session_id, outcome, created";

/// A path the store requires to be owner-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guarded {
    /// The inbox root directory.
    Root,
    /// `producer.key`.
    Key,
    /// `events.sqlite3`.
    Database,
}

/// Failure of an inbox operation.
#[derive(Debug)]
pub enum Error {
    /// The path is a symlink.
    Symlink(Guarded),
    /// The path is owned by another uid.
    ForeignOwner(Guarded),
    /// The path has a group or other permission bit.
    GroupOrOtherMode(Guarded),
    /// `producer.key` holds this many bytes instead of 32.
    KeyLength(usize),
    /// The packet MAC does not verify under the producer key.
    Unauthenticated,
    /// The packet fields or values are invalid.
    InvalidPacket(Invalid),
    /// The id is stored with a different digest.
    Conflict,
    /// No event has the id.
    NotFound,
    /// The stored `packet` column is not a packet with a string `payload`.
    StoredPacket,
    /// Filesystem failure.
    Io(io::Error),
    /// `SQLite` failure.
    Sqlite(rusqlite::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Symlink(item) => write!(f, "{item:?} must not be a symlink"),
            Self::ForeignOwner(item) => write!(f, "{item:?} must be owned by this uid"),
            Self::GroupOrOtherMode(item) => {
                write!(f, "{item:?} must have no group or other mode bits")
            }
            Self::KeyLength(len) => write!(f, "producer key has {len} bytes, not {KEY_LEN}"),
            Self::Unauthenticated => f.write_str("packet MAC does not verify"),
            Self::InvalidPacket(why) => write!(f, "invalid packet: {why:?}"),
            Self::Conflict => f.write_str("event id already has different content"),
            Self::NotFound => f.write_str("event not found"),
            Self::StoredPacket => f.write_str("stored packet has no string payload"),
            Self::Io(e) => write!(f, "inbox I/O: {e}"),
            Self::Sqlite(e) => write!(f, "inbox SQLite: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

/// One `events` row as stored, every column.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    /// Event id, the primary key.
    pub id: String,
    /// Canonical JSON of the packet with `mac`.
    pub packet: Vec<u8>,
    /// Lowercase hex SHA-256 of the canonical packet without `mac`.
    pub digest: String,
    /// Delivery status.
    pub status: String,
    /// Number of claims so far.
    pub attempts: i64,
    /// Current or last lease token.
    pub lease: Option<String>,
    /// Unix time the lease expires.
    pub lease_until: Option<f64>,
    /// Unix time from which a pending event may be claimed.
    pub available: f64,
    /// Session given to `start`.
    pub session_id: Option<String>,
    /// Settled outcome.
    pub outcome: Option<String>,
    /// Unix time the inbox stored the event.
    pub created: f64,
}

/// The inbox store over one root directory, holding one `SQLite` connection.
pub struct Inbox {
    mac: PacketMac,
    conn: Connection,
}

impl Inbox {
    /// Opens `root` as the Python `Inbox.__init__` does: creates the root 0700 and the key
    /// once, refuses non-owner-only paths, creates table `events` and chmods the database 0600.
    ///
    /// # Errors
    /// A guarded path refusal, [`Error::KeyLength`], or an I/O or `SQLite` failure.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, Error> {
        let root = root.as_ref();
        let uid = own_uid()?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        guard(root, Guarded::Root, uid)?;
        let key_path = root.join(KEY_FILE);
        create_key(&key_path)?;
        guard(&key_path, Guarded::Key, uid)?;
        let key = fs::read(&key_path)?;
        if key.len() != KEY_LEN {
            return Err(Error::KeyLength(key.len()));
        }
        let mac = PacketMac::new_from_slice(&key).map_err(|_| Error::KeyLength(key.len()))?;
        let db_path = root.join(DB_FILE);
        match fs::symlink_metadata(&db_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Error::Symlink(Guarded::Database));
            }
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let conn = connect(&db_path)?;
        conn.execute_batch(SCHEMA)?;
        fs::set_permissions(&db_path, fs::Permissions::from_mode(0o600))?;
        Ok(Self { mac, conn })
    }

    /// Stores a signed packet and returns its id; `now` is the Unix time of the call.
    /// The same packet again succeeds without a change; the same id with another digest is
    /// [`Error::Conflict`] and leaves the stored row untouched.
    ///
    /// # Errors
    /// [`Error::InvalidPacket`], [`Error::Unauthenticated`], [`Error::Conflict`] or `SQLite`.
    pub fn publish(&mut self, packet: &Value, now: f64) -> Result<String, Error> {
        let checked = packet::check(packet, &self.mac, now)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO events(id, packet, digest, created) VALUES (?1, ?2, ?3, ?4)",
            params![checked.id, checked.stored, checked.digest, now],
        )?;
        let stored: String = tx.query_row(
            "SELECT digest FROM events WHERE id = ?1",
            [checked.id],
            |row| row.get(0),
        )?;
        if stored != checked.digest {
            return Err(Error::Conflict);
        }
        tx.commit()?;
        Ok(checked.id.to_owned())
    }

    /// [`Inbox::publish`] of a packet given as JSON text; text `serde_json` cannot hold is
    /// [`Invalid::Json`].
    ///
    /// # Errors
    /// As [`Inbox::publish`].
    pub fn publish_json(&mut self, packet: &[u8], now: f64) -> Result<String, Error> {
        let packet: Value =
            serde_json::from_slice(packet).map_err(|_| Error::InvalidPacket(Invalid::Json))?;
        self.publish(&packet, now)
    }

    /// The stored row of `id`, or `None` when the id is unknown.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn get(&self, id: &str) -> Result<Option<EventRow>, Error> {
        let sql = format!("SELECT {ROW_COLUMNS} FROM events WHERE id = ?1");
        let row = self
            .conn
            .query_row(&sql, [id], |r| {
                Ok(EventRow {
                    id: r.get(0)?,
                    packet: r.get(1)?,
                    digest: r.get(2)?,
                    status: r.get(3)?,
                    attempts: r.get(4)?,
                    lease: r.get(5)?,
                    lease_until: r.get(6)?,
                    available: r.get(7)?,
                    session_id: r.get(8)?,
                    outcome: r.get(9)?,
                    created: r.get(10)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// The payload of the stored packet of `id`.
    ///
    /// # Errors
    /// [`Error::NotFound`], [`Error::StoredPacket`] or [`Error::Sqlite`].
    pub fn payload(&self, id: &str) -> Result<String, Error> {
        let row = self.get(id)?.ok_or(Error::NotFound)?;
        let packet: Value = serde_json::from_slice(&row.packet).map_err(|_| Error::StoredPacket)?;
        packet["payload"]
            .as_str()
            .map(str::to_owned)
            .ok_or(Error::StoredPacket)
    }
}

/// Opens `events.sqlite3` with the settings of ADR-0007 section 4; every connection uses it.
fn connect(path: &Path) -> Result<Connection, Error> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch(PRAGMAS)?;
    Ok(conn)
}

fn guard(path: &Path, item: Guarded, uid: u32) -> Result<(), Error> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(Error::Symlink(item));
    }
    if meta.uid() != uid {
        return Err(Error::ForeignOwner(item));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(Error::GroupOrOtherMode(item));
    }
    Ok(())
}

/// Writes 32 random bytes to a new key file with mode 0600 and fsyncs it; an existing name
/// is left alone. `create_new` is `O_CREAT | O_EXCL`, which also fails on a symlink.
fn create_key(path: &Path) -> Result<(), Error> {
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut key = [0u8; KEY_LEN];
    fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
    file.write_all(&key)?;
    file.sync_all()?;
    Ok(())
}

/// Effective uid of this process: std has no `geteuid`, and a socketpair peer is ourselves.
fn own_uid() -> Result<u32, Error> {
    let (ours, _peer) = UnixStream::pair()?;
    let cred = rustix::net::sockopt::socket_peercred(&ours).map_err(io::Error::from)?;
    Ok(cred.uid.as_raw())
}

#[cfg(test)]
mod tests;
