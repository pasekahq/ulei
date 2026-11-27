//! Owner inbox store (C1): the owner-only root, `producer.key` and `events.sqlite3` of the
//! Python inbox, with its packet checks byte for byte and its lease state machine (ADR-0007).

mod packet;
pub mod shadow;

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use hmac::{KeyInit, Mac};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::{Map, Value};
use ulei_contracts::generated::c1::{
    AckOutcome, Counts, ReconcileAudit, ReconcileOutcome, UncertainEvent, UncertainReason,
};

pub use packet::{Invalid, canonical};
use packet::{PacketMac, lower_hex};
pub(crate) use packet::{fixed_notation, lower_hex_of_len, valid_id};

/// Seconds a `deferred` event waits before it is offered again.
pub const DEFER_SECONDS: f64 = 3600.0;
/// Claims after which a due `deferred` event becomes `uncertain` (`deferred-exhausted`).
pub const MAX_OFFERS: i64 = 3;
/// Lease length of `claim` and `renew` when the caller has no other.
pub const LEASE_TTL: f64 = 120.0;
/// Delay of `retry` when the caller has no other.
pub const RETRY_DELAY: f64 = 15.0;
/// Most characters of a `reconcile` note (C1 `ReconcileNote`).
pub const MAX_NOTE_CHARS: usize = 500;

const KEY_FILE: &str = "producer.key";
const KEY_LEN: usize = 32;
/// The database file of a root.
pub const DB_FILE: &str = "events.sqlite3";
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
const PRAGMAS: &str =
    "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-512;";
// The Python text byte for byte, indentation included: SQLite keeps it as `sqlite_master.sql`.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS events (
                id TEXT PRIMARY KEY, packet BLOB NOT NULL, digest TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
                lease TEXT, lease_until REAL, available REAL NOT NULL DEFAULT 0,
                session_id TEXT, outcome TEXT, created REAL NOT NULL)";
// One audit line per event closed by `reconcile`, beside the Python table, which it never reads.
const AUDIT_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS reconciled (
    id TEXT PRIMARY KEY, outcome TEXT NOT NULL, reason TEXT NOT NULL, note TEXT NOT NULL,
    uid INTEGER NOT NULL, pid INTEGER NOT NULL, time REAL NOT NULL)";
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
    /// The divergence file `--out` of the shadow.
    Output,
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
    /// The stored `packet` column lacks a field its reader needs: a string `payload`, or for
    /// the model text also a string `kind` and a numeric `source_ts`.
    StoredPacket,
    /// The `ack` outcome is not a C1 `AckOutcome`.
    InvalidOutcome,
    /// A stored `status` is not a C1 `Status`.
    UnknownStatus,
    /// `reconcile` met an event that is not `uncertain`: its stored status and outcome.
    NotUncertain {
        /// The stored `status`.
        status: String,
        /// The stored `outcome`.
        outcome: Option<String>,
    },
    /// A `reconcile` note is blank, has a control character or is over [`MAX_NOTE_CHARS`].
    InvalidNote,
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
            Self::StoredPacket => f.write_str("stored packet lacks a field of the right type"),
            Self::InvalidOutcome => f.write_str("ack outcome is not a C1 AckOutcome"),
            Self::UnknownStatus => f.write_str("stored status is not a C1 Status"),
            Self::NotUncertain { status, outcome } => match outcome {
                Some(outcome) => write!(f, "event is {status} ({outcome}), not uncertain"),
                None => write!(f, "event is {status}, not uncertain"),
            },
            Self::InvalidNote => write!(
                f,
                "note must be 1 to {MAX_NOTE_CHARS} characters, not blank, without control characters"
            ),
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

/// The caller of a write, as the socket peer's credentials name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    /// Peer uid.
    pub uid: u32,
    /// Peer pid.
    pub pid: u32,
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
        let uid = owner_root(root)?;
        let key_path = root.join(KEY_FILE);
        create_key(&key_path)?;
        let mac = read_key(&key_path, uid)?;
        Self::with_mac(root, mac)
    }

    /// Opens `root` like [`Inbox::open`] but verifies MACs with `mac` and never creates or
    /// reads a key file there.
    fn open_with_mac(root: &Path, mac: PacketMac) -> Result<Self, Error> {
        owner_root(root)?;
        Self::with_mac(root, mac)
    }

    fn with_mac(root: &Path, mac: PacketMac) -> Result<Self, Error> {
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
        conn.execute_batch(AUDIT_SCHEMA)?;
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

    /// Stores `packet`, the six C1 fields without `mac`, signed with the producer key, as
    /// [`Inbox::publish`] does.
    ///
    /// # Errors
    /// As [`Inbox::publish`].
    pub fn publish_own(
        &mut self,
        mut packet: Map<String, Value>,
        now: f64,
    ) -> Result<String, Error> {
        let signed = packet::signed_bytes(&Value::Object(packet.clone()))?;
        let mut mac = self.mac.clone();
        mac.update(&signed);
        let tag = lower_hex(&mac.finalize().into_bytes());
        packet.insert("mac".to_owned(), Value::String(tag));
        self.publish(&Value::Object(packet), now)
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

    /// The number of events in each status except `done` and of `reconcile` audit lines, from
    /// one read that changes no row.
    ///
    /// # Errors
    /// [`Error::UnknownStatus`] or [`Error::Sqlite`].
    pub fn counts(&self) -> Result<Counts, Error> {
        let mut reconciled = 0;
        let mut counts = Counts {
            pending: 0,
            leased: 0,
            processing: 0,
            deferred: 0,
            uncertain: 0,
            reconciled: None,
        };
        let mut query = self.conn.prepare(
            "SELECT status, COUNT(*) FROM events WHERE status != 'done' GROUP BY status \
             UNION ALL SELECT NULL, COUNT(*) FROM reconciled",
        )?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let status: Option<String> = row.get(0)?;
            let count: i64 = row.get(1)?;
            let slot = match status.as_deref() {
                None => &mut reconciled,
                Some("pending") => &mut counts.pending,
                Some("leased") => &mut counts.leased,
                Some("processing") => &mut counts.processing,
                Some("deferred") => &mut counts.deferred,
                Some("uncertain") => &mut counts.uncertain,
                Some(_) => return Err(Error::UnknownStatus),
            };
            *slot = count.unsigned_abs();
        }
        counts.reconciled = Some(reconciled);
        Ok(counts)
    }

    /// Closes the `uncertain` event `id` for `by` at `now` in one `BEGIN IMMEDIATE`
    /// transaction: the row becomes `done` with `outcome` and its audit line is stored; an
    /// error changes nothing.
    ///
    /// # Errors
    /// [`Error::InvalidNote`], [`Error::NotFound`], [`Error::NotUncertain`],
    /// [`Error::StoredPacket`] for an `uncertain` row without an `UncertainReason`, or
    /// [`Error::Sqlite`].
    pub fn reconcile(
        &mut self,
        id: &str,
        outcome: ReconcileOutcome,
        note: &str,
        by: Peer,
        now: f64,
    ) -> Result<ReconcileAudit, Error> {
        if !valid_note(note) {
            return Err(Error::InvalidNote);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (status, stored): (String, Option<String>) = tx
            .query_row(
                "SELECT status, outcome FROM events WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(Error::NotFound)?;
        if status != "uncertain" {
            return Err(Error::NotUncertain {
                status,
                outcome: stored,
            });
        }
        let reason: UncertainReason = stored
            .and_then(|o| serde_json::from_value(Value::from(o)).ok())
            .ok_or(Error::StoredPacket)?;
        tx.execute(
            "UPDATE events SET status='done',outcome=?1 WHERE id=?2 AND status='uncertain'",
            params![outcome.as_str(), id],
        )?;
        tx.execute(
            "INSERT INTO reconciled(id, outcome, reason, note, uid, pid, time) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                outcome.as_str(),
                reason.as_str(),
                note,
                by.uid,
                by.pid,
                now
            ],
        )?;
        tx.commit()?;
        Ok(ReconcileAudit {
            id: id.to_owned(),
            outcome,
            reason,
            note: note.to_owned(),
            uid: by.uid.into(),
            pid: by.pid.into(),
            time: now,
        })
    }

    /// At most `limit` events with status `uncertain`, oldest first (by `created`, then `id`),
    /// from one read that changes no row; each carries no payload.
    ///
    /// # Errors
    /// [`Error::StoredPacket`] for a packet without a C1 `kind` or a row without an
    /// `uncertain` outcome, or [`Error::Sqlite`].
    pub fn uncertain(&self, limit: u32) -> Result<Vec<UncertainEvent>, Error> {
        let mut query = self.conn.prepare(
            "SELECT id, packet, created, outcome FROM events WHERE status = 'uncertain' \
             ORDER BY created, id LIMIT ?1",
        )?;
        let mut rows = query.query([limit])?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let packet: Vec<u8> = row.get(1)?;
            let outcome: Option<String> = row.get(3)?;
            let packet: Value = serde_json::from_slice(&packet).map_err(|_| Error::StoredPacket)?;
            let kind =
                serde_json::from_value(packet["kind"].clone()).map_err(|_| Error::StoredPacket)?;
            let reason = outcome
                .and_then(|outcome| serde_json::from_value(Value::from(outcome)).ok())
                .ok_or(Error::StoredPacket)?;
            let source = id
                .split_once(':')
                .map_or(id.as_str(), |(head, _)| head)
                .to_owned();
            events.push(UncertainEvent {
                source,
                kind,
                time: row.get(2)?,
                reason,
                id,
            });
        }
        Ok(events)
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

    /// Whether a `pending` event is available at `now`, the events [`Inbox::claim`] takes.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn claimable(&self, now: f64) -> Result<bool, Error> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE status='pending' AND available<=?1)",
            [now],
            |r| r.get(0),
        )?)
    }

    /// The nearest time after `now` at which [`Inbox::recover`] or [`Inbox::claim`] may find
    /// work: the smallest future `available` of a `pending` or `deferred` event and the
    /// smallest `lease_until` of a `leased` or `processing` one not expired at `now`; `None`
    /// when no row has one.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn next_due(&self, now: f64) -> Result<Option<f64>, Error> {
        Ok(self.conn.query_row(
            "SELECT min(due) FROM (
                SELECT available AS due FROM events WHERE status IN ('pending','deferred') AND available>?1
                UNION ALL SELECT lease_until FROM events WHERE status IN ('leased','processing') AND lease_until>=?1)",
            [now],
            |r| r.get(0),
        )?)
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
        // Python writes the claimed row's `available`; while this lease is held it equals the
        // stored column, because only `retry`, `ack` and `defer` change it and each ends the lease.
        let deferred_until = (outcome == AckOutcome::Deferred).then_some(now + DEFER_SECONDS);
        self.settle(id, lease, outcome, deferred_until)
    }

    /// `ack` `deferred` of the `processing` event `id` holding `lease`, available again at
    /// `until` instead of `now +` [`DEFER_SECONDS`]. True when that row changed.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn defer(&mut self, id: &str, lease: &str, until: f64) -> Result<bool, Error> {
        self.settle(id, lease, AckOutcome::Deferred, Some(until))
    }

    /// Settles the `processing` event `id` holding `lease` with `outcome`, and `available` when
    /// given.
    fn settle(
        &mut self,
        id: &str,
        lease: &str,
        outcome: AckOutcome,
        available: Option<f64>,
    ) -> Result<bool, Error> {
        let status = match outcome {
            AckOutcome::Success | AckOutcome::Silent => "done",
            AckOutcome::Uncertain | AckOutcome::Deferred => outcome.as_str(),
        };
        let changed = self.conn.execute(
            "UPDATE events SET status=?1,outcome=?2,available=coalesce(?3,available) WHERE id=?4 AND lease=?5 AND status='processing'",
            params![status, outcome.as_str(), available, id, lease],
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

/// Creates `root` 0700 when missing and returns the effective uid once `root` is owner-only.
fn owner_root(root: &Path) -> Result<u32, Error> {
    let uid = own_uid()?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)?;
    guard(root, Guarded::Root, uid)?;
    Ok(uid)
}

/// The MAC of the owner-only 32-byte key file at `path`, opened for reading only.
fn read_key(path: &Path, uid: u32) -> Result<PacketMac, Error> {
    guard(path, Guarded::Key, uid)?;
    let key = fs::read(path)?;
    if key.len() != KEY_LEN {
        return Err(Error::KeyLength(key.len()));
    }
    PacketMac::new_from_slice(&key).map_err(|_| Error::KeyLength(key.len()))
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

/// Whether `note` is a C1 `ReconcileNote`: not blank, no control character, at most
/// [`MAX_NOTE_CHARS`] characters.
#[must_use]
pub fn valid_note(note: &str) -> bool {
    !note.trim().is_empty()
        && !note.chars().any(char::is_control)
        && note.chars().count() <= MAX_NOTE_CHARS
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
///
/// # Errors
/// The socketpair or its credentials are unavailable.
pub fn own_uid() -> io::Result<u32> {
    let (ours, _peer) = UnixStream::pair()?;
    let cred = rustix::net::sockopt::socket_peercred(&ours).map_err(io::Error::from)?;
    Ok(cred.uid.as_raw())
}

#[cfg(test)]
mod tests;
