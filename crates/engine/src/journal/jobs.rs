//! The background jobs of owner turns (C8 `Job`, ADR-0102): a row per offloaded tool call from
//! its offload until the owner inbox confirmed its result; a row without an outcome runs.

use std::time::Duration;

use rusqlite::{OptionalExtension, Row, params};
use ulei_contracts::generated::c8::{Job, JobOutcome};

use super::{Error, Journal, waiting};

/// The table, created with the schema and by the migration to version 10.
pub(super) const SCHEMA: &str = "CREATE TABLE jobs (
  n INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, tool TEXT NOT NULL, started REAL NOT NULL,
  ended REAL, outcome TEXT CHECK (outcome IN ('ok', 'error', 'lost')), output TEXT);";

/// The output a job that was running when the engine started ends with.
pub const LOST_OUTPUT: &str =
    "The engine stopped while this job ran; its work was stopped with the engine.";

impl Journal {
    /// Commits the running job `id` of `tool` that started at Unix time `started`. Waits at
    /// most `within` for a lock another connection holds.
    ///
    /// # Errors
    /// [`Error::Sqlite`], a duplicate id included.
    pub fn start_job(
        &mut self,
        id: &str,
        tool: &str,
        started: f64,
        within: Duration,
    ) -> Result<(), Error> {
        waiting(&self.conn, within, |conn| {
            conn.execute(
                "INSERT INTO jobs (id, tool, started) VALUES (?1, ?2, ?3)",
                params![id, tool, started],
            )?;
            Ok(())
        })
    }

    /// Commits the end of running job `id` at `ended` with `outcome` and `output`; false when no
    /// running job has the id.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn end_job(
        &mut self,
        id: &str,
        ended: f64,
        outcome: JobOutcome,
        output: &str,
    ) -> Result<bool, Error> {
        let updated = self.conn.execute(
            "UPDATE jobs SET ended=?2, outcome=?3, output=?4 WHERE id=?1 AND outcome IS NULL",
            params![id, ended, outcome.as_str(), output],
        )?;
        Ok(updated == 1)
    }

    /// Ends every running job `lost` at `ended` with [`LOST_OUTPUT`]: none runs once the engine
    /// starts. The number ended.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn lose_jobs(&mut self, ended: f64) -> Result<usize, Error> {
        Ok(self.conn.execute(
            "UPDATE jobs SET ended=?1, outcome='lost', output=?2 WHERE outcome IS NULL",
            params![ended, LOST_OUTPUT],
        )?)
    }

    /// At most `limit` ended jobs the inbox has not confirmed, oldest first. Waits at most
    /// `within` for a lock another connection holds.
    ///
    /// # Errors
    /// [`Error::Json`] for a stored outcome that is not a C8 one, or [`Error::Sqlite`].
    pub fn ended_jobs(&self, limit: usize, within: Duration) -> Result<Vec<Job>, Error> {
        waiting(&self.conn, within, |conn| {
            let mut statement = conn.prepare(
                "SELECT id, tool, started, ended, outcome, output FROM jobs \
                 WHERE outcome IS NOT NULL ORDER BY n LIMIT ?1",
            )?;
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let rows = statement.query_map([limit], read_job)?;
            let mut jobs = Vec::new();
            for row in rows {
                let (job, outcome) = row?;
                jobs.push(Job {
                    outcome: outcome
                        .map(|text| serde_json::from_value(text.into()))
                        .transpose()?,
                    ..job
                });
            }
            Ok(jobs)
        })
    }

    /// Whether an ended job waits for the inbox's confirmation.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn has_ended_jobs(&self) -> Result<bool, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM jobs WHERE outcome IS NOT NULL LIMIT 1",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Removes the ended jobs `ids` names, in one transaction; a running or unknown id stays as
    /// it is. The number removed. Waits at most `within` for a lock another connection holds.
    ///
    /// # Errors
    /// [`Error::Sqlite`].
    pub fn confirm_jobs(&mut self, ids: &[String], within: Duration) -> Result<usize, Error> {
        self.conn.busy_timeout(within)?;
        let removed = (|| {
            let tx = self.conn.transaction()?;
            let mut removed = 0;
            {
                let mut delete =
                    tx.prepare("DELETE FROM jobs WHERE id=?1 AND outcome IS NOT NULL")?;
                for id in ids {
                    removed += delete.execute([id])?;
                }
            }
            tx.commit()?;
            Ok(removed)
        })();
        self.conn.busy_timeout(super::BUSY_TIMEOUT)?;
        removed
    }
}

/// The job of a `jobs` row and its stored outcome text.
fn read_job(row: &Row<'_>) -> rusqlite::Result<(Job, Option<String>)> {
    Ok((
        Job {
            id: row.get(0)?,
            tool: row.get(1)?,
            started: row.get(2)?,
            ended: row.get(3)?,
            outcome: None,
            output: row.get(5)?,
        },
        row.get(4)?,
    ))
}
