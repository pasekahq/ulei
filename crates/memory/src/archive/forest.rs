//! The forest (ADR-0079): the compressed records the night stage makes from the archive's
//! sessions, in the archive's file beside the items they compress. A record holds the summary of
//! one session, or of one part of a session, of one source, its time span, its producer and links
//! to the archive entries it compresses. A newer record of the same session that links any of the
//! same entries supersedes the older one, which is kept; a forget mark keeps a record out of
//! search until it is lifted. Rows are only added: triggers refuse every update but the lift of a
//! mark and every delete.

use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{
    Archive, Error, MAX_LIST_BODY, check_id, check_non_empty, check_source, check_time, get_u64,
    int, now, query,
};
use crate::wire::c10::{
    ForestForgetRequest, ForestForgetResponse, ForestForgotten, ForestHit, ForestLinked,
    ForestPutRequest, ForestPutResponse, ForestSearchRequest, Producer, PutStatus,
};

/// Most characters of a record's text.
pub const MAX_FOREST_TEXT: usize = 4_000;
/// Most bytes of UTF-8 of a record's session key.
pub const MAX_SESSION_BYTES: usize = 512;
/// Most links of one record.
pub const MAX_LINKS: usize = 1_000;
/// Hits of a forest search without `limit`.
pub const DEFAULT_FOREST_LIMIT: u64 = 10;
/// Most hits a forest search may ask for.
pub const MAX_FOREST_LIMIT: u64 = 20;
/// Most ids of one `forest_forget`.
pub const MAX_FOREST_IDS: usize = 100;
/// Most bytes of a producer's route and of a provenance.
const MAX_LABEL: usize = 128;
const MAX_PROVENANCE: usize = 64;

/// What version 5 adds to version 4: the forest's records, their links to archive entries, the
/// supersession of a record by a newer one of its session, its FTS5 text and its forget marks.
/// `forest.row` aliases the rowid, given once by `AUTOINCREMENT`, so `VACUUM` cannot renumber it
/// under the `forest_fts` rowids.
pub(super) const FOREST: &str = "
CREATE TABLE forest (
  row INTEGER PRIMARY KEY AUTOINCREMENT,
  source TEXT NOT NULL,
  session TEXT NOT NULL,
  text TEXT NOT NULL,
  from_at REAL,
  to_at REAL,
  run TEXT NOT NULL,
  route TEXT NOT NULL,
  put_at REAL NOT NULL);
CREATE INDEX forest_session ON forest (source, session);
CREATE TABLE forest_links (
  record INTEGER NOT NULL REFERENCES forest (row),
  seq INTEGER NOT NULL,
  id TEXT NOT NULL,
  external_id TEXT NOT NULL,
  PRIMARY KEY (record, seq)) WITHOUT ROWID;
CREATE INDEX forest_links_seq ON forest_links (seq);
CREATE TABLE forest_superseded (
  record INTEGER PRIMARY KEY REFERENCES forest (row),
  by_record INTEGER NOT NULL REFERENCES forest (row),
  at REAL NOT NULL);
CREATE VIRTUAL TABLE forest_fts USING fts5(text, tokenize = 'unicode61 remove_diacritics 2');
CREATE TABLE forest_marks (
  row INTEGER PRIMARY KEY,
  record INTEGER NOT NULL REFERENCES forest (row),
  at REAL NOT NULL,
  reason TEXT NOT NULL,
  provenance TEXT NOT NULL,
  lifted_at REAL,
  lifted_provenance TEXT);
CREATE INDEX forest_marks_record ON forest_marks (record);
CREATE TRIGGER forest_immutable BEFORE UPDATE ON forest
  BEGIN SELECT RAISE(ABORT, 'forest records are immutable'); END;
CREATE TRIGGER forest_kept BEFORE DELETE ON forest
  BEGIN SELECT RAISE(ABORT, 'forest records are never removed'); END;
CREATE TRIGGER forest_links_immutable BEFORE UPDATE ON forest_links
  BEGIN SELECT RAISE(ABORT, 'forest records are immutable'); END;
CREATE TRIGGER forest_links_kept BEFORE DELETE ON forest_links
  BEGIN SELECT RAISE(ABORT, 'forest records are never removed'); END;
CREATE TRIGGER forest_superseded_immutable BEFORE UPDATE ON forest_superseded
  BEGIN SELECT RAISE(ABORT, 'forest records are immutable'); END;
CREATE TRIGGER forest_superseded_kept BEFORE DELETE ON forest_superseded
  BEGIN SELECT RAISE(ABORT, 'forest records are never removed'); END;
CREATE TRIGGER forest_marks_kept BEFORE DELETE ON forest_marks
  BEGIN SELECT RAISE(ABORT, 'forget marks are never removed'); END;
CREATE TRIGGER forest_marks_lift_once BEFORE UPDATE ON forest_marks
  WHEN old.lifted_at IS NOT NULL OR new.lifted_at IS NULL OR new.lifted_provenance IS NULL
    OR new.row IS NOT old.row OR new.record IS NOT old.record OR new.at IS NOT old.at
    OR new.reason IS NOT old.reason OR new.provenance IS NOT old.provenance
  BEGIN SELECT RAISE(ABORT, 'a forget mark is only lifted, once'); END;
";

/// A record is current while no newer record supersedes it.
const CURRENT: &str = "NOT EXISTS (SELECT 1 FROM forest_superseded s WHERE s.record = f.row)";
/// A record is forgotten while one of its marks is not lifted.
const FORGOTTEN: &str =
    "EXISTS (SELECT 1 FROM forest_marks m WHERE m.record = f.row AND m.lifted_at IS NULL)";

impl Archive {
    /// Stores the forest record of `request`, superseding every current record of its source and
    /// session that links any of its entries; a live mark of a superseded record passes to the
    /// new one. A request equal in text, span and links to the one current record it would
    /// replace stores nothing and answers that record `duplicate`. One transaction.
    ///
    /// # Errors
    /// [`Error::Invalid`] for a field out of its bounds or a link whose `id` is not its entry's
    /// item, [`Error::NotFound`] for a link to no stored entry of the source, [`Error::Io`] for
    /// the clock, or [`Error::Sqlite`].
    pub fn forest_put(&mut self, request: &ForestPutRequest) -> Result<ForestPutResponse, Error> {
        check_record(request)?;
        let put_at = now()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (links, replaced) = resolve(&tx, request)?;
        if let [only] = replaced.iter().copied().collect::<Vec<_>>()[..]
            && same_record(&tx, only, request)?
        {
            return Ok(ForestPutResponse {
                id: to_id(only)?,
                status: PutStatus::Duplicate,
                replaced: None,
            });
        }
        tx.execute(
            "INSERT INTO forest (source, session, text, from_at, to_at, run, route, put_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &request.source,
                &request.session,
                &request.text,
                request.from,
                request.to,
                &request.producer.run,
                &request.producer.route,
                put_at
            ],
        )?;
        let row = tx.last_insert_rowid();
        {
            let mut link = tx.prepare(
                "INSERT INTO forest_links (record, seq, id, external_id) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (seq, id, external_id) in &links {
                link.execute(params![row, int(*seq)?, id, external_id])?;
            }
        }
        tx.execute(
            "INSERT INTO forest_fts (rowid, text) VALUES (?1, ?2)",
            params![row, &request.text],
        )?;
        supersede(&tx, &replaced, row, put_at)?;
        tx.commit()?;
        Ok(ForestPutResponse {
            id: to_id(row)?,
            status: PutStatus::Stored,
            replaced: if replaced.is_empty() {
                None
            } else {
                Some(replaced.into_iter().map(to_id).collect::<Result<_, _>>()?)
            },
        })
    }

    /// The current records that are not forgotten and match `request.q`, best first, filtered by
    /// source and by a span that reaches into `since` to `until`; each with its links to the
    /// archive entries still stored. The answer ends before a hit that would take its serialized
    /// body past [`MAX_LIST_BODY`] bytes, but holds one when one matches.
    ///
    /// # Errors
    /// [`Error::Invalid`], [`Error::Json`] or [`Error::Sqlite`].
    pub fn forest_search(&self, request: &ForestSearchRequest) -> Result<Vec<ForestHit>, Error> {
        check_non_empty("q", &request.q)?;
        if let Some(source) = &request.source {
            check_source(source)?;
        }
        check_time("since", request.since)?;
        check_time("until", request.until)?;
        let limit = request.limit.unwrap_or(DEFAULT_FOREST_LIMIT);
        if !(1..=MAX_FOREST_LIMIT).contains(&limit) {
            return Err(Error::Invalid(format!(
                "limit must be 1 to {MAX_FOREST_LIMIT}"
            )));
        }
        let expr = query::fts5_query(&request.q);
        if expr.is_empty() {
            return Ok(Vec::new());
        }
        let mut st = self.conn.prepare(&format!(
            "SELECT f.row, f.source, f.session, f.text, \
                    snippet(forest_fts, 0, '>>>', '<<<', '...', 40), \
                    f.from_at, f.to_at, f.put_at, f.run, f.route \
             FROM forest_fts JOIN forest f ON f.row = forest_fts.rowid \
             WHERE forest_fts MATCH ?1 AND (?2 IS NULL OR f.source = ?2) \
               AND (?3 IS NULL OR f.to_at >= ?3) AND (?4 IS NULL OR f.from_at <= ?4) \
               AND {CURRENT} AND NOT {FORGOTTEN} \
             ORDER BY forest_fts.rank, f.row LIMIT ?5"
        ))?;
        let rows = st.query_map(
            params![
                expr,
                request.source.as_deref(),
                request.since,
                request.until,
                int(limit)?
            ],
            |r| {
                Ok(ForestHit {
                    id: get_u64(r, 0)?,
                    source: r.get(1)?,
                    session: r.get(2)?,
                    text: r.get(3)?,
                    snippet: r.get(4)?,
                    from: r.get(5)?,
                    to: r.get(6)?,
                    put_at: r.get(7)?,
                    producer: Producer {
                        run: r.get(8)?,
                        route: r.get(9)?,
                    },
                    links: Vec::new(),
                })
            },
        )?;
        let found = match rows.collect::<Result<Vec<_>, _>>() {
            Ok(found) => found,
            // As `search`: FTS5 reports a rejected expression while stepping.
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::Unknown => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };
        let mut links = self.conn.prepare(
            "SELECT l.seq, l.id, l.external_id FROM forest_links l \
             WHERE l.record = ?1 AND EXISTS (SELECT 1 FROM entry_seq s WHERE s.seq = l.seq) \
             ORDER BY l.seq",
        )?;
        let mut hits = Vec::with_capacity(found.len());
        // `{"hits":[]}`, then each hit and a comma before every hit but the first.
        let mut body = 11;
        for mut hit in found {
            hit.links = links
                .query_map([int(hit.id)?], |r| {
                    Ok(ForestLinked {
                        seq: get_u64(r, 0)?,
                        id: r.get(1)?,
                        external_id: r.get(2)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
            let grown = body + serde_json::to_vec(&hit)?.len() + usize::from(!hits.is_empty());
            if grown > MAX_LIST_BODY && !hits.is_empty() {
                break;
            }
            body = grown;
            hits.push(hit);
        }
        Ok(hits)
    }

    /// Marks the records `request.ids` name, each resolved to its current version, or with
    /// `undo` lifts their live marks, in one transaction; one item per id, in order.
    ///
    /// # Errors
    /// [`Error::Invalid`], [`Error::NotFound`] for an unknown id before any change,
    /// [`Error::Io`] for the clock, or [`Error::Sqlite`].
    pub fn forest_forget(
        &mut self,
        request: &ForestForgetRequest,
    ) -> Result<ForestForgetResponse, Error> {
        if request.ids.is_empty() || request.ids.len() > MAX_FOREST_IDS {
            return Err(Error::Invalid(format!(
                "ids must hold 1 to {MAX_FOREST_IDS} records"
            )));
        }
        check_provenance(&request.provenance)?;
        let undo = request.undo == Some(true);
        let reason = match request.reason.as_deref() {
            Some(reason) if !reason.trim().is_empty() => reason,
            _ if undo => "",
            _ => return Err(Error::Invalid("reason is required without undo".to_owned())),
        };
        let at = now()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut items = Vec::with_capacity(request.ids.len());
        for &id in &request.ids {
            let record = current(&tx, id)?;
            let marks = if undo {
                tx.execute(
                    "UPDATE forest_marks SET lifted_at = ?2, lifted_provenance = ?3 \
                     WHERE record = ?1 AND lifted_at IS NULL",
                    params![record, at, &request.provenance],
                )?
            } else {
                tx.execute(
                    "INSERT INTO forest_marks (record, at, reason, provenance) \
                     SELECT ?1, ?2, ?3, ?4 WHERE NOT EXISTS (SELECT 1 FROM forest_marks \
                     WHERE record = ?1 AND lifted_at IS NULL)",
                    params![record, at, reason, &request.provenance],
                )?
            };
            items.push(ForestForgotten {
                id,
                record: to_id(record)?,
                marks: marks as u64,
            });
        }
        tx.commit()?;
        Ok(ForestForgetResponse { items })
    }

    /// The count of current forest records, forgotten ones included.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn forest_records(&self) -> Result<u64, Error> {
        Ok(self.conn.query_row(
            &format!("SELECT count(*) FROM forest f WHERE {CURRENT}"),
            [],
            |r| get_u64(r, 0),
        )?)
    }
}

/// A link checked against the archive: its `seq`, its item id and the entry's `external_id`.
type Linked<'a> = (u64, &'a str, String);

/// Each link of `request` as its `seq`, item id and the entry's `external_id`, checked against
/// the archive, and the current records of its session that share one of its entries.
fn resolve<'a>(
    tx: &rusqlite::Connection,
    request: &'a ForestPutRequest,
) -> Result<(Vec<Linked<'a>>, BTreeSet<i64>), Error> {
    let mut links: Vec<Linked<'_>> = Vec::with_capacity(request.links.len());
    let mut replaced: BTreeSet<i64> = BTreeSet::new();
    let mut entry = tx.prepare(
        "SELECT e.id, e.external_id FROM entry_seq s \
         JOIN entries e ON e.source = s.source AND e.external_id = s.external_id \
         WHERE s.seq = ?1 AND s.source = ?2",
    )?;
    let mut sharing = tx.prepare(&format!(
        "SELECT f.row FROM forest_links l JOIN forest f ON f.row = l.record \
         WHERE l.seq = ?1 AND f.source = ?2 AND f.session = ?3 AND {CURRENT}"
    ))?;
    for link in &request.links {
        let seq = int(link.seq)?;
        let stored: Option<(String, String)> = entry
            .query_row(params![seq, &request.source], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((id, external_id)) = stored else {
            return Err(Error::NotFound(format!(
                "entry {} of source {}",
                link.seq, request.source
            )));
        };
        if id != link.id {
            return Err(Error::Invalid(format!(
                "link {} names item {}, the entry's item is {id}",
                link.seq, link.id
            )));
        }
        let rows = sharing
            .query_map(params![seq, &request.source, &request.session], |r| {
                r.get::<_, i64>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        replaced.extend(rows);
        links.push((link.seq, link.id.as_str(), external_id));
    }
    Ok((links, replaced))
}

/// Marks each of `replaced` superseded by `row` at `put_at`, and passes the first live forget mark
/// among them to `row`.
fn supersede(
    tx: &rusqlite::Connection,
    replaced: &BTreeSet<i64>,
    row: i64,
    put_at: f64,
) -> Result<(), Error> {
    let mut carried = false;
    for old in replaced {
        tx.execute(
            "INSERT INTO forest_superseded (record, by_record, at) VALUES (?1, ?2, ?3)",
            params![old, row, put_at],
        )?;
        if carried {
            continue;
        }
        let mark: Option<(String, String)> = tx
            .query_row(
                "SELECT reason, provenance FROM forest_marks \
                 WHERE record = ?1 AND lifted_at IS NULL ORDER BY row LIMIT 1",
                [old],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((reason, provenance)) = mark {
            tx.execute(
                "INSERT INTO forest_marks (record, at, reason, provenance) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![row, put_at, reason, provenance],
            )?;
            carried = true;
        }
    }
    Ok(())
}

/// The current version of record `id`: itself, or the record that supersedes it, followed to the
/// end.
fn current(conn: &rusqlite::Connection, id: u64) -> Result<i64, Error> {
    let mut row = int(id)?;
    let known: Option<i64> = conn
        .query_row("SELECT row FROM forest WHERE row = ?1", [row], |r| r.get(0))
        .optional()?;
    if known.is_none() {
        return Err(Error::NotFound(format!("forest record {id}")));
    }
    // A record is superseded only by a later one, so the walk ends.
    while let Some(by) = conn
        .query_row(
            "SELECT by_record FROM forest_superseded WHERE record = ?1",
            [row],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        row = by;
    }
    Ok(row)
}

/// Whether record `row` has the text, span and link seqs of `request`.
fn same_record(
    conn: &rusqlite::Connection,
    row: i64,
    request: &ForestPutRequest,
) -> Result<bool, Error> {
    let (text, from, to): (String, Option<f64>, Option<f64>) = conn.query_row(
        "SELECT text, from_at, to_at FROM forest WHERE row = ?1",
        [row],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if text != request.text || from != request.from || to != request.to {
        return Ok(false);
    }
    let mut st = conn.prepare("SELECT seq FROM forest_links WHERE record = ?1 ORDER BY seq")?;
    let stored = st
        .query_map([row], |r| get_u64(r, 0))?
        .collect::<Result<Vec<_>, _>>()?;
    let wanted: BTreeSet<u64> = request.links.iter().map(|l| l.seq).collect();
    Ok(stored.into_iter().eq(wanted))
}

fn to_id(row: i64) -> Result<u64, Error> {
    u64::try_from(row).map_err(|_| Error::Corrupt(format!("forest row number {row}")))
}

fn check_record(request: &ForestPutRequest) -> Result<(), Error> {
    check_source(&request.source)?;
    if request.session.is_empty() || request.session.len() > MAX_SESSION_BYTES {
        return Err(Error::Invalid(format!(
            "session must be 1 to {MAX_SESSION_BYTES} bytes of UTF-8"
        )));
    }
    if request.text.trim().is_empty() || request.text.chars().count() > MAX_FOREST_TEXT {
        return Err(Error::Invalid(format!(
            "text must be 1 to {MAX_FOREST_TEXT} characters, not only white space"
        )));
    }
    check_time("from", request.from)?;
    check_time("to", request.to)?;
    match (request.from, request.to) {
        (None, None) => {}
        (Some(from), Some(to)) if from <= to => {}
        _ => {
            return Err(Error::Invalid(
                "from and to come together, from not after to".to_owned(),
            ));
        }
    }
    if request.links.is_empty() || request.links.len() > MAX_LINKS {
        return Err(Error::Invalid(format!(
            "links must hold 1 to {MAX_LINKS} entries"
        )));
    }
    let mut seen = BTreeSet::new();
    for link in &request.links {
        check_id(&link.id)?;
        if link.seq == 0 || !seen.insert(link.seq) {
            return Err(Error::Invalid(format!(
                "link seq {} is zero or given twice",
                link.seq
            )));
        }
    }
    let run = &request.producer.run;
    let mut parts = run.split('-');
    let numbers = parts.clone().count();
    let run_ok = run.len() <= MAX_PROVENANCE
        && (2..=3).contains(&numbers)
        && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !run_ok {
        return Err(Error::Invalid(
            "producer.run must be <start>-<pid> or <start>-<pid>-<n>".to_owned(),
        ));
    }
    let route = &request.producer.route;
    if route.is_empty() || route.len() > MAX_LABEL {
        return Err(Error::Invalid(format!(
            "producer.route must be 1 to {MAX_LABEL} bytes"
        )));
    }
    Ok(())
}

/// `s<session>-t<turn>` with an optional `-n<n>`, at most 64 bytes.
fn check_provenance(provenance: &str) -> Result<(), Error> {
    let digits = |s: Option<&str>, prefix: char| {
        s.and_then(|s| s.strip_prefix(prefix))
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    };
    let mut parts = provenance.split('-');
    let ok = provenance.len() <= MAX_PROVENANCE
        && digits(parts.next(), 's')
        && digits(parts.next(), 't')
        && parts.next().is_none_or(|n| digits(Some(n), 'n'))
        && parts.next().is_none();
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(
            "provenance must be s<session>-t<turn> or s<session>-t<turn>-n<n>".to_owned(),
        ))
    }
}
