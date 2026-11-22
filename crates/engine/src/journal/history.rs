// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Reads of the visible history: the turn messages of a session, live and archived, that no live
//! forget mark hides, each with the text the history index holds for it.

use rusqlite::{OptionalExtension, Row, params};

use super::search::MessageRole;
use super::{Error, Journal};

/// Joins and filters of a visible history row `m` with its index row `f`.
const VISIBLE: &str = "FROM messages m JOIN messages_fts f ON f.rowid = m.n WHERE m.origin = 'turn' AND NOT EXISTS (SELECT 1 FROM marks k WHERE k.n = m.n AND k.lifted_at IS NULL)";
const ENTRY: &str = "SELECT m.n, f.role, f.content, f.tool_name, f.tool_calls, m.at";

/// A visible message with its index text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its position over the whole journal, the message id the history shapes show.
    pub n: u64,
    /// Its role.
    pub role: MessageRole,
    /// Its text.
    pub content: String,
    /// The names of the calls its tool results answer.
    pub tool_name: Option<String>,
    /// Its tool calls as a JSON array of C4G `tool_use` blocks.
    pub tool_calls: Option<String>,
    /// Unix time the engine wrote it; `None` for a row written before the journal kept it.
    pub at: Option<u64>,
}

/// The visible messages around an anchor, in order, with the counts beside it in the window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Around {
    /// At most `window` messages before the anchor, the anchor, at most `window` after it.
    pub window: Vec<Entry>,
    /// Messages of the window before the anchor.
    pub before: usize,
    /// Messages of the window after the anchor.
    pub after: usize,
}

/// A session with visible messages, as a browse lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Its id.
    pub session: u64,
    /// Its key.
    pub key: String,
    /// The earliest `at` of its visible messages.
    pub started: Option<u64>,
    /// The latest `at` of its visible messages.
    pub last_active: Option<u64>,
    /// Its visible messages.
    pub count: u64,
    /// The text of its first visible user message, if any.
    pub opening: Option<String>,
}

impl Journal {
    /// The key of session `session`; `None` when there is no such session.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn session_key(&self, session: u64) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT key FROM sessions WHERE id = ?1",
                [session.cast_signed()],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The session of message `n` and whether it is live; `None` when no message has `n`.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn placement(&self, n: u64) -> Result<Option<(u64, bool)>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT session, active FROM messages WHERE n = ?1",
                [n.cast_signed()],
                |r| Ok((r.get::<_, i64>(0)?.cast_unsigned(), r.get(1)?)),
            )
            .optional()?)
    }

    /// The visible messages of session `session` in order.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn history(&self, session: u64) -> Result<Vec<Entry>, Error> {
        self.entries(
            &format!("{ENTRY} {VISIBLE} AND m.session = ?1 ORDER BY m.n"),
            params![session.cast_signed()],
        )
    }

    /// The visible messages of session `session` around its visible message `n`, at most
    /// `window` on each side; empty when `n` is not one of them.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn around(&self, session: u64, n: u64, window: usize) -> Result<Around, Error> {
        let (session, n) = (session.cast_signed(), n.cast_signed());
        let anchored = self
            .conn
            .query_row(
                &format!("SELECT 1 {VISIBLE} AND m.session = ?1 AND m.n = ?2"),
                params![session, n],
                |_| Ok(()),
            )
            .optional()?;
        if anchored.is_none() {
            return Ok(Around::default());
        }
        let side = |cmp: &str, order: &str, limit: usize| {
            self.entries(
                &format!(
                    "{ENTRY} {VISIBLE} AND m.session = ?1 AND m.n {cmp} ?2 ORDER BY m.n {order} LIMIT ?3"
                ),
                params![session, n, i64::try_from(limit).unwrap_or(i64::MAX)],
            )
        };
        let mut window_rows = side("<=", "DESC", window.saturating_add(1))?;
        window_rows.reverse();
        let after = side(">", "ASC", window)?;
        let before = window_rows.len() - 1;
        let count = after.len();
        window_rows.extend(after);
        Ok(Around {
            window: window_rows,
            before,
            after: count,
        })
    }

    /// The first `count` visible user and assistant messages of session `session` with text
    /// before message `first`, and the last `count` such messages after message `last`.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn bookends(
        &self,
        session: u64,
        first: u64,
        last: u64,
        count: usize,
    ) -> Result<(Vec<Entry>, Vec<Entry>), Error> {
        let limit = i64::try_from(count).unwrap_or(i64::MAX);
        let side = |cmp: &str, order: &str, bound: u64| {
            self.entries(
                &format!(
                    "{ENTRY} {VISIBLE} AND m.session = ?1 AND m.n {cmp} ?2 AND f.role IN ('user', 'assistant') AND length(f.content) > 0 ORDER BY m.n {order} LIMIT ?3"
                ),
                params![session.cast_signed(), bound.cast_signed(), limit],
            )
        };
        let start = side("<", "ASC", first)?;
        let mut end = side(">", "DESC", last)?;
        end.reverse();
        Ok((start, end))
    }

    /// The earliest `at` of the visible messages of session `session`.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn started(&self, session: u64) -> Result<Option<u64>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT min(m.at) {VISIBLE} AND m.session = ?1"),
                [session.cast_signed()],
                |r| r.get::<_, Option<i64>>(0),
            )?
            .map(i64::cast_unsigned))
    }

    /// At most `limit` sessions other than `except` that have visible messages, the most
    /// recently active first.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn recent(&self, except: u64, limit: usize) -> Result<Vec<Listed>, Error> {
        let sql = format!(
            "SELECT s.id, s.key, min(v.at), max(v.at), count(*), (SELECT f.content {VISIBLE} AND m.session = s.id AND f.role = 'user' ORDER BY m.n LIMIT 1) FROM sessions s JOIN (SELECT m.session, m.at {VISIBLE}) v ON v.session = s.id WHERE s.id != ?1 GROUP BY s.id ORDER BY max(v.at) DESC, min(v.at) DESC, s.id DESC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                except.cast_signed(),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |r| {
                Ok(Listed {
                    session: r.get::<_, i64>(0)?.cast_unsigned(),
                    key: r.get(1)?,
                    started: r.get::<_, Option<i64>>(2)?.map(i64::cast_unsigned),
                    last_active: r.get::<_, Option<i64>>(3)?.map(i64::cast_unsigned),
                    count: r.get::<_, i64>(4)?.cast_unsigned(),
                    opening: r.get(5)?,
                })
            },
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn entries(&self, sql: &str, values: impl rusqlite::Params) -> Result<Vec<Entry>, Error> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(values, entry)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

fn entry(r: &Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        n: r.get::<_, i64>(0)?.cast_unsigned(),
        role: r.get(1)?,
        content: r.get(2)?,
        tool_name: r.get(3)?,
        tool_calls: r.get(4)?,
        at: r.get::<_, Option<i64>>(5)?.map(i64::cast_unsigned),
    })
}
