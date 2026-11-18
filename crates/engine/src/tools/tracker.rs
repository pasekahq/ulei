// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! State the file tools of one session share: the consecutive-call counter of `read_file` and
//! `search_files`, and the modification times of the regions `read_file` returned.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

/// Most regions remembered for dedup; the oldest are forgotten first.
const DEDUP_CAP: usize = 1000;

/// A `read_file` or `search_files` call as the consecutive-call counter compares them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Call {
    Read {
        path: String,
        offset: usize,
        limit: usize,
    },
    Search {
        pattern: String,
        target: String,
        path: String,
        file_glob: String,
        limit: usize,
        offset: usize,
    },
}

/// A region of a file: its resolved path, offset and limit.
pub(super) type Region = (PathBuf, usize, usize);

/// The tracker of one session, shared by its tools and by the calls of a parallel batch.
#[derive(Debug, Default, Clone)]
pub(super) struct Shared(Arc<Mutex<Tracker>>);

impl Shared {
    /// The tracker, held until the guard drops; every method leaves it consistent, so a
    /// poisoned lock still holds a valid tracker.
    pub(super) fn lock(&self) -> MutexGuard<'_, Tracker> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Default)]
pub(super) struct Tracker {
    last: Option<Call>,
    consecutive: usize,
    /// Modification time of each region when it was last returned, in insertion order.
    dedup: Vec<(Region, SystemTime)>,
    /// Stub answers given for each region since its last full read, in insertion order.
    dedup_hits: Vec<(Region, usize)>,
}

impl Tracker {
    /// Counts `call`: how many times in a row it has now been made.
    pub(super) fn repeat(&mut self, call: Call) -> usize {
        if self.last.as_ref() == Some(&call) {
            self.consecutive += 1;
        } else {
            self.last = Some(call);
            self.consecutive = 1;
        }
        self.consecutive
    }

    /// Modification time of `region` when it was last returned.
    pub(super) fn stamp(&self, region: &Region) -> Option<SystemTime> {
        self.dedup
            .iter()
            .find(|(r, _)| r == region)
            .map(|(_, t)| *t)
    }

    /// Remembers `region` as returned at modification time `mtime`.
    pub(super) fn record(&mut self, region: Region, mtime: SystemTime) {
        upsert(&mut self.dedup, region, |_| mtime);
    }

    /// Counts one stub answer for `region`: how many it has had since its last full read.
    pub(super) fn stub_hit(&mut self, region: &Region) -> usize {
        upsert(&mut self.dedup_hits, region.clone(), |hits| {
            hits.unwrap_or(0) + 1
        })
    }

    /// `region` was read in full.
    pub(super) fn read_in_full(&mut self, region: &Region) {
        self.dedup_hits.retain(|(r, _)| r != region);
    }

    /// Another tool ran, so the next read or search is not a repeat.
    pub(super) fn other_call(&mut self) {
        self.last = None;
        self.consecutive = 0;
        self.dedup_hits.clear();
    }

    /// `resolved` was written, so every region of it is read again in full.
    pub(super) fn written(&mut self, resolved: &Path) {
        self.dedup.retain(|((path, _, _), _)| path != resolved);
    }
}

/// Sets the value of `key` from its old one, appending a new key and dropping the oldest past
/// [`DEDUP_CAP`]; the new value.
fn upsert<V: Copy>(
    entries: &mut Vec<(Region, V)>,
    key: Region,
    value: impl FnOnce(Option<V>) -> V,
) -> V {
    if let Some((_, v)) = entries.iter_mut().find(|(k, _)| *k == key) {
        *v = value(Some(*v));
        return *v;
    }
    let v = value(None);
    entries.push((key, v));
    if entries.len() > DEDUP_CAP {
        entries.remove(0);
    }
    v
}
