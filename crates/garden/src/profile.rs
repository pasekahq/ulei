//! The pinned core profile (ADR-0047 section 3a): one entry `<entity name>: <fact text>` per
//! pinned current fact, joined by [`DELIMITER`] within a budget of characters, in one owner-only
//! file that a reader sees whole, old or new.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::model::UnixTime;
use crate::store::{Store, StoreError};

/// `garden.core_char_limit` when unset: the engine's default `user_char_limit`.
pub const DEFAULT_CHAR_LIMIT: usize = 1375;
/// The entry delimiter of the engine's memory files.
pub const DELIMITER: &str = "\n§\n";

/// The file of `garden.core_file`, shared by the request path and the worker.
#[derive(Debug)]
pub struct Profile {
    file: PathBuf,
    char_limit: usize,
    /// The text last written and the entries it left out; the lock also orders the renders of
    /// both store connections.
    written: Mutex<Option<(String, usize)>>,
}

/// Why the profile was not rendered.
#[derive(Debug)]
pub enum RenderError {
    /// The store failed.
    Store(StoreError),
    /// The file or its directory failed.
    Io(io::Error),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => f.write_str(e.class()),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RenderError {}

impl Profile {
    /// The profile rendered to `file` within `char_limit` characters.
    pub fn new(file: PathBuf, char_limit: usize) -> Self {
        Self {
            file,
            char_limit,
            written: Mutex::new(None),
        }
    }

    /// The rendered file.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Writes the pinned versions of `store` that hold at `now` ([`Store::pinned`]) unless they
    /// render as this profile last wrote them. Entries are taken in order while the joined
    /// text has at most `char_limit` characters; the rest are left out with one stderr line
    /// `garden: core profile over its budget, <n> facts left out`. The text goes to a 0600 file
    /// beside the profile, renamed over it; a missing directory is created 0700.
    ///
    /// # Errors
    ///
    /// [`RenderError`]; the next render tries again.
    pub fn render(&self, store: &Store, now: UnixTime) -> Result<(), RenderError> {
        let mut written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
        let entries = store.pinned(now).map_err(RenderError::Store)?;
        let rendered = cut(&entries, self.char_limit);
        if written.as_ref() == Some(&rendered) {
            return Ok(());
        }
        let (text, left_out) = &rendered;
        if *left_out > 0 {
            let _ = writeln!(
                io::stderr(),
                "garden: core profile over its budget, {left_out} facts left out"
            );
        }
        replace(&self.file, text).map_err(RenderError::Io)?;
        *written = Some(rendered);
        Ok(())
    }

    /// [`Profile::render`], a failure written as one stderr line `garden: core profile: <error>`.
    pub fn update(&self, store: &Store, now: UnixTime) {
        if let Err(e) = self.render(store, now) {
            let _ = writeln!(io::stderr(), "garden: core profile: {e}");
        }
    }
}

/// The entries `<name>: <text>` joined by [`DELIMITER`], taken in order while the joined text
/// has at most `limit` characters, and the number left out.
fn cut(entries: &[(String, String)], limit: usize) -> (String, usize) {
    let mut text = String::new();
    let mut chars = 0;
    for (n, (name, fact)) in entries.iter().enumerate() {
        let entry = format!("{name}: {fact}");
        let sep = if n == 0 { 0 } else { DELIMITER.chars().count() };
        let next = chars + sep + entry.chars().count();
        if next > limit {
            return (text, entries.len() - n);
        }
        if n > 0 {
            text.push_str(DELIMITER);
        }
        text.push_str(&entry);
        chars = next;
    }
    (text, 0)
}

/// Replaces `file` with `text` through a temporary file in its directory.
fn replace(file: &Path, text: &str) -> io::Result<()> {
    let name = file
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "core file has no name"))?;
    let dir = match file.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut temp = OsString::from(".");
    temp.push(name);
    temp.push(".tmp");
    let temp = dir.join(temp);
    match fs::remove_file(&temp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    out.write_all(text.as_bytes())?;
    out.sync_all()?;
    fs::rename(&temp, file)
}
