//! The miss outbox (ADR-0047 section 5): a `search` that finds nothing queues one row per
//! normalised query; the worker asks the second level about each new row and signs every row
//! it does not answer as a C1 `garden_miss` with the producer key, publishing it to the owner
//! inbox and retrying until the inbox stores it.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use sha2::Sha256;

use crate::api::as_f64;
use crate::edge::normalise;
use crate::model::UnixTime;
use crate::secret::looks_secret;
use crate::store::{Store, StoreError};
use crate::wire::c1;

/// A query with a row younger than this many seconds gets no second row.
pub const COALESCE: UnixTime = 86_400;
/// Unpublished rows kept at most; a new row over it drops the oldest.
pub const MAX_UNPUBLISHED: i64 = 1_000;
/// Bytes of the producer key.
pub const KEY_LEN: usize = 32;
/// Deadline of every read and of every write of the connection to the inbox.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// Seconds before the first retry of a failed publish, and of a pass the store failed.
pub const FIRST_RETRY: UnixTime = 60;
const LAST_RETRY: UnixTime = 3_600;
const MAX_ANSWER: u64 = 64 * 1024;
/// Prefix of every input of [`ProducerKey::query_hash`]. A packet's canonical JSON starts
/// with `{`, so no query hash can equal the MAC of a packet under the same key.
const QUERY_TAG: &[u8] = b"garden-miss-query\0";
const SHOWN_CHARS: usize = 200;

/// What a search that found nothing did to the outbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Queued {
    /// The query already has a row younger than [`COALESCE`] or not yet published.
    Repeat,
    /// A new row is queued.
    Added,
    /// A new row is queued and the oldest unpublished row was dropped for it.
    AddedOverCap,
}

/// Queues the miss of `q` at `now` unless its normalised form has a row younger than
/// [`COALESCE`] or one still unpublished. The row is keyed by [`ProducerKey::query_hash`] and
/// holds `q` for the second level; a query that looks secret is kept only as that hash and a
/// payload that does not repeat it. Published rows past [`COALESCE`] are removed.
///
/// # Errors
///
/// SQLite errors.
pub fn queue(
    store: &mut Store,
    key: &ProducerKey,
    q: &str,
    now: UnixTime,
) -> Result<Queued, StoreError> {
    let hash = key.query_hash(&normalise(q));
    let (payload, query) = if looks_secret(q) {
        (
            "The garden has nothing for a query (not shown).".to_owned(),
            None,
        )
    } else {
        let shown: String = q.chars().take(SHOWN_CHARS).collect();
        (format!("The garden has nothing for: {shown}"), Some(q))
    };
    let since = now.saturating_sub(COALESCE);
    store.atomically(|s| {
        let c = s.conn();
        c.execute(
            "DELETE FROM miss_outbox WHERE published_at IS NOT NULL AND missed_at <= ?1",
            [since],
        )?;
        let repeat: Option<i64> = c
            .query_row(
                "SELECT seq FROM miss_outbox
                 WHERE query_hash = ?1 AND (missed_at > ?2 OR published_at IS NULL) LIMIT 1",
                params![hash, since],
                |r| r.get(0),
            )
            .optional()?;
        if repeat.is_some() {
            return Ok(Queued::Repeat);
        }
        c.execute(
            "INSERT INTO miss_outbox (query_hash, payload, missed_at, next_attempt, query)
             VALUES (?1, ?2, ?3, ?3, ?4)",
            params![hash, payload, now, query],
        )?;
        let dropped = c.execute(
            "DELETE FROM miss_outbox WHERE seq IN (
                 SELECT seq FROM miss_outbox WHERE published_at IS NULL ORDER BY seq
                 LIMIT max((SELECT count(*) FROM miss_outbox WHERE published_at IS NULL) - ?1, 0))",
            [MAX_UNPUBLISHED],
        )?;
        Ok(if dropped == 0 {
            Queued::Added
        } else {
            Queued::AddedOverCap
        })
    })
}

/// Rows waiting to be published.
///
/// # Errors
///
/// SQLite errors.
pub fn unpublished(store: &Store) -> Result<u64, StoreError> {
    let n: i64 = store.conn().query_row(
        "SELECT count(*) FROM miss_outbox WHERE published_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    Ok(n.unsigned_abs())
}

/// The earliest time an unpublished row is due, if any waits.
///
/// # Errors
///
/// SQLite errors.
pub fn next_attempt(store: &Store) -> Result<Option<UnixTime>, StoreError> {
    Ok(store.conn().query_row(
        "SELECT MIN(next_attempt) FROM miss_outbox WHERE published_at IS NULL",
        [],
        |r| r.get(0),
    )?)
}

/// A row whose query the second level has not been asked about.
pub(crate) struct Fresh {
    pub(crate) seq: i64,
    pub(crate) query: String,
}

/// The rows that still hold their query, oldest first.
///
/// # Errors
///
/// SQLite errors.
pub(crate) fn fresh(store: &Store) -> Result<Vec<Fresh>, StoreError> {
    Ok(store
        .conn()
        .prepare("SELECT seq, query FROM miss_outbox WHERE query IS NOT NULL ORDER BY seq")?
        .query_map([], |r| {
            Ok(Fresh {
                seq: r.get(0)?,
                query: r.get(1)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// Settles row `seq` after the second level was asked: removed unpublished when it `answered`,
/// else kept for publishing without its query.
///
/// # Errors
///
/// SQLite errors.
pub(crate) fn asked(store: &mut Store, seq: i64, answered: bool) -> Result<(), StoreError> {
    let sql = if answered {
        "DELETE FROM miss_outbox WHERE seq = ?1"
    } else {
        "UPDATE miss_outbox SET query = NULL WHERE seq = ?1"
    };
    store.atomically(|s| {
        s.conn().execute(sql, [seq])?;
        Ok(())
    })
}

/// Publishes the rows due at `now`, oldest first. A row gets its event id at its first attempt
/// and keeps it, so a retry sends the same packet; that attempt drops the row's query. After
/// the first failure the pass stops sending, and every due row it did not publish is retried
/// after 1, 2, 4 and up to 60 minutes counted by its own failed attempts.
///
/// # Errors
///
/// SQLite errors.
pub fn publish_due(store: &mut Store, inbox: &Inbox, now: UnixTime) -> Result<(), StoreError> {
    let due: Vec<Row> = store
        .conn()
        .prepare(
            "SELECT seq, query_hash, payload, missed_at, event_id, attempts FROM miss_outbox
             WHERE published_at IS NULL AND next_attempt <= ?1 ORDER BY seq",
        )?
        .query_map([now], |r| {
            Ok(Row {
                seq: r.get(0)?,
                hash: r.get(1)?,
                payload: r.get(2)?,
                missed_at: r.get(3)?,
                id: r.get(4)?,
                attempts: r.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let mut down = false;
    for row in due {
        if !down {
            let id = if let Some(id) = &row.id {
                id.clone()
            } else {
                let id = format!("garden:miss:{now}:{}", row.hash);
                store.atomically(|s| {
                    s.conn().execute(
                        "UPDATE miss_outbox SET event_id = ?2, query = NULL WHERE seq = ?1",
                        params![row.seq, id],
                    )?;
                    Ok::<_, StoreError>(())
                })?;
                id
            };
            let event = inbox.key.sign(id, row.payload, as_f64(row.missed_at));
            if inbox.publish(&event).is_ok() {
                store.atomically(|s| {
                    s.conn().execute(
                        "UPDATE miss_outbox SET published_at = ?2, next_attempt = NULL
                         WHERE seq = ?1",
                        params![row.seq, now],
                    )?;
                    Ok::<_, StoreError>(())
                })?;
                continue;
            }
            down = true;
        }
        let attempts = row.attempts.saturating_add(1);
        store.atomically(|s| {
            s.conn().execute(
                "UPDATE miss_outbox SET attempts = ?2, next_attempt = ?3 WHERE seq = ?1",
                params![row.seq, attempts, now.saturating_add(retry_after(attempts))],
            )?;
            Ok::<_, StoreError>(())
        })?;
    }
    Ok(())
}

/// Seconds before the next attempt of a row that has failed `attempts` times.
fn retry_after(attempts: i64) -> UnixTime {
    // Six doublings of a minute already pass the last retry.
    let doublings = u32::try_from(attempts.saturating_sub(1).clamp(0, 6)).unwrap_or(6);
    (FIRST_RETRY << doublings).min(LAST_RETRY)
}

struct Row {
    seq: i64,
    hash: String,
    payload: String,
    missed_at: UnixTime,
    id: Option<String>,
    attempts: i64,
}

/// The 32-byte C1 producer key, held as its HMAC-SHA256 state.
#[derive(Clone)]
pub struct ProducerKey(Hmac<Sha256>);

impl fmt::Debug for ProducerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProducerKey(..)")
    }
}

impl ProducerKey {
    /// The key in the file at `path`, which must not be a symlink, must belong to `uid`, be open
    /// to no other user and hold exactly [`KEY_LEN`] bytes, as the inbox requires of its own.
    ///
    /// # Errors
    ///
    /// [`KeyError`].
    pub fn read(path: &Path, uid: u32) -> Result<Self, KeyError> {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(KeyError::Symlink);
        }
        if meta.uid() != uid {
            return Err(KeyError::ForeignOwner(meta.uid()));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(KeyError::OpenMode(meta.mode() & 0o777));
        }
        Self::from_bytes(&fs::read(path)?)
    }

    /// The key of `bytes`, which must be exactly [`KEY_LEN`] long.
    ///
    /// # Errors
    ///
    /// [`KeyError::Length`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, KeyError> {
        if bytes.len() != KEY_LEN {
            return Err(KeyError::Length(bytes.len()));
        }
        Hmac::new_from_slice(bytes)
            .map(Self)
            .map_err(|_| KeyError::Length(bytes.len()))
    }

    /// The outbox key and event id suffix of a normalised query: the lowercase hex
    /// HMAC-SHA256 of a fixed tag followed by the query. Keyed, so a stored row or id does
    /// not let anyone recover a short query by trying candidates.
    pub fn query_hash(&self, normalised: &str) -> String {
        let mut mac = self.0.clone();
        mac.update(QUERY_TAG);
        mac.update(normalised.as_bytes());
        lower_hex(&mac.finalize().into_bytes())
    }

    /// The C1 `garden_miss` packet of these fields, its MAC the lowercase hex HMAC-SHA256 of
    /// the canonical JSON of the six other fields (sorted keys, no spaces, UTF-8).
    fn sign(&self, id: String, payload: String, source_ts: f64) -> c1::Event {
        let mut event = c1::Event {
            version: 1,
            kind: c1::Kind::GardenMiss,
            id,
            payload,
            source_ts,
            trust: c1::EventTrust::UntrustedSourceData,
            mac: String::new(),
        };
        let signed: BTreeMap<&str, Value> = BTreeMap::from([
            ("id", Value::from(event.id.as_str())),
            ("kind", Value::from(event.kind.as_str())),
            ("payload", Value::from(event.payload.as_str())),
            ("source_ts", Value::from(event.source_ts)),
            ("trust", Value::from(event.trust.as_str())),
            ("version", Value::from(event.version)),
        ]);
        let mut mac = self.0.clone();
        mac.update(Value::from_iter(signed).to_string().as_bytes());
        event.mac = lower_hex(&mac.finalize().into_bytes());
        event
    }
}

/// Why the producer key file was refused.
#[derive(Debug)]
pub enum KeyError {
    /// The path is a symlink.
    Symlink,
    /// The file belongs to this uid instead of the daemon's.
    ForeignOwner(u32),
    /// The file has this mode, open to other users.
    OpenMode(u32),
    /// The file holds this many bytes instead of [`KEY_LEN`].
    Length(usize),
    /// The file cannot be read.
    Io(io::Error),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Symlink => f.write_str("must not be a symlink"),
            Self::ForeignOwner(uid) => {
                write!(
                    f,
                    "belongs to uid {uid}, it must belong to the daemon's uid"
                )
            }
            Self::OpenMode(mode) => write!(f, "has mode {mode:04o}, it must be 0600 or stricter"),
            Self::Length(len) => write!(f, "holds {len} bytes, it must hold {KEY_LEN}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for KeyError {}

impl From<io::Error> for KeyError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// The owner inbox at `inbox.socket` and the key the garden signs for it with.
#[derive(Clone, Debug)]
pub struct Inbox {
    socket: PathBuf,
    key: ProducerKey,
}

impl Inbox {
    /// The inbox serving C1 at `socket`, signed for with `key`.
    pub fn new(socket: PathBuf, key: ProducerKey) -> Self {
        Self { socket, key }
    }

    /// `POST /v1/publish` of `event`: one request on a new connection, answered within
    /// [`TIMEOUT`] per read and write; only a `200` naming the event's id is success.
    ///
    /// # Errors
    ///
    /// [`PublishError`].
    pub fn publish(&self, event: &c1::Event) -> Result<(), PublishError> {
        let body = serde_json::to_vec(&c1::PublishRequest {
            packet: event.clone(),
        })
        .map_err(|_| PublishError::Malformed)?;
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        let mut request = format!(
            "POST /v1/publish HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(&body);
        stream.write_all(&request)?;
        let mut answer = Vec::new();
        stream.take(MAX_ANSWER).read_to_end(&mut answer)?;
        let head_end = answer
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or(PublishError::Malformed)?;
        let status = answer
            .strip_prefix(b"HTTP/1.1 ")
            .and_then(|rest| rest.get(..3))
            .and_then(|code| std::str::from_utf8(code).ok())
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or(PublishError::Malformed)?;
        if status != 200 {
            return Err(PublishError::Refused(status));
        }
        let stored: c1::PublishResponse =
            serde_json::from_slice(&answer[head_end + 4..]).map_err(|_| PublishError::Malformed)?;
        if stored.id != event.id {
            return Err(PublishError::Malformed);
        }
        Ok(())
    }
}

/// Why a publish did not store the event.
#[derive(Debug)]
pub enum PublishError {
    /// The inbox could not be reached, or the exchange failed or missed its deadline.
    Unreachable(io::Error),
    /// The inbox answered with this status.
    Refused(u16),
    /// The answer is not a C1 `PublishResponse` for the event.
    Malformed,
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "inbox unreachable: {e}"),
            Self::Refused(status) => write!(f, "inbox answered {status}"),
            Self::Malformed => f.write_str("inbox answer is not a publish response"),
        }
    }
}

impl std::error::Error for PublishError {}

impl From<io::Error> for PublishError {
    fn from(e: io::Error) -> Self {
        Self::Unreachable(e)
    }
}

/// `bytes` as lowercase hex digits.
pub(crate) fn lower_hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}
