//! `state-db-guard`: the agent never opens or writes the live session database.
//!
//! A sqlite reader of the live file holds locks that break running turns and a writer can
//! corrupt it. A shell command that names the file passes only when each simple command naming
//! it is a plain `cp` from it, `ls`, `stat`, `du` or `sha256sum`; anything the lexer cannot
//! read is denied.

use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use ulei_contracts::generated::c4g::ToolCall;

use super::{Policy, Verdict};
use crate::tools::{EDIT, SHELL, WRITE};

/// Commands that only copy or inspect a file.
const READ_ONLY: [&str; 5] = ["cp", "ls", "stat", "du", "sha256sum"];
/// Files SQLite keeps next to a database, as suffixes of its name.
const SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// The protected path is not absolute or has no UTF-8 file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidDbPath(pub PathBuf);

impl fmt::Display for InvalidDbPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "protected database path {} must be absolute with a UTF-8 file name",
            self.0.display()
        )
    }
}

impl std::error::Error for InvalidDbPath {}

/// Denies shell commands that open the protected database through sqlite and `write`/`edit`
/// calls targeting it or its sidecar files.
#[derive(Debug, Clone)]
pub struct StateDbGuard {
    db: PathBuf,
    name: String,
}

impl StateDbGuard {
    /// Guards the database at the absolute path `db`.
    ///
    /// # Errors
    /// [`InvalidDbPath`] when `db` is relative or its file name is missing or not UTF-8.
    pub fn new(db: PathBuf) -> Result<Self, InvalidDbPath> {
        let name = db.file_name().and_then(|n| n.to_str()).map(str::to_owned);
        match name {
            Some(name) if db.is_absolute() => Ok(Self {
                db: normalize(&db),
                name,
            }),
            _ => Err(InvalidDbPath(db)),
        }
    }

    fn check_shell(&self, command: &str) -> Verdict {
        let Some(segments) = lex(command) else {
            return if self.mentions(command) || command.contains("sqlite") {
                self.deny_shell("the command has an unterminated quote")
            } else {
                Verdict::Allow
            };
        };
        for segment in &segments {
            if let Err(why) = self.check_segment(segment) {
                return self.deny_shell(&format!("`{}` {why}", render(segment)));
            }
        }
        Verdict::Allow
    }

    fn check_segment(&self, words: &[Word]) -> Result<(), &'static str> {
        let assignments = words
            .iter()
            .take_while(|w| !w.redirect && is_assignment(&w.text))
            .count();
        let mut operands = words[assignments..].iter().filter(|w| !w.redirect);
        let program = operands.next().map(|w| basename(&w.text));
        let operands: Vec<&Word> = operands.collect();
        if !words.iter().any(|w| self.mentions(&w.text)) {
            let sqlite_cli = program.is_some_and(|p| p.starts_with("sqlite"));
            if sqlite_cli && operands.iter().any(|w| w.expands || w.glob) {
                return Err("may name the database through an expansion or a glob");
            }
            return Ok(());
        }
        let read_only = program.is_some_and(|p| READ_ONLY.contains(&p));
        let mention_is_operand = words
            .iter()
            .enumerate()
            .filter(|(_, w)| self.mentions(&w.text))
            .all(|(i, w)| i > assignments && !w.redirect && !w.expands);
        if !read_only || !mention_is_operand {
            return Err("names the database outside a copy or inspect command");
        }
        if program == Some("cp") && !self.copies_from_db(&operands) {
            return Err("may copy onto the database");
        }
        Ok(())
    }

    /// The last operand of `cp` is its destination unless `-t` names one.
    fn copies_from_db(&self, operands: &[&Word]) -> bool {
        let names_target = operands.iter().any(|w| {
            w.text.starts_with("--target-directory")
                || (w.text.starts_with('-') && !w.text.starts_with("--") && w.text.contains('t'))
        });
        let destination = operands.iter().rev().find(|w| !w.text.starts_with('-'));
        !names_target && destination.is_some_and(|w| !self.mentions(&w.text))
    }

    /// `text` contains the database file name or a sidecar name as a whole path component.
    fn mentions(&self, text: &str) -> bool {
        let name_char = |c: char| c.is_alphanumeric() || matches!(c, '.' | '_' | '-');
        text.match_indices(&self.name).any(|(start, _)| {
            let before = text[..start].chars().next_back();
            let after = &text[start + self.name.len()..];
            let rest = SIDECARS
                .iter()
                .find_map(|s| after.strip_prefix(s))
                .unwrap_or(after);
            !before.is_some_and(name_char)
                && (!after.chars().next().is_some_and(name_char)
                    || !rest.chars().next().is_some_and(name_char))
        })
    }

    fn deny_shell(&self, why: &str) -> Verdict {
        Verdict::Deny(format!(
            "{why}; the live session database {} must not be opened through sqlite. Copy it with \
             `cp` and inspect the copy.",
            self.db.display()
        ))
    }

    /// `target`, lexically or through symlinked directories, is the database or a sidecar.
    fn protects(&self, target: &Path) -> bool {
        let target = normalize(target);
        let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
            return false;
        };
        let names: Vec<OsString> = std::iter::once(self.name.clone())
            .chain(SIDECARS.iter().map(|s| format!("{}{s}", self.name)))
            .map(OsString::from)
            .collect();
        if !names.iter().any(|n| n == name) {
            return false;
        }
        let db_dir = self.db.parent().unwrap_or(&self.db);
        dir == db_dir
            || matches!(
                (dir.canonicalize(), db_dir.canonicalize()),
                (Ok(a), Ok(b)) if a == b
            )
    }
}

impl Policy for StateDbGuard {
    fn name(&self) -> &'static str {
        "state-db-guard"
    }

    fn pre_tool(&mut self, call: &ToolCall, cwd: &Path) -> Verdict {
        let field = |name: &str| call.input.get(name).and_then(Value::as_str);
        match call.name.as_str() {
            SHELL => field("command").map_or(Verdict::Allow, |c| self.check_shell(c)),
            WRITE | EDIT => match field("file_path") {
                Some(path) if self.protects(&cwd.join(path)) => Verdict::Deny(format!(
                    "{path} is the live session database or its sidecar and must not be written"
                )),
                _ => Verdict::Allow,
            },
            _ => Verdict::Allow,
        }
    }
}

/// One shell word with the facts the guard needs about it.
#[derive(Debug, Default)]
struct Word {
    text: String,
    /// Holds a `$` or backtick outside single quotes.
    expands: bool,
    /// Holds an unquoted `*`, `?` or `[`.
    glob: bool,
    /// Target of a redirection.
    redirect: bool,
}

/// Simple commands of `command`, split at operators, newlines, subshells and command
/// substitutions; `None` on an unterminated quote.
fn lex(command: &str) -> Option<Vec<Vec<Word>>> {
    let mut segments = Vec::new();
    let mut segment: Vec<Word> = Vec::new();
    let mut word: Option<Word> = None;
    let mut redirect = false;
    let mut chars = command.chars().peekable();
    let finish_word = |word: &mut Option<Word>, segment: &mut Vec<Word>, redirect: &mut bool| {
        if let Some(mut w) = word.take() {
            w.redirect = std::mem::take(redirect);
            segment.push(w);
        }
    };
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => finish_word(&mut word, &mut segment, &mut redirect),
            '\n' | ';' | '&' | '|' | '(' | ')' | '`' => {
                finish_word(&mut word, &mut segment, &mut redirect);
                redirect = false;
                if !segment.is_empty() {
                    segments.push(std::mem::take(&mut segment));
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                finish_word(&mut word, &mut segment, &mut redirect);
                if !segment.is_empty() {
                    segments.push(std::mem::take(&mut segment));
                }
            }
            '<' | '>' => {
                let fd = word
                    .as_ref()
                    .is_some_and(|w| w.text.chars().all(|c| c.is_ascii_digit()));
                if fd {
                    word = None;
                }
                finish_word(&mut word, &mut segment, &mut redirect);
                while chars
                    .next_if(|c| matches!(c, '<' | '>' | '&' | '|'))
                    .is_some()
                {}
                redirect = true;
            }
            '\'' => {
                let w = word.get_or_insert_with(Word::default);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => w.text.push(c),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(Word::default);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => w.text.push(chars.next()?),
                        c => {
                            w.expands |= matches!(c, '$' | '`');
                            w.text.push(c);
                        }
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') | None => {}
                Some(c) => word.get_or_insert_with(Word::default).text.push(c),
            },
            c => {
                let w = word.get_or_insert_with(Word::default);
                w.expands |= c == '$';
                w.glob |= matches!(c, '*' | '?' | '[');
                w.text.push(c);
            }
        }
    }
    finish_word(&mut word, &mut segment, &mut redirect);
    if !segment.is_empty() {
        segments.push(segment);
    }
    Some(segments)
}

fn render(words: &[Word]) -> String {
    words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_assignment(text: &str) -> bool {
    text.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// `path` without `.` components and with `..` applied lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}
