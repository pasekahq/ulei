//! Forget marks on journal messages. A message is forgotten while one of its marks has no
//! `lifted_at`; [`Journal::search`] leaves it out. Marks are never deleted.

use std::collections::HashSet;

use rusqlite::{Connection, TransactionBehavior, params};
use ulei_contracts::generated::c8::Mark;

use super::{Error, Journal, Turn, exists};

/// What a mark or a lift names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The message of this `n`.
    Message(u64),
    /// Every message the session of this id holds at that moment.
    Session(u64),
}

/// What [`Journal::mark`] did with the messages its targets named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Marking {
    /// Messages that got a new mark.
    pub marked: usize,
    /// Messages left as they were, being forgotten already.
    pub already: usize,
}

impl Journal {
    /// Marks every message `targets` name that is not forgotten yet, in one transaction at Unix
    /// time `at`, with `reason` and the provenance of turn `by`.
    ///
    /// # Errors
    /// [`Error::UnknownMessage`] or [`Error::UnknownSession`] for a target that names nothing,
    /// with nothing marked, or [`Error::Sqlite`].
    pub fn mark(
        &mut self,
        targets: &[Target],
        reason: &str,
        by: Turn,
        at: i64,
    ) -> Result<Marking, Error> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut marking = Marking::default();
        {
            let mut live = tx.prepare("SELECT 1 FROM marks WHERE n=?1 AND lifted_at IS NULL")?;
            let mut insert = tx
                .prepare("INSERT INTO marks (n, at, reason, provenance) VALUES (?1, ?2, ?3, ?4)")?;
            for n in messages(&tx, targets)? {
                if live.exists([n])? {
                    marking.already += 1;
                } else {
                    insert.execute(params![n, at, reason, provenance(by)])?;
                    marking.marked += 1;
                }
            }
        }
        tx.commit()?;
        Ok(marking)
    }

    /// Lifts every live mark of the messages `targets` name, in one transaction at Unix time
    /// `at`, with the provenance of turn `by`; the number of marks lifted.
    ///
    /// # Errors
    /// [`Error::UnknownMessage`] or [`Error::UnknownSession`] for a target that names nothing,
    /// with nothing lifted, or [`Error::Sqlite`].
    pub fn lift(&mut self, targets: &[Target], by: Turn, at: i64) -> Result<usize, Error> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut lifted = 0;
        {
            let mut update = tx.prepare(
                "UPDATE marks SET lifted_at=?2, lifted_provenance=?3 WHERE n=?1 AND lifted_at IS NULL",
            )?;
            for n in messages(&tx, targets)? {
                lifted += update.execute(params![n, at, provenance(by)])?;
            }
        }
        tx.commit()?;
        Ok(lifted)
    }

    /// The `n` of every message `targets` name at this moment, each once, in target order.
    ///
    /// # Errors
    /// [`Error::UnknownMessage`] or [`Error::UnknownSession`] for a target that names nothing,
    /// or [`Error::Sqlite`].
    pub fn messages_of(&self, targets: &[Target]) -> Result<Vec<u64>, Error> {
        let ns = messages(&self.conn, targets)?;
        Ok(ns.into_iter().map(i64::cast_unsigned).collect())
    }

    /// Whether the message at `seq` of session `session` under key `key` is forgotten; a message
    /// the journal does not hold is not.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn forgotten(&self, key: &str, session: u64, seq: u64) -> Result<bool, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT 1 FROM messages m JOIN sessions s ON s.id = m.session JOIN marks k ON k.n = m.n AND k.lifted_at IS NULL WHERE s.key = ?1 AND m.session = ?2 AND m.seq = ?3",
        )?;
        Ok(stmt.exists(params![key, session.cast_signed(), seq.cast_signed()])?)
    }

    /// Every mark of message `n`, lifted or live, in the order they were made.
    ///
    /// # Errors
    /// [`Error::UnknownMessage`], [`Error::Sqlite`].
    pub fn marks(&self, n: u64) -> Result<Vec<Mark>, Error> {
        let n = message(&self.conn, n)?;
        let mut stmt = self.conn.prepare(
            "SELECT id, at, reason, provenance, lifted_at, lifted_provenance FROM marks WHERE n=?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([n], |r| {
            Ok(Mark {
                id: r.get::<_, i64>(0)?.cast_unsigned(),
                n: n.cast_unsigned(),
                at: r.get::<_, i64>(1)?.cast_unsigned(),
                reason: r.get(2)?,
                provenance: r.get(3)?,
                lifted_at: r.get::<_, Option<i64>>(4)?.map(i64::cast_unsigned),
                lifted_provenance: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// `s<session>-t<turn>` of the turn that marks or lifts; the append of that turn adds the
/// `n` of its prompt.
pub fn provenance(by: Turn) -> String {
    format!("s{}-t{}", by.session, by.number)
}

/// Adds `-n<prompt>` to the provenance of every mark and lift turn `turn` made.
pub(super) fn sign(conn: &Connection, turn: Turn, prompt: i64) -> Result<(), Error> {
    let made = provenance(turn);
    let signed = format!("{made}-n{prompt}");
    conn.execute(
        "UPDATE marks SET provenance=?2 WHERE provenance=?1",
        [&made, &signed],
    )?;
    conn.execute(
        "UPDATE marks SET lifted_provenance=?2 WHERE lifted_provenance=?1",
        [&made, &signed],
    )?;
    Ok(())
}

/// The `n` of every message `targets` name, each once.
fn messages(conn: &Connection, targets: &[Target]) -> Result<Vec<i64>, Error> {
    let mut ns = Vec::new();
    let mut session = conn.prepare("SELECT n FROM messages WHERE session=?1 ORDER BY seq")?;
    for target in targets {
        match *target {
            Target::Message(n) => ns.push(message(conn, n)?),
            Target::Session(id) => {
                if !exists(conn, id)? {
                    return Err(Error::UnknownSession(id));
                }
                let rows = session.query_map([id.cast_signed()], |r| r.get::<_, i64>(0))?;
                for n in rows {
                    ns.push(n?);
                }
            }
        }
    }
    let mut seen = HashSet::new();
    ns.retain(|n| seen.insert(*n));
    Ok(ns)
}

/// `n` as stored, when a message has it.
fn message(conn: &Connection, n: u64) -> Result<i64, Error> {
    let stored = n.cast_signed();
    let mut stmt = conn.prepare("SELECT 1 FROM messages WHERE n=?1")?;
    if stmt.exists([stored])? {
        Ok(stored)
    } else {
        Err(Error::UnknownMessage(n))
    }
}
