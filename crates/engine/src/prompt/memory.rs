// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The MEMORY and USER blocks of the system prompt: the entries of a memory file under a
//! framed header with the usage of the store's char limit. A store over its limit renders in
//! full at 100%; the limit binds writes, not this snapshot.

use std::io;
use std::path::Path;

use super::Error;
use crate::tools::{py_strip, thousands};

/// The separator of two entries in a memory file.
const ENTRY_DELIMITER: &str = "\n§\n";

const FRAME: &str = "══════════════════════════════════════════════";

/// A memory store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    /// `MEMORY.md`, the agent's notes.
    Memory,
    /// `USER.md`, the profile of the user.
    User,
}

impl Target {
    fn header(self) -> &'static str {
        match self {
            Self::Memory => "MEMORY (your personal notes)",
            Self::User => "USER PROFILE (who the user is)",
        }
    }
}

/// The entries of the memory file at `path` in file order, each stripped, empty and repeated
/// ones dropped; none when the file is absent.
///
/// # Errors
/// [`Error::Read`] when the file exists but is not readable UTF-8 text.
pub(super) fn entries(path: &Path) -> Result<Vec<String>, Error> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Read(path.to_owned(), e)),
    };
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let mut entries: Vec<String> = Vec::new();
    for entry in raw.split(ENTRY_DELIMITER).map(py_strip) {
        if !entry.is_empty() && !entries.iter().any(|e| e == entry) {
            entries.push(entry.to_owned());
        }
    }
    Ok(entries)
}

/// The block of `target` with `entries` against `limit` chars; empty without entries.
pub(super) fn render_block(target: Target, entries: &[String], limit: usize) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let content = entries.join(ENTRY_DELIMITER);
    let current = content.chars().count();
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the float percentage the reference truncates"
    )]
    let pct = if limit > 0 {
        ((current as f64 / limit as f64 * 100.0) as u64).min(100)
    } else {
        0
    };
    format!(
        "{FRAME}\n{} [{pct}% — {}/{} chars]\n{FRAME}\n{content}",
        target.header(),
        thousands(current),
        thousands(limit)
    )
}
