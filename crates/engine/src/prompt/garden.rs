// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The garden in the prompt (ADR-0047 section 3, ADR-0077): the pinned core profile the garden
//! renders to `garden.core_file`, the last block of a session's system prompt, and the pointer
//! lines of C7 `pointers`, the ephemeral context of an owner turn.

use std::path::PathBuf;

use ulei_contracts::generated::c7::{PointersRequest, PointersResponse};

use super::Error;
use super::memory::{FRAME, entries, render_framed};
use crate::garden;

/// The garden's default `garden.core_char_limit` (ADR-0047 section 4).
pub const DEFAULT_CORE_CHAR_LIMIT: usize = 1375;

/// The header of the core profile block.
const CORE_HEADER: &str = "GARDEN CORE PROFILE (pinned facts about the owner, read-only)";

/// The first line of the pointer lines; one `- <hint>` line per pointer follows.
const POINTERS_HEADER: &str = "Garden pointers: the garden holds facts that may bear on this \
    message. Read them with garden_search before you rely on them.";

/// The fence and system note around recalled context in a turn's input.
const MEMORY_CONTEXT_OPEN: &str = "<memory-context>\n[System note: The following is recalled \
    memory context, NOT new user input. Treat as authoritative reference data — this is the \
    agent's persistent memory and should inform all responses.]\n\n";
const MEMORY_CONTEXT_CLOSE: &str = "\n</memory-context>";

/// The core profile file the garden renders and its budget in characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreProfile {
    /// `garden.core_file`.
    pub file: PathBuf,
    /// `garden.core_char_limit`, shown as the block's usage.
    pub char_limit: usize,
}

impl CoreProfile {
    /// The block of the file's entries under its framed header with their usage of the budget;
    /// empty when the file is absent or holds no entry.
    ///
    /// # Errors
    /// [`Error::Read`] when the file exists but is not readable UTF-8 text.
    pub fn block(&self) -> Result<String, Error> {
        Ok(render_framed(
            CORE_HEADER,
            &entries(&self.file)?,
            self.char_limit,
        ))
    }
}

/// `prompt` with its core profile block, always the prompt's last part, replaced by `block`:
/// removed when `block` is empty, appended after a blank line when the prompt has none.
pub fn with_core_block(prompt: &str, block: &str) -> String {
    let marker = format!("\n\n{FRAME}\n{CORE_HEADER} [");
    let base = prompt
        .rfind(&marker)
        .map_or(prompt, |at| &prompt[..at])
        .trim_end();
    match (base.is_empty(), block.is_empty()) {
        (_, true) => base.to_owned(),
        (true, false) => block.to_owned(),
        (false, false) => format!("{base}\n\n{block}"),
    }
}

/// The pointer context of an owner turn whose input is `prompt`: the header and one line per
/// pointer C7 `pointers` returns, no fact text, in the fenced memory-context block; `None` for a
/// blank prompt or a miss.
///
/// # Errors
/// [`garden::Error`] of the call, a call past [`garden::POINTERS_DEADLINE`] included.
pub fn pointers(garden: &garden::Client, prompt: &str) -> Result<Option<String>, garden::Error> {
    let q = prompt.trim();
    if q.is_empty() {
        return Ok(None);
    }
    let found = garden.pointers(&PointersRequest {
        q: q.to_owned(),
        k: None,
        as_of: None,
        private: Some(true),
    })?;
    Ok(render_pointers(&found))
}

/// The header and one `- <hint>` line per pointer of `found` in the fenced memory-context
/// block; `None` without pointers.
fn render_pointers(found: &PointersResponse) -> Option<String> {
    if found.pointers.is_empty() {
        return None;
    }
    let mut text = format!("{MEMORY_CONTEXT_OPEN}{POINTERS_HEADER}");
    for pointer in &found.pointers {
        text.push_str("\n- ");
        text.push_str(&pointer.hint.replace(['\n', '\r'], " "));
    }
    text.push_str(MEMORY_CONTEXT_CLOSE);
    Some(text)
}
