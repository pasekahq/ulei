//! Owner inbox store (C1): the owner-only root, `producer.key` and `events.sqlite3` of the
//! Python inbox, with its packet checks byte for byte and its lease state machine (ADR-0007).

mod packet;

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use hmac::KeyInit;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::Value;
use ulei_contracts::generated::c1::AckOutcome;

pub use packet::{Invalid, canonical};
use packet::{PacketMac, lower_hex};

/// Seconds a `deferred` event waits before it is offered again.
pub const DEFER_SECONDS: f64 = 3600.0;
/// Claims after which a due `deferred` event becomes `uncertain` (`deferred-exhausted`).
pub const MAX_OFFERS: i64 = 3;
/// Lease length of `claim` and `renew` when the caller has no other.
pub const LEASE_TTL: f64 = 120.0;
/// Delay of `retry` when the caller has no other.
pub const RETRY_DELAY: f64 = 15.0;

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
const LEASE_LEN: usize = 16;
const ACK_OUTCOMES: [AckOutcome; 4] = [
    AckOutcome::Success,
    AckOutcome::Silent,
    AckOutcome::Uncertain,
    AckOutcome::Deferred,
];

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
    /// The `ack` outcome is not a C1 `AckOutcome`.
    InvalidOutcome,
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
            Self::InvalidOutcome => f.write_str("ack outcome is not a C1 AckOutcome"),
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
        Ok(self.conn.query_row(&sql, [id], event_row).optional()?)
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

    /// Python `recover(now)` in one transaction: an expired `leased` event returns to
    /// `pending` without a lease, an expired `processing` one becomes `uncertain`
    /// (`expired-processing`), and a due `deferred` one becomes `uncertain`
    /// (`deferred-exhausted`) after [`MAX_OFFERS`] claims, else returns to `pending`.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn recover(&mut self, now: f64) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE events SET status='pending',lease=NULL WHERE status='leased' AND lease_until<?1",
            [now],
        )?;
        tx.execute(
            "UPDATE events SET status='uncertain',outcome='expired-processing' WHERE status='processing' AND lease_until<?1",
            [now],
        )?;
        tx.execute(
            "UPDATE events SET status='uncertain',outcome='deferred-exhausted' WHERE status='deferred' AND available<=?1 AND attempts>=?2",
            params![now, MAX_OFFERS],
        )?;
        tx.execute(
            "UPDATE events SET status='pending',lease=NULL WHERE status='deferred' AND available<=?1 AND attempts<?2",
            params![now, MAX_OFFERS],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Python `claim(now, ttl)`: runs [`Inbox::recover`], then in one `BEGIN IMMEDIATE`
    /// transaction leases the oldest pending event available at `now` (by `created`, then
    /// `id`) with a fresh token until `now + ttl` and one more attempt; `None` when none is.
    ///
    /// # Errors
    /// [`Error::Io`] reading `/dev/urandom` or [`Error::Sqlite`].
    pub fn claim(&mut self, now: f64, ttl: f64) -> Result<Option<EventRow>, Error> {
        self.recover(now)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id: Option<String> = tx
            .query_row(
                "SELECT id FROM events WHERE status='pending' AND available<=?1 ORDER BY created,id LIMIT 1",
                [now],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            return Ok(None);
        };
        let token = lower_hex(&random_bytes::<LEASE_LEN>()?);
        tx.execute(
            "UPDATE events SET status='leased',lease=?1,lease_until=?2,attempts=attempts+1 WHERE id=?3",
            params![token, now + ttl, id],
        )?;
        let sql = format!("SELECT {ROW_COLUMNS} FROM events WHERE id = ?1");
        let row = tx.query_row(&sql, [&id], event_row)?;
        tx.commit()?;
        Ok(Some(row))
    }

    /// Python `start(row, session_id)`: moves the `leased` event `id` holding `lease` to
    /// `processing` with `session_id`; true when that row changed.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn start(&mut self, id: &str, lease: &str, session_id: &str) -> Result<bool, Error> {
        let changed = self.conn.execute(
            "UPDATE events SET status='processing',session_id=?1 WHERE id=?2 AND lease=?3 AND status='leased'",
            params![session_id, id, lease],
        )?;
        Ok(changed == 1)
    }

    /// Python `renew(row, now, ttl)`: extends the lease of the `leased` or `processing` event
    /// `id` holding `lease` to `now + ttl`; true when that row changed.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn renew(&mut self, id: &str, lease: &str, now: f64, ttl: f64) -> Result<bool, Error> {
        let changed = self.conn.execute(
            "UPDATE events SET lease_until=?1 WHERE id=?2 AND lease=?3 AND status IN ('leased','processing')",
            params![now + ttl, id, lease],
        )?;
        Ok(changed == 1)
    }

    /// Python `retry(row, now, delay)`: returns the `leased` event `id` holding `lease` to
    /// `pending` without a lease, claimable from `now + delay`; true when that row changed.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn retry(&mut self, id: &str, lease: &str, now: f64, delay: f64) -> Result<bool, Error> {
        let changed = self.conn.execute(
            "UPDATE events SET status='pending',available=?1,lease=NULL WHERE id=?2 AND lease=?3 AND status='leased'",
            params![now + delay, id, lease],
        )?;
        Ok(changed == 1)
    }

    /// Python `ack(row, outcome)` at time `now`: settles the `processing` event `id` holding
    /// `lease` with `outcome`, status `done` for `success` and `silent`, else the outcome
    /// itself; `deferred` becomes available at `now +` [`DEFER_SECONDS`]. True when that row
    /// changed.
    ///
    /// # Errors
    /// [`Error::InvalidOutcome`] before any change, or [`Error::Sqlite`].
    pub fn ack(&mut self, id: &str, lease: &str, outcome: &str, now: f64) -> Result<bool, Error> {
        let outcome = ACK_OUTCOMES
            .into_iter()
            .find(|o| o.as_str() == outcome)
            .ok_or(Error::InvalidOutcome)?;
        let status = match outcome {
            AckOutcome::Success | AckOutcome::Silent => "done",
            AckOutcome::Uncertain | AckOutcome::Deferred => outcome.as_str(),
        };
        // Python writes the claimed row's `available`; while this lease is held it equals the
        // stored column, because only `retry` and `ack` change it and both end the lease.
        let deferred_until = (outcome == AckOutcome::Deferred).then_some(now + DEFER_SECONDS);
        let changed = self.conn.execute(
            "UPDATE events SET status=?1,outcome=?2,available=coalesce(?3,available) WHERE id=?4 AND lease=?5 AND status='processing'",
            params![status, outcome.as_str(), deferred_until, id, lease],
        )?;
        Ok(changed == 1)
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
    file.write_all(&random_bytes::<KEY_LEN>()?)?;
    file.sync_all()?;
    Ok(())
}

fn random_bytes<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0u8; N];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn event_row(r: &Row<'_>) -> rusqlite::Result<EventRow> {
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
}

/// Effective uid of this process: std has no `geteuid`, and a socketpair peer is ourselves.
fn own_uid() -> Result<u32, Error> {
    let (ours, _peer) = UnixStream::pair()?;
    let cred = rustix::net::sockopt::socket_peercred(&ours).map_err(io::Error::from)?;
    Ok(cred.uid.as_raw())
}

#[cfg(test)]
mod tests;
