//! One shadow pass of ADR-0007 section 7: reads the live Python inbox read-only, replays every
//! row through the Rust packet checks and store, checks the changes since the previous
//! snapshot and appends each divergence to a JSONL file.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OpenFlags, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ulei_contracts::generated::c1::{Event, RowOutcome, Status};

use super::packet::{digest_hex, lower_hex, signed_bytes, verify};
use super::{
    DB_FILE, Error, EventRow, Guarded, Inbox, Invalid, KEY_FILE, ROW_COLUMNS, canonical, event_row,
    lower_hex_of_len, own_uid, read_key, valid_id,
};

/// The live columns in `ROW_COLUMNS` order, as named in divergence details.
const COLUMNS: [&str; 11] = [
    "id",
    "packet",
    "digest",
    "status",
    "attempts",
    "lease",
    "lease_until",
    "available",
    "session_id",
    "outcome",
    "created",
];
/// The edges of the Python lease state machine.
const EDGES: [(&str, &str); 8] = [
    ("pending", "leased"),
    ("leased", "pending"),
    ("leased", "processing"),
    ("processing", "done"),
    ("processing", "uncertain"),
    ("processing", "deferred"),
    ("deferred", "pending"),
    ("deferred", "uncertain"),
];
const SNAPSHOT_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS snapshot (
    id TEXT PRIMARY KEY, packet BLOB NOT NULL, digest TEXT NOT NULL, status TEXT NOT NULL,
    attempts INTEGER NOT NULL, lease TEXT, lease_until REAL, available REAL NOT NULL,
    session_id TEXT, outcome TEXT, created REAL NOT NULL)";
const MAX_PAYLOAD_CHARS: usize = 65_536;

/// A divergence kind: the `check` of a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// `canonical(packet)` differs from the stored bytes.
    PacketBytes,
    /// The `digest` column differs from the recomputed digest.
    Digest,
    /// The MAC does not verify under the live producer key.
    Mac,
    /// The Rust store refuses the packet or keeps other bytes or another digest.
    Publish,
    /// The row is not valid for C1 `Row`, or a snapshot row is gone from live.
    Row,
    /// A changed row follows no path of the lease state machine.
    Transition,
    /// `packet`, `digest` or `created` changed, or `attempts` decreased.
    Immutable,
}

impl Check {
    /// The `check` value of a record.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PacketBytes => "packet_bytes",
            Self::Digest => "digest",
            Self::Mac => "mac",
            Self::Publish => "publish",
            Self::Row => "row",
            Self::Transition => "transition",
            Self::Immutable => "immutable",
        }
    }
}

/// Failure of the shadow pass itself; no divergence is appended and the snapshot is unchanged.
#[derive(Debug)]
pub enum Failure {
    /// The live database is locked or has a hot journal a read-only reader may not roll back.
    Retry(rusqlite::Error),
    /// `--state` or the directory of `--out` lies inside the live root.
    InsideLive(PathBuf),
    /// The live key, the shadow store or the output failed.
    Store(Error),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retry(e) => write!(
                f,
                "live database cannot be read consistently now, retry later: {e}"
            ),
            Self::InsideLive(path) => {
                write!(f, "{} lies inside the live root", path.display())
            }
            Self::Store(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Failure {}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Self::Store(e)
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Self::Store(Error::Io(e))
    }
}

impl From<rusqlite::Error> for Failure {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(Error::Sqlite(e))
    }
}

/// Counts of one completed pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pass {
    /// Live rows read.
    pub rows: usize,
    /// Divergence records appended to the output.
    pub divergences: usize,
}

/// One shadow pass at Unix time `now`: reads `live_root` read-only, appends one canonical
/// JSON line per divergence to `out` and commits the validated rows to the snapshot under
/// `state` in one transaction after the lines are written.
///
/// # Errors
/// [`Failure::Retry`] before anything under `state` or `out` is touched, else
/// [`Failure::InsideLive`] or [`Failure::Store`].
pub fn run(live_root: &Path, state: &Path, out: &Path, now: f64) -> Result<Pass, Failure> {
    let live_dir = fs::canonicalize(live_root)?;
    let out_dir = out
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    for path in [state, out_dir] {
        if existing_ancestor(path)?.starts_with(&live_dir) {
            return Err(Failure::InsideLive(path.to_owned()));
        }
    }
    refuse_symlink(out)?;
    let mac = read_key(&live_dir.join(KEY_FILE), own_uid()?)?;
    let live = read_live(&live_dir.join(DB_FILE))?;
    let mut store = Inbox::open_with_mac(state, mac)?;
    store.conn.execute_batch(SNAPSHOT_SCHEMA)?;
    let snapshot = read_snapshot(&store.conn)?;

    let mut found = Vec::new();
    let mut validated = Vec::new();
    for raw in &live {
        let before = found.len();
        let row = match typed(raw) {
            Ok(row) => row,
            Err(column) => {
                found.push(wrong_type(raw, column));
                continue;
            }
        };
        check_packet(&mut store, &row, &mut found)?;
        check_row(&row, &mut found);
        if let Some(old) = snapshot.get(&row.id) {
            check_change(old, &row, &mut found);
        }
        if found.len() == before && snapshot.get(&row.id) != Some(&row) {
            validated.push(row);
        }
    }
    let live_ids: BTreeSet<&str> = live
        .iter()
        .filter_map(|raw| match &raw[0] {
            Sql::Text(id) => Some(id.as_str()),
            _ => None,
        })
        .collect();
    for (id, old) in &snapshot {
        if !live_ids.contains(id.as_str()) {
            found.push(Divergence {
                check: Check::Row,
                detail: "row of the snapshot is gone from live".to_owned(),
                id: Value::from(id.as_str()),
                live: Value::Null,
                rust: Value::from(old.digest.as_str()),
            });
        }
    }

    append(out, &found, now)?;
    let tx = store.conn.transaction()?;
    for row in &validated {
        tx.execute(
            &format!("INSERT OR REPLACE INTO snapshot({ROW_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"),
            params![
                row.id,
                row.packet,
                row.digest,
                row.status,
                row.attempts,
                row.lease,
                row.lease_until,
                row.available,
                row.session_id,
                row.outcome,
                row.created
            ],
        )?;
    }
    tx.commit()?;
    Ok(Pass {
        rows: live.len(),
        divergences: found.len(),
    })
}

/// The snapshot under `state`: per id the last live row a pass validated, ordered by id.
///
/// # Errors
/// [`Error::Sqlite`], also when `state` holds no shadow store.
pub fn snapshot(state: &Path) -> Result<Vec<EventRow>, Error> {
    let conn = Connection::open_with_flags(
        state.join(DB_FILE),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    Ok(read_snapshot(&conn)?.into_values().collect())
}

struct Divergence {
    check: Check,
    detail: String,
    id: Value,
    live: Value,
    rust: Value,
}

/// The canonical JSON of the record, keys in code point order. Floats print as `serde_json`
/// does, so a live value Python would print with an exponent cannot fail the pass.
fn record(d: &Divergence, now: f64) -> Vec<u8> {
    let mut line = json!({
        "check": d.check.as_str(),
        "detail": d.detail,
        "id": d.id,
        "live": d.live,
        "rust": d.rust,
        "ts": now,
    })
    .to_string()
    .into_bytes();
    line.push(b'\n');
    line
}

fn append(out: &Path, found: &[Divergence], now: f64) -> Result<(), Failure> {
    if found.is_empty() {
        return Ok(());
    }
    let lines: Vec<u8> = found.iter().flat_map(|d| record(d, now)).collect();
    refuse_symlink(out)?;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(out)?;
    // Without `O_NOFOLLOW` in reach, a symlink swapped in after the check above is caught
    // before any byte goes through it: the name must still be the file that was opened.
    let opened = file.metadata()?;
    let named = fs::symlink_metadata(out)?;
    if named.file_type().is_symlink() || (named.dev(), named.ino()) != (opened.dev(), opened.ino())
    {
        return Err(Error::Symlink(Guarded::Output).into());
    }
    file.write_all(&lines)?;
    file.sync_data()?;
    Ok(())
}

/// Refuses an `out` that is a symlink; a missing `out` is fine, `append` creates it.
fn refuse_symlink(out: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(out) {
        Ok(meta) if meta.file_type().is_symlink() => Err(Error::Symlink(Guarded::Output)),
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// The canonical form of the nearest existing ancestor of `path`, `path` itself included.
fn existing_ancestor(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let mut ancestors = path.ancestors();
    loop {
        match ancestors.next() {
            Some(dir) => match fs::canonicalize(dir) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                found => return found,
            },
            None => return Err(io::ErrorKind::NotFound.into()),
        }
    }
}

/// Every `events` row, each column as stored, in one read transaction of a read-only
/// connection that never waits for a lock.
fn read_live(db: &Path) -> Result<Vec<Vec<Sql>>, Failure> {
    let uri = read_only_uri(db)?;
    let rows = || -> rusqlite::Result<Vec<Vec<Sql>>> {
        let conn = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(Duration::ZERO)?;
        conn.execute_batch("PRAGMA query_only=1")?;
        let tx = conn.unchecked_transaction()?;
        let rows = tx
            .prepare(&format!("SELECT {ROW_COLUMNS} FROM events ORDER BY id"))?
            .query_map([], |r| (0..COLUMNS.len()).map(|i| r.get(i)).collect())?
            .collect::<Result<_, _>>()?;
        tx.commit()?;
        Ok(rows)
    };
    rows().map_err(|e| {
        if retry_later(&e) {
            Failure::Retry(e)
        } else {
            e.into()
        }
    })
}

fn retry_later(e: &rusqlite::Error) -> bool {
    use rusqlite::ErrorCode::{DatabaseBusy, DatabaseLocked};
    matches!(
        e.sqlite_error(),
        Some(err) if matches!(err.code, DatabaseBusy | DatabaseLocked)
            || err.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK
    )
}

/// `file:<absolute path>?mode=ro` with every byte outside the URI unreserved set and `/`
/// percent-encoded, so `?`, `#` and `%` in a path stay part of the file name.
fn read_only_uri(db: &Path) -> io::Result<String> {
    let path = std::path::absolute(db)?;
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            uri.push(char::from(b));
        } else {
            uri.push('%');
            uri.push_str(&lower_hex(&[b]));
        }
    }
    uri.push_str("?mode=ro");
    Ok(uri)
}

fn read_snapshot(conn: &Connection) -> rusqlite::Result<BTreeMap<String, EventRow>> {
    conn.prepare(&format!("SELECT {ROW_COLUMNS} FROM snapshot"))?
        .query_map([], event_row)?
        .map(|row| row.map(|row| (row.id.clone(), row)))
        .collect()
}

/// The live row as an [`EventRow`], or the first column whose SQLite type does not fit.
fn typed(raw: &[Sql]) -> Result<EventRow, usize> {
    let text = |i: usize| match &raw[i] {
        Sql::Text(t) => Ok(t.clone()),
        _ => Err(i),
    };
    let optional_text = |i: usize| match &raw[i] {
        Sql::Null => Ok(None),
        _ => text(i).map(Some),
    };
    #[allow(
        clippy::cast_precision_loss,
        reason = "SQLite stores an integer in a REAL column only when it is exact as a float"
    )]
    let real = |i: usize| match raw[i] {
        Sql::Real(x) => Ok(x),
        Sql::Integer(n) => Ok(n as f64),
        _ => Err(i),
    };
    Ok(EventRow {
        id: text(0)?,
        packet: match &raw[1] {
            Sql::Blob(b) => b.clone(),
            _ => return Err(1),
        },
        digest: text(2)?,
        status: text(3)?,
        attempts: match raw[4] {
            Sql::Integer(n) => n,
            _ => return Err(4),
        },
        lease: optional_text(5)?,
        lease_until: match raw[6] {
            Sql::Null => None,
            _ => Some(real(6)?),
        },
        available: real(7)?,
        session_id: optional_text(8)?,
        outcome: optional_text(9)?,
        created: real(10)?,
    })
}

fn wrong_type(raw: &[Sql], column: usize) -> Divergence {
    Divergence {
        check: Check::Row,
        detail: format!("column {} has the wrong SQLite type", COLUMNS[column]),
        id: match &raw[0] {
            Sql::Text(t) => Value::from(t.as_str()),
            _ => Value::Null,
        },
        live: Value::Null,
        rust: Value::Null,
    }
}

fn sha256_ref(bytes: &[u8]) -> Value {
    Value::from(format!("sha256:{}", lower_hex(&Sha256::digest(bytes))))
}

/// `packet_bytes`, `digest`, `mac` and then, when all three held, `publish` into the Rust store
/// at the row's `created`, the time the live inbox accepted it.
fn check_packet(
    store: &mut Inbox,
    row: &EventRow,
    found: &mut Vec<Divergence>,
) -> Result<(), Failure> {
    let id = Value::from(row.id.as_str());
    let mut push = |check, detail: &str, live, rust| {
        found.push(Divergence {
            check,
            detail: detail.to_owned(),
            id: id.clone(),
            live,
            rust,
        });
    };
    let live_bytes = sha256_ref(&row.packet);
    let Ok(packet) = serde_json::from_slice::<Value>(&row.packet) else {
        push(
            Check::PacketBytes,
            "stored packet is not JSON",
            live_bytes,
            Value::Null,
        );
        return Ok(());
    };
    let mut held = true;
    let rule4 = match canonical(&packet) {
        Ok(ours) => {
            if ours != row.packet {
                held = false;
                push(
                    Check::PacketBytes,
                    "canonical(packet) differs from the stored bytes",
                    live_bytes.clone(),
                    sha256_ref(&ours),
                );
            }
            false
        }
        Err(Error::InvalidPacket(Invalid::ExponentFloat)) => true,
        Err(e) => return Err(e.into()),
    };
    if !rule4 {
        let signed = signed_bytes(&packet)?;
        let digest = digest_hex(&signed);
        if digest != row.digest {
            held = false;
            push(
                Check::Digest,
                "digest column differs from the recomputed digest",
                Value::from(row.digest.as_str()),
                Value::from(digest),
            );
        }
        if verify(&store.mac, &signed, &packet["mac"]).is_err() {
            held = false;
            let given = packet["mac"].as_str().map_or(Value::Null, Value::from);
            push(
                Check::Mac,
                "MAC does not verify under the live producer key",
                given,
                Value::Null,
            );
        }
    }
    if !held {
        return Ok(());
    }
    match store.publish(&packet, row.created) {
        Ok(stored_id) => {
            let stored = store.get(&stored_id)?.ok_or(Error::NotFound)?;
            if stored_id != row.id {
                push(
                    Check::Publish,
                    "Rust store keeps the packet under another id",
                    id.clone(),
                    Value::from(stored_id),
                );
            } else if stored.packet != row.packet {
                push(
                    Check::Publish,
                    "Rust store keeps other packet bytes",
                    live_bytes,
                    sha256_ref(&stored.packet),
                );
            } else if stored.digest != row.digest {
                push(
                    Check::Publish,
                    "Rust store keeps another digest",
                    Value::from(row.digest.as_str()),
                    Value::from(stored.digest),
                );
            }
        }
        Err(Error::InvalidPacket(Invalid::ExponentFloat)) if rule4 => {}
        Err(e @ (Error::InvalidPacket(_) | Error::Unauthenticated | Error::Conflict)) => push(
            Check::Publish,
            &format!("Rust store refuses the packet: {e}"),
            live_bytes,
            Value::Null,
        ),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// One `row` divergence per column not valid for C1 `Row`.
fn check_row(row: &EventRow, found: &mut Vec<Divergence>) {
    let number = |x: f64| x.is_finite() && x >= 0.0;
    let invalid = [
        ("id", !valid_id(&row.id), Value::from(row.id.as_str())),
        ("packet", !valid_event(&row.packet), sha256_ref(&row.packet)),
        (
            "digest",
            !lower_hex_of_len(&row.digest, 64),
            Value::from(row.digest.as_str()),
        ),
        (
            "status",
            Status::deserialize(&Value::from(row.status.as_str())).is_err(),
            Value::from(row.status.as_str()),
        ),
        ("attempts", row.attempts < 0, Value::from(row.attempts)),
        (
            "lease",
            row.lease
                .as_deref()
                .is_some_and(|l| !lower_hex_of_len(l, 32)),
            row.lease.as_deref().map_or(Value::Null, Value::from),
        ),
        (
            "lease_until",
            row.lease_until.is_some_and(|x| !x.is_finite()),
            row.lease_until.map_or(Value::Null, Value::from),
        ),
        (
            "available",
            !number(row.available),
            Value::from(row.available),
        ),
        (
            "outcome",
            row.outcome
                .as_deref()
                .is_some_and(|o| RowOutcome::deserialize(&Value::from(o)).is_err()),
            row.outcome.as_deref().map_or(Value::Null, Value::from),
        ),
        ("created", !number(row.created), Value::from(row.created)),
    ];
    for (column, bad, live) in invalid {
        if bad {
            found.push(Divergence {
                check: Check::Row,
                detail: format!("column {column} is not valid for C1 Row"),
                id: Value::from(row.id.as_str()),
                live,
                rust: Value::Null,
            });
        }
    }
}

/// C1 `Event` with the patterns and bounds the generated type does not check.
fn valid_event(packet: &[u8]) -> bool {
    let Ok(event) = serde_json::from_slice::<Event>(packet) else {
        return false;
    };
    event.version == 1
        && valid_id(&event.id)
        && (1..=MAX_PAYLOAD_CHARS).contains(&event.payload.chars().count())
        && event.source_ts >= 0.0
        && lower_hex_of_len(&event.mac, 64)
}

/// `transition` and `immutable` of a row against its snapshot state `old`.
fn check_change(old: &EventRow, new: &EventRow, found: &mut Vec<Divergence>) {
    let id = Value::from(new.id.as_str());
    let mut push = |check, detail: &str, live, rust| {
        found.push(Divergence {
            check,
            detail: detail.to_owned(),
            id: id.clone(),
            live,
            rust,
        });
    };
    let moved = (
        &old.status,
        old.attempts,
        &old.lease,
        old.lease_until,
        old.available,
        &old.session_id,
        &old.outcome,
    ) != (
        &new.status,
        new.attempts,
        &new.lease,
        new.lease_until,
        new.available,
        &new.session_id,
        &new.outcome,
    );
    if moved && !reachable(&old.status, &new.status) {
        push(
            Check::Transition,
            "state change follows no edge of the lease state machine",
            Value::from(new.status.as_str()),
            Value::from(old.status.as_str()),
        );
    }
    if new.packet != old.packet {
        push(
            Check::Immutable,
            "packet changed",
            sha256_ref(&new.packet),
            sha256_ref(&old.packet),
        );
    }
    if new.digest != old.digest {
        push(
            Check::Immutable,
            "digest changed",
            Value::from(new.digest.as_str()),
            Value::from(old.digest.as_str()),
        );
    }
    if new.created.to_bits() != old.created.to_bits() {
        push(
            Check::Immutable,
            "created changed",
            Value::from(new.created),
            Value::from(old.created),
        );
    }
    if new.attempts < old.attempts {
        push(
            Check::Immutable,
            "attempts decreased",
            Value::from(new.attempts),
            Value::from(old.attempts),
        );
    }
}

/// True when `to` is reached from `from` along one or more edges.
fn reachable(from: &str, to: &str) -> bool {
    let mut seen = vec![from];
    let mut next = 0;
    while let Some(&at) = seen.get(next) {
        next += 1;
        for &(a, b) in &EDGES {
            if a == at {
                if b == to {
                    return true;
                }
                if !seen.contains(&b) {
                    seen.push(b);
                }
            }
        }
    }
    false
}
