//! The directory walk of `search_files`: depth first in name order, bounded in directory
//! entries read and by the interrupt handle; skips hidden entries and never enters a symlink.

use std::fs::{self, FileType};
use std::io;
use std::path::{Path, PathBuf};

use super::Interrupt;

/// Most directory entries one `search_files` call reads before it stops.
pub const MAX_SCAN_ENTRIES: usize = 20_000;

/// File type of a walked entry, symlinks not followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

impl From<FileType> for Kind {
    fn from(t: FileType) -> Self {
        if t.is_symlink() {
            Self::Symlink
        } else if t.is_dir() {
            Self::Dir
        } else if t.is_file() {
            Self::File
        } else {
            Self::Other
        }
    }
}

/// What the walk does after a visit.
pub(super) enum Step {
    Continue,
    /// Do not enter this directory.
    Prune,
    Stop,
}

/// Why the walk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum End {
    Complete,
    /// A visit returned [`Step::Stop`].
    Stopped,
    ScanLimit,
    Interrupted,
}

/// Paths that could not be read; the walk goes on without them.
#[derive(Debug, Default)]
pub(super) struct Unreadable {
    first: Option<String>,
}

impl Unreadable {
    pub(super) fn note(&mut self, rel: &str, error: &io::Error) {
        self.first.get_or_insert_with(|| format!("{rel}: {error}"));
    }

    pub(super) fn absorb(&mut self, other: Self) {
        self.first = self.first.take().or(other.first);
    }

    /// The first unreadable path and its error, if any.
    pub(super) fn first(&self) -> Option<&str> {
        self.first.as_deref()
    }
}

/// How a walk ended and the directories it could not read.
pub(super) struct Walked {
    pub(super) end: End,
    pub(super) unreadable: Unreadable,
}

/// Visits every entry under the directory `root` with its path components relative to `root`.
///
/// # Errors
/// The text of the failure when `root` itself cannot be read.
pub(super) fn walk(
    root: &Path,
    interrupt: &Interrupt,
    visit: impl FnMut(&[String], &Path, Kind) -> Step,
) -> Result<Walked, String> {
    let mut unreadable = Unreadable::default();
    let end = walk_entries(root, interrupt, &mut unreadable, visit)?;
    Ok(Walked { end, unreadable })
}

fn walk_entries(
    root: &Path,
    interrupt: &Interrupt,
    unreadable: &mut Unreadable,
    mut visit: impl FnMut(&[String], &Path, Kind) -> Step,
) -> Result<End, String> {
    let mut budget = MAX_SCAN_ENTRIES;
    let mut stack = match read_sorted(root, &mut budget) {
        Ok(Some(entries)) => vec![entries.into_iter()],
        Ok(None) => return Ok(End::ScanLimit),
        Err(e) => return Err(format!("cannot read {}: {e}", root.display())),
    };
    let mut comps: Vec<String> = Vec::new();
    while let Some(top) = stack.last_mut() {
        let Some((name, path, kind)) = top.next() else {
            stack.pop();
            comps.pop();
            continue;
        };
        if interrupt.is_interrupted() {
            return Ok(End::Interrupted);
        }
        if name.starts_with('.') {
            continue;
        }
        comps.push(name);
        match visit(&comps, &path, kind) {
            Step::Stop => return Ok(End::Stopped),
            Step::Continue if kind == Kind::Dir => match read_sorted(&path, &mut budget) {
                Ok(Some(entries)) => {
                    stack.push(entries.into_iter());
                    continue;
                }
                Ok(None) => return Ok(End::ScanLimit),
                Err(e) => unreadable.note(&comps.join("/"), &e),
            },
            Step::Continue | Step::Prune => {}
        }
        comps.pop();
    }
    Ok(End::Complete)
}

/// Entries of `dir` sorted by name; `None` when they exceed what is left of `budget`.
fn read_sorted(dir: &Path, budget: &mut usize) -> io::Result<Option<Vec<(String, PathBuf, Kind)>>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let Some(left) = budget.checked_sub(1) else {
            return Ok(None);
        };
        *budget = left;
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        entries.push((name, entry.path(), entry.file_type()?.into()));
    }
    crate::sort::sort_by(&mut entries, |a, b| a.0.cmp(&b.0));
    Ok(Some(entries))
}
