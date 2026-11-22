//! The raw dialogue the archive pulls through C6 `raw_sessions` (ADR-0051 section 3): every
//! message of origin `turn`, live, archived and forgotten alike, in `n` order, with the text and
//! role its history index row holds.

use std::time::Duration;

use rusqlite::params;

use super::search::MessageRole;
use super::{Error, Journal, waiting};

const RAW: &str = "SELECT m.n, m.session, s.key, m.seq, m.at, f.role, f.content, m.message FROM messages m JOIN sessions s ON s.id = m.session JOIN messages_fts f ON f.rowid = m.n WHERE m.origin = 'turn' AND m.n > ?1 AND (?2 IS NULL OR m.at >= ?2) AND (?3 IS NULL OR m.at <= ?3) ORDER BY m.n LIMIT ?4";

/// One message of the raw dialogue.
#[derive(Debug, Clone, PartialEq)]
pub struct Raw {
    /// Its position over the whole journal.
    pub n: u64,
    /// `id` of its session.
    pub session: u64,
    /// Key of its session.
    pub key: String,
    /// Its position in the session.
    pub seq: u64,
    /// Unix time the engine wrote it; `None` for a row written before the journal kept it.
    pub at: Option<u64>,
    /// Its role in the history index.
    pub role: MessageRole,
    /// Its text in the history index.
    pub content: String,
    /// The stored C4G message as written.
    pub message: String,
}

/// Bounds on `at` of the raw messages read, both inclusive; a bound excludes a row without `at`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Window {
    /// Earliest `at`.
    pub since: Option<f64>,
    /// Latest `at`.
    pub until: Option<f64>,
}

impl Journal {
    /// Hands `take` the raw messages after `n` `after` within `window`, at most `limit`, in `n`
    /// order, while it returns true. Waits at most `within` for a lock another connection holds.
    ///
    /// # Errors
    /// [`Error::Sqlite`], `SQLITE_BUSY` when the lock outlasts `within`.
    pub fn raw(
        &self,
        after: u64,
        window: Window,
        limit: usize,
        within: Duration,
        mut take: impl FnMut(Raw) -> bool,
    ) -> Result<(), Error> {
        waiting(&self.conn, within, |conn| {
            let mut stmt = conn.prepare(RAW)?;
            let mut rows = stmt.query(params![
                i64::try_from(after).unwrap_or(i64::MAX),
                window.since,
                window.until,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ])?;
            while let Some(r) = rows.next()? {
                let raw = Raw {
                    n: r.get::<_, i64>(0)?.cast_unsigned(),
                    session: r.get::<_, i64>(1)?.cast_unsigned(),
                    key: r.get(2)?,
                    seq: r.get::<_, i64>(3)?.cast_unsigned(),
                    at: r.get::<_, Option<i64>>(4)?.map(i64::cast_unsigned),
                    role: r.get(5)?,
                    content: r.get(6)?,
                    message: r.get(7)?,
                };
                if !take(raw) {
                    break;
                }
            }
            Ok(())
        })
    }
}
