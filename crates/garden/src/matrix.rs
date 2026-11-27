//! The resident dense matrix of the daemon (ADR-0047 section 3c): the vectors of one model's
//! current versions, and of versions closed after its last sync, as one f32 block in memory.
//! A search at or after that sync scans it in place of decoding vectors from SQLite; the worker
//! is its only writer.

use std::collections::HashSet;
use std::sync::{PoisonError, RwLock};

use crate::embed::dot;
use crate::model::{FactId, UnixTime};
use crate::store::{ResidentVector, Store, StoreError};

/// The matrix, empty until the worker builds it for a model.
#[derive(Debug, Default)]
pub struct Matrix {
    rows: RwLock<Option<Rows>>,
}

#[derive(Debug)]
struct Rows {
    model: String,
    dim: usize,
    /// The time of the last sync; the rows answer a search known at this time or later.
    since: UnixTime,
    /// One entry per row of `values`, its `vector` moved into `values`.
    meta: Vec<ResidentVector>,
    values: Vec<f32>,
}

impl Rows {
    fn push(&mut self, mut row: ResidentVector) -> Result<(), StoreError> {
        if self.meta.is_empty() {
            self.dim = row.vector.len();
        }
        if row.vector.len() != self.dim {
            return Err(StoreError::Corrupt(format!(
                "vector of fact {}: dimension {}, the matrix has {}",
                row.fact.0,
                row.vector.len(),
                self.dim
            )));
        }
        // Copy the vector into the block and free its own buffer: `Vec::append` empties the
        // vector but keeps its allocation, which the row in `meta` would then hold for as long
        // as it lives, doubling what a resident row costs.
        self.values
            .extend_from_slice(&std::mem::take(&mut row.vector));
        self.meta.push(row);
        Ok(())
    }

    /// Removes row `i`; the last row takes its place.
    fn swap_remove(&mut self, i: usize) {
        let last = self.meta.len() - 1;
        if i != last {
            let (head, tail) = self.values.split_at_mut(last * self.dim);
            head[i * self.dim..(i + 1) * self.dim].copy_from_slice(&tail[..self.dim]);
        }
        self.values.truncate(last * self.dim);
        self.meta.swap_remove(i);
    }
}

impl Matrix {
    /// An empty matrix of no model.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the matrix holds the vectors of `model`.
    pub fn is_of(&self, model: &str) -> bool {
        self.read().as_ref().is_some_and(|r| r.model == model)
    }

    /// The model whose vectors the matrix holds; `None` before it was built.
    pub fn model(&self) -> Option<String> {
        self.read().as_ref().map(|r| r.model.clone())
    }

    /// Rows held.
    pub fn len(&self) -> usize {
        self.read().as_ref().map_or(0, |r| r.meta.len())
    }

    /// Whether no row is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Replaces the matrix with the vectors of `model` that [`Store::resident_vectors`] lists
    /// at `now`.
    ///
    /// # Errors
    ///
    /// The store fails or two vectors differ in length; the matrix is left as it was.
    pub fn rebuild(&self, store: &Store, model: &str, now: UnixTime) -> Result<(), StoreError> {
        let mut rows = Rows {
            model: model.to_owned(),
            dim: 0,
            since: now,
            meta: Vec::new(),
            values: Vec::new(),
        };
        for row in store.resident_vectors(model, now, None)? {
            rows.push(row)?;
        }
        *self.rows.write().unwrap_or_else(PoisonError::into_inner) = Some(rows);
        Ok(())
    }

    /// Brings the matrix to the store at `now`: drops the rows [`Store::resident_keys`] no
    /// longer lists and adds the ones it lists anew. Nothing to do before the first
    /// [`Matrix::rebuild`].
    ///
    /// # Errors
    ///
    /// The store fails, and the matrix is left as it was, or a new vector differs in length,
    /// and the matrix is emptied of every model until the next rebuild.
    pub fn sync(&self, store: &Store, now: UnixTime) -> Result<(), StoreError> {
        let (model, held) = match self.read().as_ref() {
            Some(r) => (
                r.model.clone(),
                r.meta.iter().map(|m| m.seq).collect::<HashSet<i64>>(),
            ),
            None => return Ok(()),
        };
        let keys = store.resident_keys(&model, now)?;
        let mut added = Vec::new();
        for &seq in keys.iter().filter(|seq| !held.contains(seq)) {
            added.extend(store.resident_vectors(&model, now, Some(seq))?);
        }
        let keys: HashSet<i64> = keys.into_iter().collect();
        let mut guard = self.rows.write().unwrap_or_else(PoisonError::into_inner);
        let Some(rows) = guard.as_mut() else {
            return Ok(());
        };
        let mut i = 0;
        while i < rows.meta.len() {
            if keys.contains(&rows.meta[i].seq) {
                i += 1;
            } else {
                rows.swap_remove(i);
            }
        }
        for row in added {
            if let Err(e) = rows.push(row) {
                *guard = None;
                return Err(e);
            }
        }
        rows.since = now;
        Ok(())
    }

    /// The `k` rows of `model` most like `q` by dot product that hold at (`valid_at`,
    /// `known_at`), private ones only with `private`, best first and ties by fact id; `None`
    /// when the matrix holds another model or `known_at` precedes its last sync.
    pub fn nearest(
        &self,
        model: &str,
        q: &[f32],
        valid_at: UnixTime,
        known_at: UnixTime,
        private: bool,
        k: usize,
    ) -> Option<Vec<(FactId, f32)>> {
        let guard = self.read();
        let rows = guard
            .as_ref()
            .filter(|r| r.model == model && known_at >= r.since)?;
        let mut scored: Vec<(usize, f32)> = rows
            .meta
            .iter()
            .enumerate()
            .filter(|(_, m)| (private || !m.private) && m.times.holds(valid_at, known_at))
            .map(|(i, _)| (i, dot(q, &rows.values[i * rows.dim..(i + 1) * rows.dim])))
            .collect();
        let id = |i: usize| &rows.meta[i].fact;
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| id(a.0).cmp(id(b.0))));
        scored.truncate(k);
        Some(
            scored
                .into_iter()
                .map(|(i, s)| (id(i).clone(), s))
                .collect(),
        )
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Option<Rows>> {
        self.rows.read().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Times;

    #[test]
    fn a_pushed_row_keeps_no_buffer_of_its_vector() {
        let mut rows = Rows {
            model: "m".to_owned(),
            dim: 0,
            since: 0,
            meta: Vec::new(),
            values: Vec::new(),
        };
        for seq in 0..3 {
            rows.push(ResidentVector {
                seq,
                fact: FactId(format!("f{seq}")),
                times: Times {
                    valid_from: 0,
                    valid_to: None,
                    recorded_at: 0,
                    superseded_at: None,
                    expires_at: None,
                },
                private: false,
                vector: vec![1.0; 384],
            })
            .expect("same dimension");
        }
        assert_eq!(rows.values.len(), 3 * 384);
        assert!(rows.meta.iter().all(|m| m.vector.capacity() == 0));
    }
}
