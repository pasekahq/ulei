// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The history index of the journal: one `messages_fts` row per message of origin `turn`,
//! rowid its `n`, and [`Journal::search`] over it with the reference's query sanitizer, filters,
//! order and snippet.

use std::fmt::Write as _;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use rusqlite::{ErrorCode, params_from_iter};

use super::{Error, Journal};

/// Longest query the sanitizer reads, in chars; the rest is cut off.
pub const MAX_QUERY_CHARS: usize = 2_048;

/// Characters the FTS5 query grammar rejects outside a quoted phrase.
const SPECIAL: &[char] = &[
    '+', '{', '}', '(', ')', ':', '"', '^', '@', '/', '#', '&', '|', '~', '[', ']', '<', '>', ',',
    ';', '!', '?', '$', '=', '\\', '\'',
];
const OPERATORS: [&str; 3] = ["AND", "OR", "NOT"];

/// Role of a message as the reference stores it: a user message carrying a tool result is a
/// `tool` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageRole {
    /// The owner or an event.
    User,
    /// The model.
    Assistant,
    /// Tool output.
    Tool,
    /// System text.
    System,
}

impl MessageRole {
    /// The stored and reported name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::System => "system",
        }
    }
}

impl FromSql for MessageRole {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "tool" => Ok(Self::Tool),
            "system" => Ok(Self::System),
            other => Err(FromSqlError::Other(
                format!("no message role {other:?}").into(),
            )),
        }
    }
}

/// Order of the hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    /// BM25 relevance only.
    #[default]
    Rank,
    /// Newest `at` first, relevance among equal times.
    Newest,
    /// Oldest `at` first, relevance among equal times.
    Oldest,
}

/// One query of the history index.
#[derive(Debug, Clone, Copy)]
pub struct Search<'a> {
    /// The user's words, sanitized by [`fts5_query`] before they reach `MATCH`.
    pub query: &'a str,
    /// Roles a hit may have; empty takes every role.
    pub roles: &'a [MessageRole],
    /// The only session a hit may come from.
    pub session: Option<u64>,
    /// Earliest `at` of a hit, inclusive; leaves out rows without `at`.
    pub since: Option<i64>,
    /// Latest `at` of a hit, inclusive; leaves out rows without `at`.
    pub until: Option<i64>,
    /// Order of the hits.
    pub sort: Sort,
    /// Most hits returned.
    pub limit: u32,
}

/// A message the history index matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// `id` of its session.
    pub session: u64,
    /// Its position over the whole journal.
    pub n: u64,
    /// Unix time the engine wrote it; `None` for a row written before the journal kept it.
    pub at: Option<u64>,
    /// Its role.
    pub role: MessageRole,
    /// The best column's text around the match, terms between `>>>` and `<<<`.
    pub snippet: String,
}

impl Journal {
    /// The live and archived turn messages matching `search`, at most `search.limit`. A query
    /// the sanitizer empties, or one FTS5 still rejects, finds nothing.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn search(&self, search: &Search<'_>) -> Result<Vec<Hit>, Error> {
        let query = fts5_query(search.query);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = String::from(
            "SELECT m.session, m.n, m.at, messages_fts.role, snippet(messages_fts, -1, '>>>', '<<<', '...', 40) FROM messages_fts JOIN messages m ON m.n = messages_fts.rowid WHERE messages_fts MATCH ? AND (m.active = 1 OR m.compacted = 1)",
        );
        let mut values = vec![rusqlite::types::Value::Text(query)];
        if !search.roles.is_empty() {
            let marks = vec!["?"; search.roles.len()].join(",");
            let _ = write!(sql, " AND messages_fts.role IN ({marks})");
            values.extend(
                search
                    .roles
                    .iter()
                    .map(|r| rusqlite::types::Value::Text(r.as_str().to_owned())),
            );
        }
        for (clause, value) in [
            (" AND m.session = ?", search.session.map(u64::cast_signed)),
            (" AND m.at >= ?", search.since),
            (" AND m.at <= ?", search.until),
        ] {
            if let Some(value) = value {
                sql.push_str(clause);
                values.push(rusqlite::types::Value::Integer(value));
            }
        }
        sql.push_str(match search.sort {
            Sort::Rank => " ORDER BY rank",
            Sort::Newest => " ORDER BY m.at DESC, rank",
            Sort::Oldest => " ORDER BY m.at ASC, rank",
        });
        sql.push_str(" LIMIT ?");
        values.push(rusqlite::types::Value::Integer(search.limit.into()));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), |r| {
            Ok(Hit {
                session: r.get::<_, i64>(0)?.cast_unsigned(),
                n: r.get::<_, i64>(1)?.cast_unsigned(),
                at: r.get::<_, Option<i64>>(2)?.map(i64::cast_unsigned),
                role: r.get(3)?,
                snippet: r.get(4)?,
            })
        })?;
        match rows.collect::<Result<Vec<_>, _>>() {
            Ok(hits) => Ok(hits),
            // FTS5 parses the expression while stepping and reports a rejected one as a plain
            // SQLITE_ERROR; the reference answers it with no hits.
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::Unknown => {
                Ok(Vec::new())
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// `query` made safe for FTS5 `MATCH` as the reference makes it: cut to [`MAX_QUERY_CHARS`],
/// balanced double-quoted phrases kept, every other grammar character and `%` blanked, a
/// leading `*` and dangling `AND`, `OR`, `NOT` dropped, dotted and hyphenated terms quoted.
#[must_use]
pub fn fts5_query(query: &str) -> String {
    let query: Vec<char> = query.chars().take(MAX_QUERY_CHARS).collect();
    let mut phrases: Vec<String> = Vec::new();
    let mut text = String::with_capacity(query.len());
    let mut i = 0;
    while i < query.len() {
        if query[i] != '"' {
            text.push(query[i]);
            i += 1;
            continue;
        }
        match query[i + 1..].iter().position(|&c| c == '"') {
            None => {
                text.push(' ');
                i += 1;
            }
            Some(len) => {
                let end = i + 1 + len;
                phrases.push(query[i..=end].iter().collect());
                let _ = write!(text, "\0Q{}\0", phrases.len() - 1);
                i = end + 1;
            }
        }
    }
    let text: String = text
        .chars()
        .map(|c| {
            if SPECIAL.contains(&c) || c == '%' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut text = quote_compounds(&drop_operators(&drop_stars(&text)));
    for (i, phrase) in phrases.iter().enumerate() {
        text = text.replace(&format!("\0Q{i}\0"), phrase);
    }
    text.trim().to_owned()
}

/// Runs of `*` collapsed to one, then a `*` at the start or after whitespace dropped.
fn drop_stars(text: &str) -> String {
    let mut collapsed: Vec<char> = Vec::with_capacity(text.len());
    for c in text.chars() {
        if !(c == '*' && collapsed.last() == Some(&'*')) {
            collapsed.push(c);
        }
    }
    collapsed
        .iter()
        .enumerate()
        .filter(|&(i, &c)| !(c == '*' && (i == 0 || collapsed[i - 1].is_whitespace())))
        .map(|(_, &c)| c)
        .collect()
}

/// `text` trimmed without one leading operator word and one trailing whitespace-led operator.
fn drop_operators(text: &str) -> String {
    let mut text = text.trim();
    for op in OPERATORS {
        let rest = text
            .get(..op.len())
            .filter(|head| head.eq_ignore_ascii_case(op))
            .map(|_| &text[op.len()..]);
        if let Some(rest) = rest.filter(|rest| !rest.starts_with(word_char)) {
            text = rest.trim();
            break;
        }
    }
    for op in OPERATORS {
        let before = text
            .len()
            .checked_sub(op.len())
            .and_then(|at| Some((text.get(..at)?, text.get(at..)?)))
            .filter(|(_, tail)| tail.eq_ignore_ascii_case(op))
            .map(|(before, _)| before);
        if let Some(before) = before.filter(|before| before.ends_with(char::is_whitespace)) {
            text = before.trim_end();
            break;
        }
    }
    text.to_owned()
}

/// Every word joined to further words by `.`, `_` or `-` wrapped in double quotes, so FTS5
/// reads it as a phrase instead of splitting it into separate terms.
fn quote_compounds(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let word_end = |from: usize| {
        chars[from..]
            .iter()
            .position(|&c| !word_char(c))
            .map_or(chars.len(), |len| from + len)
    };
    let mut out = String::with_capacity(text.len() + 2);
    let mut i = 0;
    while i < chars.len() {
        if !word_char(chars[i]) || (i > 0 && word_char(chars[i - 1])) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut end = word_end(i);
        let mut joined = false;
        while end + 1 < chars.len()
            && matches!(chars[end], '.' | '_' | '-')
            && word_char(chars[end + 1])
        {
            end = word_end(end + 1);
            joined = true;
        }
        let compound: String = chars[i..end].iter().collect();
        if joined {
            let _ = write!(out, "\"{compound}\"");
        } else {
            out.push_str(&compound);
        }
        i = end;
    }
    out
}

/// A character of the reference's `\w`.
fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
