//! `state-db-guard`: the agent never opens or writes the live session database.
//!
//! A sqlite reader of the live file holds locks that break running turns and a writer can
//! corrupt it. A shell command that names the file passes only when every simple command in it
//! is `cp` from it, `ls`, `stat`, `du` or `sha256sum` with plain operands; a sqlite call whose
//! operands the text does not show, or a command the lexer cannot read, is denied.

use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use ulei_contracts::generated::c4g::ToolCall;

use super::{Policy, Verdict};
use crate::tools::{EDIT, SHELL, WRITE};

/// Commands that only copy or inspect a file.
const READ_ONLY: [&str; 5] = ["cp", "ls", "stat", "du", "sha256sum"];
/// Words through which a command gets operands its text does not show.
const FROM_ELSEWHERE: [&str; 6] = ["xargs", "parallel", "-exec", "-execdir", "-ok", "{}"];
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

    /// The single decision for a shell command, in order: the lexer must read it, no sqlite call
    /// may take operands the text does not show, and a command naming the database must consist
    /// only of copy or inspect commands.
    fn check_shell(&self, command: &str) -> Verdict {
        let Some(segments) = lex(command) else {
            return if self.mentions(command) || command.to_lowercase().contains("sqlite") {
                self.deny_shell("the command has an unterminated quote")
            } else {
                Verdict::Allow
            };
        };
        for segment in segments.iter().filter(|s| is_sqlite_call(s)) {
            let hidden = segment
                .iter()
                .filter(|w| !w.redirect)
                .any(|w| w.expands || w.glob || FROM_ELSEWHERE.contains(&basename(&w.text)));
            if hidden {
                return self.deny_shell(&format!(
                    "`{}` runs sqlite on operands an expansion, a glob or another command supplies",
                    render(segment)
                ));
            }
        }
        if segments.iter().flatten().any(|w| self.mentions(&w.text)) {
            for segment in &segments {
                if let Err(why) = self.copy_or_inspect(segment) {
                    return self.deny_shell(&format!(
                        "the command names the database and `{}` {why}",
                        render(segment)
                    ));
                }
            }
        }
        Verdict::Allow
    }

    /// `words` is a copy/inspect command with plain operands that never writes the database.
    fn copy_or_inspect(&self, words: &[Word]) -> Result<(), &'static str> {
        let (redirects, args): (Vec<&Word>, Vec<&Word>) = words.iter().partition(|w| w.redirect);
        let Some((program, operands)) = args.split_first() else {
            return Err("is a bare redirection");
        };
        let program = basename(&program.text);
        if !READ_ONLY.contains(&program) {
            return Err("is not a copy or inspect command");
        }
        if operands.iter().any(|w| w.expands || w.glob) {
            return Err("has an expansion or a glob operand");
        }
        if redirects
            .iter()
            .any(|w| w.expands || self.mentions(&w.text))
        {
            return Err("redirects to or from the database");
        }
        if program == "cp" && operands.iter().any(|w| self.mentions(&w.text)) {
            let plain = operands
                .iter()
                .filter(|w| w.text.starts_with('-'))
                .all(|w| {
                    matches!(w.text.as_str(), "-p" | "-v")
                        || w.text == "--preserve"
                        || w.text.starts_with("--preserve=")
                });
            if !plain {
                return Err("copies with an option other than -p, -v or --preserve");
            }
            let destination = operands.iter().rev().find(|w| !w.text.starts_with('-'));
            if destination.is_none_or(|w| self.mentions(&w.text)) {
                return Err("may copy onto the database");
            }
        }
        Ok(())
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
            '\n' | ';' | '&' | '|' | '(' | ')' | '`' | '$'
                if c != '$' || chars.peek() == Some(&'(') =>
            {
                finish_word(&mut word, &mut segment, &mut redirect);
                redirect = false;
                if matches!(c, '`' | '$') {
                    let text = if c == '$' {
                        chars.next();
                        "$("
                    } else {
                        "`"
                    };
                    // The command substitution is an operand of the command around it.
                    segment.push(Word {
                        text: text.into(),
                        expands: true,
                        ..Word::default()
                    });
                }
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

/// A word runs sqlite: its CLI under any wrapper or a scripting language's sqlite module.
fn is_sqlite_call(words: &[Word]) -> bool {
    words
        .iter()
        .any(|w| !w.redirect && w.text.to_lowercase().contains("sqlite"))
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
