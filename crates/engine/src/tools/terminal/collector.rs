// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Streamed command output kept as a bounded 40 % head and 60 % tail window, with the full
//! stream teed to a spill file once the window overflows.

use std::collections::VecDeque;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::tools::{char_byte, thousands};

/// Most characters a spill file holds; a marker line follows the last one kept.
pub const SPILL_CAP_CHARS: usize = 5_000_000;
const SPILL_CAP_MARKER: &str = "\n... [spill capped at 5,000,000 chars] ...\n";
/// Spill files older than this are removed when a later call picks its spill path.
const SPILL_MAX_AGE: Duration = Duration::from_secs(7 * 86_400);

/// The retained window, the count of every character seen and the optional spill file.
#[derive(Debug)]
pub(super) struct Collector {
    max_chars: usize,
    head_limit: usize,
    tail_limit: usize,
    head: String,
    head_chars: usize,
    tail: VecDeque<String>,
    tail_chars: usize,
    total_chars: usize,
    spill: Option<Spill>,
}

#[derive(Debug)]
struct Spill {
    path: PathBuf,
    file: Option<File>,
    chars: usize,
    /// The file takes no more text: the cap was reached, a write failed or it was closed.
    capped: bool,
}

impl Collector {
    /// A window of `max_chars` that tees to a file at `spill_path` once it overflows.
    pub(super) fn new(max_chars: usize, spill_path: Option<PathBuf>) -> Self {
        let max_chars = max_chars.max(1);
        let head_limit = max_chars * 2 / 5;
        Self {
            max_chars,
            head_limit,
            tail_limit: max_chars - head_limit,
            head: String::new(),
            head_chars: 0,
            tail: VecDeque::new(),
            tail_chars: 0,
            total_chars: 0,
            spill: spill_path.map(|path| Spill {
                path,
                file: None,
                chars: 0,
                capped: false,
            }),
        }
    }

    /// Characters appended so far.
    pub(super) fn total_chars(&self) -> usize {
        self.total_chars
    }

    pub(super) fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let len = text.chars().count();
        let overflowing = self.total_chars + len > self.max_chars;
        if let Some(spill) = &mut self.spill
            && (spill.file.is_some() || overflowing)
        {
            let backlog = || {
                let mut all = self.head.clone();
                all.extend(self.tail.iter().map(String::as_str));
                all
            };
            spill.write(text, len, backlog);
        }
        self.total_chars += len;

        let mut start = 0;
        let mut taken = 0;
        if self.head_chars < self.head_limit {
            taken = (self.head_limit - self.head_chars).min(len);
            start = char_byte(text, taken);
            self.head.push_str(&text[..start]);
            self.head_chars += taken;
        }
        let remaining = len - taken;
        if remaining == 0 || self.tail_limit == 0 {
            return;
        }
        if remaining >= self.tail_limit {
            self.tail.clear();
            self.tail
                .push_back(text[char_byte(text, len - self.tail_limit)..].to_owned());
            self.tail_chars = self.tail_limit;
            return;
        }
        self.tail.push_back(text[start..].to_owned());
        self.tail_chars += remaining;
        while self.tail_chars > self.tail_limit {
            let excess = self.tail_chars - self.tail_limit;
            let Some(first) = self.tail.front_mut() else {
                break;
            };
            let first_chars = first.chars().count();
            if first_chars <= excess {
                self.tail.pop_front();
                self.tail_chars -= first_chars;
            } else {
                first.drain(..char_byte(first, excess));
                self.tail_chars -= excess;
            }
        }
    }

    /// The window within `max_chars` characters, ending with `suffix`; an overflowed window
    /// holds the omission notice between head and tail.
    pub(super) fn render(&self, suffix: &str) -> String {
        let suffix_chars = suffix.chars().count();
        if suffix_chars >= self.max_chars {
            return suffix[char_byte(suffix, suffix_chars - self.max_chars)..].to_owned();
        }
        let mut tail = String::new();
        tail.extend(self.tail.iter().map(String::as_str));
        let available = self.max_chars - suffix_chars;
        if self.total_chars <= available {
            return format!("{}{tail}{suffix}", self.head);
        }

        let mut notice = String::new();
        for _ in 0..4 {
            let budget = available.saturating_sub(notice.chars().count());
            let omitted = self.total_chars.saturating_sub(budget);
            let updated = format!(
                "\n\n... [OUTPUT TRUNCATED - {} chars omitted out of {} total] ...\n\n",
                thousands(omitted),
                thousands(self.total_chars)
            );
            if updated == notice {
                break;
            }
            notice = updated;
        }
        let budget = available.saturating_sub(notice.chars().count());
        let head_chars = budget * 2 / 5;
        let tail_chars = budget - head_chars;
        let tail_len = tail.chars().count();
        let rendered_tail = &tail[char_byte(&tail, tail_len.saturating_sub(tail_chars))..];
        let head = &self.head[..char_byte(&self.head, head_chars)];
        let notice = &notice[..char_byte(&notice, available)];
        format!("{head}{notice}{rendered_tail}{suffix}")
    }

    /// Closes the spill file; its path when it was created.
    pub(super) fn close_spill(&mut self) -> Option<PathBuf> {
        let spill = self.spill.as_mut()?;
        spill.capped = true;
        spill.file.take().map(|_| spill.path.clone())
    }
}

impl Spill {
    /// Tees `text`, opening the file on first use with everything retained before it.
    fn write(&mut self, text: &str, len: usize, backlog: impl FnOnce() -> String) {
        if self.capped {
            return;
        }
        if self.try_write(text, len, backlog).is_err() {
            // A full or broken disk must not fail the command; the file keeps what it got.
            self.capped = true;
        }
    }

    fn try_write(
        &mut self,
        text: &str,
        len: usize,
        backlog: impl FnOnce() -> String,
    ) -> io::Result<()> {
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                ensure_private_dir(dir)?;
            }
            let file = self.file.insert(create_private(&self.path)?);
            let backlog = backlog();
            file.write_all(backlog.as_bytes())?;
            self.chars = backlog.chars().count();
        }
        let Some(file) = &mut self.file else {
            return Ok(());
        };
        let budget = SPILL_CAP_CHARS.saturating_sub(self.chars);
        if len > budget {
            file.write_all(&text.as_bytes()[..char_byte(text, budget)])?;
            file.write_all(SPILL_CAP_MARKER.as_bytes())?;
            self.capped = true;
        } else {
            file.write_all(text.as_bytes())?;
        }
        self.chars += len;
        Ok(())
    }
}

/// Path of the spill file of the command `child` started now, after removing spill files of
/// `dir` older than seven days.
pub(super) fn spill_path(dir: &Path, child: u32) -> PathBuf {
    let now = SystemTime::now();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|modified| {
                    now.duration_since(modified)
                        .is_ok_and(|age| age > SPILL_MAX_AGE)
                });
            if name.starts_with("out-") && name.ends_with(".log") && stale {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    dir.join(format!(
        "out-{secs}-{}-{:x}.log",
        std::process::id(),
        child & 0xffff
    ))
}

/// Replaces the file at `path` with `text`, refusing to follow a link planted there.
pub(super) fn rewrite_private(path: &Path, text: &str) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            return Err(io::Error::other(format!(
                "refusing to overwrite a directory: {}",
                path.display()
            )));
        }
        Ok(_) => fs::remove_file(path)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    create_private(path)?.write_all(text.as_bytes())
}

/// A new file of mode 0600; an existing path, a dangling link included, is an error.
fn create_private(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// `dir` and its parents created, `dir` a real directory of mode 0700.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "spill dir is not a directory (symlink?): {}",
            dir.display()
        )));
    }
    if meta.permissions().mode() & 0o7777 != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// UTF-8 decoding of a byte stream read in chunks: a sequence split between chunks is joined,
/// and each invalid sequence becomes U+FFFD.
#[derive(Debug, Default)]
pub(super) struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    /// The text `chunk` completes; an incomplete sequence at its end waits for the next one.
    pub(super) fn decode(&mut self, chunk: &[u8]) -> String {
        let mut joined = std::mem::take(&mut self.pending);
        joined.extend_from_slice(chunk);
        let mut out = String::with_capacity(joined.len());
        let mut rest = joined.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    return out;
                }
                Err(e) => {
                    let (valid, tail) = rest.split_at(e.valid_up_to());
                    out.push_str(&String::from_utf8_lossy(valid));
                    let Some(bad) = e.error_len() else {
                        self.pending = tail.to_vec();
                        return out;
                    };
                    out.push(char::REPLACEMENT_CHARACTER);
                    rest = &tail[bad..];
                }
            }
        }
    }

    /// U+FFFD for a sequence the stream ended inside.
    pub(super) fn finish(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned()
    }
}
